use std::fmt;
use std::os::raw::{c_int, c_void};

use crate::error::{Error, Result};
use crate::function::Function;
use crate::state::RawLua;
use crate::traits::{FromLuaMulti, IntoLuaMulti};
use crate::types::{LuaType, ValueRef};
use crate::util::{check_stack, error_traceback_thread, pop_error, StackGuard};

#[cfg(not(feature = "luau"))]
use crate::{
    hook::{Debug, HookTriggers},
    types::HookKind,
};

/// Continuation thread status. Can either be Ok, Yielded (rare, but can happen) or Error
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ContinuationStatus {
    Ok,
    Yielded,
    Error,
}

impl ContinuationStatus {
    #[allow(dead_code)]
    pub(crate) fn from_status(status: c_int) -> Self {
        match status {
            ffi::LUA_YIELD => Self::Yielded,
            ffi::LUA_OK => Self::Ok,
            _ => Self::Error,
        }
    }
}

/// Status of a Lua thread (coroutine).
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ThreadStatus {
    /// The thread was just created or is suspended (yielded).
    ///
    /// If a thread is in this state, it can be resumed by calling [`Thread::resume`].
    Resumable,
    /// The thread is currently running.
    Running,
    /// The thread has finished executing.
    Finished,
    /// The thread has raised a Lua error during execution.
    Error,
}

/// Internal representation of a Lua thread status.
///
/// The number in `New` and `Yielded` variants is the number of arguments pushed
/// to the thread stack.
#[derive(Clone, Copy)]
enum ThreadStatusInner {
    New(c_int),
    Running,
    Yielded(c_int),
    Finished,
    Error,
}

/// Handle to an internal Lua thread (coroutine).
#[derive(Clone)]
pub struct Thread(pub(crate) ValueRef, pub(crate) *mut ffi::lua_State);

#[cfg(feature = "send")]
unsafe impl Send for Thread {}
#[cfg(feature = "send")]
unsafe impl Sync for Thread {}

impl Thread {
    #[inline(always)]
    fn state(&self) -> *mut ffi::lua_State {
        self.1
    }

    /// Tries converting whatevers on the thread stack to ``R``.
    pub unsafe fn pop_results<R>(&self) -> Result<R>
    where
        R: FromLuaMulti,
    {
        let lua = self.0.lua.lock();
        let thread_state = self.state();
        let nresults = ffi::lua_gettop(thread_state);
        R::from_specified_stack_multi(nresults, &lua, thread_state)
    }

    /// Resumes execution of this thread.
    ///
    /// Equivalent to [`coroutine.resume`].
    ///
    /// Passes `args` as arguments to the thread. If the coroutine has called [`coroutine.yield`],
    /// it will return these arguments. Otherwise, the coroutine wasn't yet started, so the
    /// arguments are passed to its main function.
    ///
    /// If the thread is no longer resumable (meaning it has finished execution or encountered an
    /// error), this will return [`Error::CoroutineUnresumable`], otherwise will return `Ok` as
    /// follows:
    ///
    /// If the thread calls [`coroutine.yield`], returns the values passed to `yield`. If the thread
    /// `return`s values from its main function, returns those.
    ///
    /// # Examples
    ///
    /// ```
    /// # use mlua::{Error, Lua, Result, Thread};
    /// # fn main() -> Result<()> {
    /// # let lua = Lua::new();
    /// let thread: Thread = lua.load(r#"
    ///     coroutine.create(function(arg)
    ///         assert(arg == 42)
    ///         local yieldarg = coroutine.yield(123)
    ///         assert(yieldarg == 43)
    ///         return 987
    ///     end)
    /// "#).eval()?;
    ///
    /// assert_eq!(thread.resume::<u32>(42)?, 123);
    /// assert_eq!(thread.resume::<u32>(43)?, 987);
    ///
    /// // The coroutine has now returned, so `resume` will fail
    /// match thread.resume::<u32>(()) {
    ///     Err(Error::CoroutineUnresumable) => {},
    ///     unexpected => panic!("unexpected result {:?}", unexpected),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// [`coroutine.resume`]: https://www.lua.org/manual/5.4/manual.html#pdf-coroutine.resume
    /// [`coroutine.yield`]: https://www.lua.org/manual/5.4/manual.html#pdf-coroutine.yield
    pub fn resume<R>(&self, args: impl IntoLuaMulti) -> Result<R>
    where
        R: FromLuaMulti,
    {
        let lua = self.0.lua.lock();
        let mut pushed_nargs = match self.status_inner(&lua) {
            ThreadStatusInner::New(nargs) | ThreadStatusInner::Yielded(nargs) => nargs,
            _ => return Err(Error::CoroutineUnresumable),
        };

        let state = lua.state();
        let thread_state = self.state();
        unsafe {
            let _sg = StackGuard::new(state);
            let _thread_sg = StackGuard::with_top(thread_state, 0);

            let nargs = args.push_into_specified_stack_multi(&lua, thread_state)?;
            pushed_nargs += nargs;

            let (_, nresults) = self.resume_inner(&lua, pushed_nargs)?;

            R::from_specified_stack_multi(nresults, &lua, thread_state)
        }
    }

    /// Resumes execution of this thread, immediately raising an error.
    ///
    /// This is a Luau specific extension.
    #[cfg(feature = "luau")]
    #[cfg_attr(docsrs, doc(cfg(feature = "luau")))]
    pub fn resume_error<R>(&self, error: impl crate::IntoLua) -> Result<R>
    where
        R: FromLuaMulti,
    {
        let lua = self.0.lua.lock();
        match self.status_inner(&lua) {
            ThreadStatusInner::New(_) | ThreadStatusInner::Yielded(_) => {}
            _ => return Err(Error::CoroutineUnresumable),
        };

        let state = lua.state();
        let thread_state = self.state();
        unsafe {
            let _sg = StackGuard::new(state);
            let _thread_sg = StackGuard::with_top(thread_state, 0);

            check_stack(thread_state, 1)?;
            error.push_into_specified_stack(&lua, thread_state)?;

            let (_, nresults) = self.resume_inner(&lua, ffi::LUA_RESUMEERROR)?;

            R::from_specified_stack_multi(nresults, &lua, thread_state)
        }
    }

    /// Resumes execution of this thread.
    ///
    /// It's similar to `resume()` but leaves `nresults` values on the thread stack.
    unsafe fn resume_inner(&self, lua: &RawLua, nargs: c_int) -> Result<(ThreadStatusInner, c_int)> {
        let state = lua.state();
        let thread_state = self.state();
        let mut nresults = 0;
        #[cfg(not(feature = "luau"))]
        let ret = ffi::lua_resume(thread_state, state, nargs, &mut nresults as *mut c_int);
        #[cfg(feature = "luau")]
        let ret = ffi::lua_resumex(thread_state, state, nargs, &mut nresults as *mut c_int);

        match ret {
            ffi::LUA_OK => Ok((ThreadStatusInner::Finished, nresults)),
            ffi::LUA_YIELD => Ok((ThreadStatusInner::Yielded(0), nresults)),
            ffi::LUA_ERRMEM => {
                // Don't call error handler for memory errors
                Err(pop_error(thread_state, ret))
            }
            _ => {
                check_stack(state, 3)?;
                protect_lua!(state, 0, 1, |state| error_traceback_thread(state, thread_state))?;
                Err(pop_error(state, ret))
            }
        }
    }

    /// Gets the status of the thread.
    pub fn status(&self) -> ThreadStatus {
        match self.status_inner(&self.0.lua.lock()) {
            ThreadStatusInner::New(_) | ThreadStatusInner::Yielded(_) => ThreadStatus::Resumable,
            ThreadStatusInner::Running => ThreadStatus::Running,
            ThreadStatusInner::Finished => ThreadStatus::Finished,
            ThreadStatusInner::Error => ThreadStatus::Error,
        }
    }

    /// Gets the status of the thread (internal implementation).
    fn status_inner(&self, lua: &RawLua) -> ThreadStatusInner {
        let thread_state = self.state();
        if thread_state == lua.state() {
            // The thread is currently running
            return ThreadStatusInner::Running;
        }
        let status = unsafe { ffi::lua_status(thread_state) };
        match status {
            ffi::LUA_YIELD => {
                let top = unsafe { ffi::lua_gettop(thread_state) };
                ThreadStatusInner::Yielded(top)
            }
            ffi::LUA_OK => {
                let top = unsafe { ffi::lua_gettop(thread_state) };
                if top > 0 {
                    ThreadStatusInner::New(top - 1)
                } else {
                    ThreadStatusInner::Finished
                }
            }
            _ => ThreadStatusInner::Error,
        }
    }

    /// Sets a hook function that will periodically be called as Lua code executes.
    ///
    /// This function is similar or [`Lua::set_hook`] except that it sets for the thread.
    /// You can have multiple hooks for different threads.
    ///
    /// To remove a hook call [`Thread::remove_hook`].
    ///
    /// [`Lua::set_hook`]: crate::Lua::set_hook
    #[cfg(not(feature = "luau"))]
    #[cfg_attr(docsrs, doc(cfg(not(feature = "luau"))))]
    pub fn set_hook<F>(&self, triggers: HookTriggers, callback: F) -> Result<()>
    where
        F: Fn(&crate::Lua, Debug) -> Result<crate::VmState> + crate::MaybeSend + 'static,
    {
        let lua = self.0.lua.lock();
        unsafe {
            lua.set_thread_hook(
                self.state(),
                HookKind::Thread(triggers, crate::types::XRc::new(callback)),
            )
        }
    }

    /// Removes any hook function from this thread.
    #[cfg(not(feature = "luau"))]
    #[cfg_attr(docsrs, doc(cfg(not(feature = "luau"))))]
    pub fn remove_hook(&self) {
        let _lua = self.0.lua.lock();
        unsafe {
            ffi::lua_sethook(self.state(), None, 0, 0);
        }
    }

    /// Resets a thread
    ///
    /// In [Lua 5.4]: cleans its call stack and closes all pending to-be-closed variables.
    /// Returns a error in case of either the original error that stopped the thread or errors
    /// in closing methods.
    ///
    /// In Luau: resets to the initial state of a newly created Lua thread.
    /// Lua threads in arbitrary states (like yielded or errored) can be reset properly.
    ///
    /// Other Lua versions can reset only new or finished threads.
    ///
    /// Sets a Lua function for the thread afterwards.
    ///
    /// [Lua 5.4]: https://www.lua.org/manual/5.4/manual.html#lua_closethread
    pub fn reset(&self, func: Function) -> Result<()> {
        let lua = self.0.lua.lock();
        let thread_state = self.state();
        unsafe {
            let status = self.status_inner(&lua);
            self.reset_inner(status)?;

            // Push function to the top of the thread stack
            ffi::lua_xpush(lua.ref_thread(func.0.aux_thread), thread_state, func.0.index);

            #[cfg(feature = "luau")]
            {
                // Inherit `LUA_GLOBALSINDEX` from the main thread
                ffi::lua_xpush(lua.main_state(), thread_state, ffi::LUA_GLOBALSINDEX);
                ffi::lua_replace(thread_state, ffi::LUA_GLOBALSINDEX);
            }

            Ok(())
        }
    }

    unsafe fn reset_inner(&self, status: ThreadStatusInner) -> Result<()> {
        match status {
            ThreadStatusInner::New(_) => {
                // The thread is new, so we can just set the top to 0
                ffi::lua_settop(self.state(), 0);
                Ok(())
            }
            ThreadStatusInner::Running => Err(Error::runtime("cannot reset a running thread")),
            ThreadStatusInner::Finished => Ok(()),
            #[cfg(not(any(feature = "lua54", feature = "luau")))]
            ThreadStatusInner::Yielded(_) | ThreadStatusInner::Error => {
                Err(Error::runtime("cannot reset non-finished thread"))
            }
            #[cfg(any(feature = "lua54", feature = "luau"))]
            ThreadStatusInner::Yielded(_) | ThreadStatusInner::Error => {
                let thread_state = self.state();

                #[cfg(all(feature = "lua54", not(feature = "vendored")))]
                let status = ffi::lua_resetthread(thread_state);
                #[cfg(all(feature = "lua54", feature = "vendored"))]
                let status = {
                    let lua = self.0.lua.lock();
                    ffi::lua_closethread(thread_state, lua.state())
                };
                #[cfg(feature = "lua54")]
                if status != ffi::LUA_OK {
                    return Err(pop_error(thread_state, status));
                }
                #[cfg(feature = "luau")]
                ffi::lua_resetthread(thread_state);

                Ok(())
            }
        }
    }

    /// Enables sandbox mode on this thread.
    ///
    /// Under the hood replaces the global environment table with a new table,
    /// that performs writes locally and proxies reads to caller's global environment.
    ///
    /// This mode ideally should be used together with the global sandbox mode [`Lua::sandbox`].
    ///
    /// Please note that Luau links environment table with chunk when loading it into Lua state.
    /// Therefore you need to load chunks into a thread to link with the thread environment.
    ///
    /// # Examples
    ///
    /// ```
    /// # use mlua::{Lua, Result};
    /// # #[cfg(feature = "luau")]
    /// # fn main() -> Result<()> {
    /// let lua = Lua::new();
    /// let thread = lua.create_thread(lua.create_function(|lua2, ()| {
    ///     lua2.load("var = 123").exec()?;
    ///     assert_eq!(lua2.globals().get::<u32>("var")?, 123);
    ///     Ok(())
    /// })?)?;
    /// thread.sandbox()?;
    /// thread.resume::<()>(())?;
    ///
    /// // The global environment should be unchanged
    /// assert_eq!(lua.globals().get::<Option<u32>>("var")?, None);
    /// # Ok(())
    /// # }
    ///
    /// # #[cfg(not(feature = "luau"))]
    /// # fn main() { }
    /// ```
    #[cfg(any(feature = "luau", doc))]
    #[cfg_attr(docsrs, doc(cfg(feature = "luau")))]
    pub fn sandbox(&self) -> Result<()> {
        let lua = self.0.lua.lock();
        let state = lua.state();
        let thread_state = self.state();
        unsafe {
            check_stack(thread_state, 3)?;
            check_stack(state, 3)?;
            protect_lua!(state, 0, 0, |_| ffi::luaL_sandboxthread(thread_state))
        }
    }

    /// Converts this thread to a generic C pointer.
    ///
    /// There is no way to convert the pointer back to its original value.
    ///
    /// Typically this function is used only for hashing and debug information.
    #[inline]
    pub fn to_pointer(&self) -> *const c_void {
        self.0.to_pointer()
    }
}

impl fmt::Debug for Thread {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_tuple("Thread").field(&self.0).finish()
    }
}

impl PartialEq for Thread {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl LuaType for Thread {
    const TYPE_ID: c_int = ffi::LUA_TTHREAD;
}

#[cfg(test)]
mod assertions {
    use super::*;

    #[cfg(not(feature = "send"))]
    static_assertions::assert_not_impl_any!(Thread: Send);
    #[cfg(feature = "send")]
    static_assertions::assert_impl_all!(Thread: Send, Sync);
}
