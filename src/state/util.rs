use crate::IntoLuaMulti;
use std::mem::take;
use std::os::raw::c_int;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::state::{ExtraData, RawLua};
use crate::util::{self, get_internal_metatable, WrappedFailure};

struct StateGuard<'a>(&'a RawLua, *mut ffi::lua_State);

impl<'a> StateGuard<'a> {
    fn new(inner: &'a RawLua, mut state: *mut ffi::lua_State) -> Self {
        state = inner.state.replace(state);
        Self(inner, state)
    }
}

impl Drop for StateGuard<'_> {
    fn drop(&mut self) {
        self.0.state.set(self.1);
    }
}

pub(crate) enum PreallocatedFailure {
    New(*mut WrappedFailure),
    Reserved,
}

impl PreallocatedFailure {
    unsafe fn reserve(state: *mut ffi::lua_State, extra: *mut ExtraData) -> Self {
        if (*extra).wrapped_failure_top > 0 {
            (*extra).wrapped_failure_top -= 1;
            return PreallocatedFailure::Reserved;
        }

        // We need to check stack for Luau in case when callback is called from interrupt
        // See https://github.com/luau-lang/luau/issues/446 and mlua #142 and #153
        #[cfg(feature = "luau")]
        ffi::lua_rawcheckstack(state, 2);
        // Place it to the beginning of the stack
        let ud = WrappedFailure::new_userdata(state);
        ffi::lua_insert(state, 1);
        PreallocatedFailure::New(ud)
    }

    #[cold]
    unsafe fn r#use(&self, state: *mut ffi::lua_State, extra: *mut ExtraData) -> *mut WrappedFailure {
        let ref_thread = (*extra).ref_thread;
        match *self {
            PreallocatedFailure::New(ud) => {
                ffi::lua_settop(state, 1);
                ud
            }
            PreallocatedFailure::Reserved => {
                let index = (*extra).wrapped_failure_pool.pop().unwrap();
                ffi::lua_settop(state, 0);
                #[cfg(feature = "luau")]
                ffi::lua_rawcheckstack(state, 2);
                ffi::lua_xpush(ref_thread, state, index);
                ffi::lua_pushnil(ref_thread);
                ffi::lua_replace(ref_thread, index);
                (*extra).ref_free.push(index);
                ffi::lua_touserdata(state, -1) as *mut WrappedFailure
            }
        }
    }

    unsafe fn release(self, state: *mut ffi::lua_State, extra: *mut ExtraData) {
        let ref_thread = (*extra).ref_thread;
        match self {
            PreallocatedFailure::New(_) => {
                ffi::lua_rotate(state, 1, -1);
                ffi::lua_xmove(state, ref_thread, 1);
                let index = ref_stack_pop(extra);
                (*extra).wrapped_failure_pool.push(index);
                (*extra).wrapped_failure_top += 1;
            }
            PreallocatedFailure::Reserved => (*extra).wrapped_failure_top += 1,
        }
    }
}

// An optimized version of `callback_error` that does not allocate `WrappedFailure` userdata
// and instead reuses unused values from previous calls (or allocates new).
pub(crate) unsafe fn callback_error_ext<F, R>(
    state: *mut ffi::lua_State,
    mut extra: *mut ExtraData,
    wrap_error: bool,
    f: F,
) -> R
where
    F: FnOnce(*mut ExtraData, c_int) -> Result<R>,
{
    if extra.is_null() {
        extra = ExtraData::get(state);
    }

    let nargs = ffi::lua_gettop(state);

    // We cannot shadow Rust errors with Lua ones, so we need to reserve pre-allocated memory
    // to store a wrapped failure (error or panic) *before* we proceed.
    let prealloc_failure = PreallocatedFailure::reserve(state, extra);

    match catch_unwind(AssertUnwindSafe(|| {
        let rawlua = (*extra).raw_lua();
        let _guard = StateGuard::new(rawlua, state);
        f(extra, nargs)
    })) {
        Ok(Ok(r)) => {
            // Ensure yielded values are cleared
            take(&mut extra.as_mut().unwrap_unchecked().yielded_values);

            // Return unused `WrappedFailure` to the pool
            prealloc_failure.release(state, extra);
            r
        }
        Ok(Err(err)) => {
            let wrapped_error = prealloc_failure.r#use(state, extra);

            if !wrap_error {
                ptr::write(wrapped_error, WrappedFailure::Error(err));
                get_internal_metatable::<WrappedFailure>(state);
                ffi::lua_setmetatable(state, -2);
                ffi::lua_error(state)
            }

            // Build `CallbackError` with traceback
            let traceback = if ffi::lua_checkstack(state, ffi::LUA_TRACEBACK_STACK) != 0 {
                ffi::luaL_traceback(state, state, ptr::null(), 0);
                let traceback = util::to_string(state, -1);
                ffi::lua_pop(state, 1);
                traceback
            } else {
                "<not enough stack space for traceback>".to_string()
            };
            let cause = Arc::new(err);
            ptr::write(
                wrapped_error,
                WrappedFailure::Error(Error::CallbackError { traceback, cause }),
            );
            get_internal_metatable::<WrappedFailure>(state);
            ffi::lua_setmetatable(state, -2);

            ffi::lua_error(state)
        }
        Err(p) => {
            let wrapped_panic = prealloc_failure.r#use(state, extra);
            ptr::write(wrapped_panic, WrappedFailure::Panic(Some(p)));
            get_internal_metatable::<WrappedFailure>(state);
            ffi::lua_setmetatable(state, -2);
            ffi::lua_error(state)
        }
    }
}

/// An yieldable version of `callback_error_ext`
///
/// Unlike ``callback_error_ext``, this method requires a c_int return
/// and not a generic R
pub(crate) unsafe fn callback_error_ext_yieldable<F>(
    state: *mut ffi::lua_State,
    mut extra: *mut ExtraData,
    wrap_error: bool,
    f: F,
) -> c_int
where
    F: FnOnce(*mut ExtraData, c_int) -> Result<c_int>,
{
    if extra.is_null() {
        extra = ExtraData::get(state);
    }

    let nargs = ffi::lua_gettop(state);

    // We cannot shadow Rust errors with Lua ones, so we need to reserve pre-allocated memory
    // to store a wrapped failure (error or panic) *before* we proceed.
    let prealloc_failure = PreallocatedFailure::reserve(state, extra);

    match catch_unwind(AssertUnwindSafe(|| {
        let rawlua = (*extra).raw_lua();
        let _guard = StateGuard::new(rawlua, state);
        f(extra, nargs)
    })) {
        Ok(Ok(r)) => {
            let raw = extra.as_ref().unwrap_unchecked().raw_lua();
            let values = take(&mut extra.as_mut().unwrap_unchecked().yielded_values);

            if !values.is_empty() {
                match values.push_into_stack_multi(raw) {
                    Ok(nargs) => {
                        ffi::lua_pop(state, -1);
                        ffi::lua_xmove(raw.state(), state, nargs);
                        return ffi::lua_yield(state, nargs);
                    }
                    Err(err) => {
                        let wrapped_error = prealloc_failure.r#use(state, extra);
                        ptr::write(
                            wrapped_error,
                            WrappedFailure::Error(Error::external(err.to_string())),
                        );
                        get_internal_metatable::<WrappedFailure>(state);
                        ffi::lua_setmetatable(state, -2);

                        ffi::lua_error(state)
                    }
                }
            }

            // Return unused `WrappedFailure` to the pool
            prealloc_failure.release(state, extra);
            r
        }
        Ok(Err(err)) => {
            let wrapped_error = prealloc_failure.r#use(state, extra);

            if !wrap_error {
                ptr::write(wrapped_error, WrappedFailure::Error(err));
                get_internal_metatable::<WrappedFailure>(state);
                ffi::lua_setmetatable(state, -2);
                ffi::lua_error(state)
            }

            // Build `CallbackError` with traceback
            let traceback = if ffi::lua_checkstack(state, ffi::LUA_TRACEBACK_STACK) != 0 {
                ffi::luaL_traceback(state, state, ptr::null(), 0);
                let traceback = util::to_string(state, -1);
                ffi::lua_pop(state, 1);
                traceback
            } else {
                "<not enough stack space for traceback>".to_string()
            };
            let cause = Arc::new(err);
            ptr::write(
                wrapped_error,
                WrappedFailure::Error(Error::CallbackError { traceback, cause }),
            );
            get_internal_metatable::<WrappedFailure>(state);
            ffi::lua_setmetatable(state, -2);

            ffi::lua_error(state)
        }
        Err(p) => {
            let wrapped_panic = prealloc_failure.r#use(state, extra);
            ptr::write(wrapped_panic, WrappedFailure::Panic(Some(p)));
            get_internal_metatable::<WrappedFailure>(state);
            ffi::lua_setmetatable(state, -2);
            ffi::lua_error(state)
        }
    }
}

pub(super) unsafe fn ref_stack_pop(extra: *mut ExtraData) -> c_int {
    let extra = &mut *extra;
    if let Some(free) = extra.ref_free.pop() {
        ffi::lua_replace(extra.ref_thread, free);
        return free;
    }

    // Try to grow max stack size
    if extra.ref_stack_top >= extra.ref_stack_size {
        let mut inc = extra.ref_stack_size; // Try to double stack size
        while inc > 0 && ffi::lua_checkstack(extra.ref_thread, inc) == 0 {
            inc /= 2;
        }
        if inc == 0 {
            // Pop item on top of the stack to avoid stack leaking and successfully run destructors
            // during unwinding.
            ffi::lua_pop(extra.ref_thread, 1);
            let top = extra.ref_stack_top;
            // It is a user error to create enough references to exhaust the Lua max stack size for
            // the ref thread.
            panic!("cannot create a Lua reference, out of auxiliary stack space (used {top} slots)");
        }
        extra.ref_stack_size += inc;
    }
    extra.ref_stack_top += 1;
    extra.ref_stack_top
}
