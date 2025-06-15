use crate::error::{Error, Result};
use crate::state::util::get_next_spot;
use crate::state::RawLua;
use crate::util::check_stack;
use crate::{ffi, Lua, WeakLua, Table, Function, Thread};
use crate::types::MaybeSend;
use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, BitXor, BitXorAssign};

/// Flags describing the set of lute standard libraries to load.
#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct LuteStdLib(u32);

impl LuteStdLib {
    #[cfg(feature = "luau-lute-crypto")]
    pub const CRYPTO: LuteStdLib = LuteStdLib(1);
    pub const FS: LuteStdLib = LuteStdLib(1 << 1);
    pub const LUAU: LuteStdLib = LuteStdLib(1 << 2);
    #[cfg(feature = "luau-lute-net")]
    pub const NET: LuteStdLib = LuteStdLib(1 << 3);
    pub const PROCESS: LuteStdLib = LuteStdLib(1 << 4);
    pub const TASK: LuteStdLib = LuteStdLib(1 << 5);
    pub const VM: LuteStdLib = LuteStdLib(1 << 6);
    pub const SYSTEM: LuteStdLib = LuteStdLib(1 << 7);
    pub const TIME: LuteStdLib = LuteStdLib(1 << 8);

    /// No libraries
    pub const NONE: LuteStdLib = LuteStdLib(0);
    /// (**unsafe**) All standard libraries
    pub const ALL: LuteStdLib = LuteStdLib(u32::MAX);

    pub fn contains(self, lib: Self) -> bool {
        (self & lib).0 != 0
    }
}

#[derive(Debug, PartialEq)]
pub enum LuteSchedulerRunOnceResult {
    Empty,
    Success(Thread),
}

impl BitAnd for LuteStdLib {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self::Output {
        LuteStdLib(self.0 & rhs.0)
    }
}

impl BitAndAssign for LuteStdLib {
    fn bitand_assign(&mut self, rhs: Self) {
        *self = LuteStdLib(self.0 & rhs.0)
    }
}

impl BitOr for LuteStdLib {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self::Output {
        LuteStdLib(self.0 | rhs.0)
    }
}

impl BitOrAssign for LuteStdLib {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = LuteStdLib(self.0 | rhs.0)
    }
}

impl BitXor for LuteStdLib {
    type Output = Self;
    fn bitxor(self, rhs: Self) -> Self::Output {
        LuteStdLib(self.0 ^ rhs.0)
    }
}

impl BitXorAssign for LuteStdLib {
    fn bitxor_assign(&mut self, rhs: Self) {
        *self = LuteStdLib(self.0 ^ rhs.0)
    }
}

/// A handle to the lute runtime, which provides access to various standard libraries
/// and functionality within lute
#[derive(Debug, Clone)]
pub struct LuteRuntimeHandle {
    #[cfg(feature = "luau-lute-crypto")]
    pub crypto: Option<Table>,
    pub fs: Option<Table>,
    pub luau: Option<Table>,
    #[cfg(feature = "luau-lute-net")]
    pub net: Option<Table>,
    pub process: Option<Table>,
    pub task: Option<Table>,
    pub vm: Option<Table>,
    pub system: Option<Table>,
    pub time: Option<Table>,
    pub scheduler_run_once: Function,
}

impl LuteRuntimeHandle {
    pub(crate) fn new(rawlua: &RawLua) -> Result<Self> {
        let mut handle = Self {
            #[cfg(feature = "luau-lute-crypto")]
            crypto: None,
            fs: None,
            luau: None,
            #[cfg(feature = "luau-lute-net")]
            net: None,
            process: None,
            task: None,
            vm: None,
            system: None,
            time: None,
            scheduler_run_once: rawlua.lute_run_once_lua()?
        };

        Ok(handle)
    }
}

pub struct Lute(pub(crate) WeakLua);

impl Lute {
    pub(crate) fn new(lua: &Lua) -> Result<Self> {
        let lute = Self(lua.weak());
        
        let lua = lua.lock();
        if !lua.is_lute_loaded()? {
            lua.setup_lute_runtime()?;
        }

        Ok(lute)
    }

    /// Loads the specified lute standard libraries into the current Lua state.
    ///
    /// This errors if the runtime is not loaded.
    pub fn load_stdlib(&self, libs: LuteStdLib) -> Result<()> {
        let Some(lua) = self.0.try_upgrade() else {
            return Err(Error::RuntimeError("Lua VM not open".into()));
        };
        lua.lock().load_lute_stdlib(libs)
    }

    /// Sets a runtime initialization routine which will be called whenever lute
    /// tries to make a new lute child runtime.
    /// 
    /// This is, for example, used in ``@lute/vm`` to setup the state of the child 
    /// VM
    #[cfg(feature = "send")]
    pub fn set_runtime_initter<F>(&self, initter: F) -> Result<()>
    where
        F: Fn(&Lua, &Lua) -> Result<()> + Send + Sync + 'static,
    {
        let Some(lua) = self.0.try_upgrade() else {
            return Err(Error::RuntimeError("Lua VM not open".into()));
        };

        lua.lock().set_lute_runtime_initter(initter);
        Ok(())
    }

    /// Sets a runtime initialization routine which will be called whenever lute
    /// tries to make a new lute child runtime.
    /// 
    /// This is, for example, used in ``@lute/vm`` to setup the state of the child 
    /// VM
    #[cfg(not(feature = "send"))]
    pub fn set_runtime_initter<F>(&self, initter: F) -> Result<()>
    where
        F: Fn(&Lua, &Lua) -> Result<()> + 'static,
    {
        let Some(lua) = self.0.try_upgrade() else {
            return Err(Error::RuntimeError("Lua VM not open".into()));
        };

        lua.lock().set_lute_runtime_initter(initter);
        Ok(())
    }

    /// Returns if the lute scheduler has work to do
    pub fn has_work(&self) -> Result<bool> {
        let Some(lua) = self.0.try_upgrade() else {
            return Err(Error::RuntimeError("Lua VM not open".into()));
        };

        lua.lock().has_lute_work()
    }

    /// Returns if the lute scheduler has threads to run
    pub fn has_threads(&self) -> Result<bool> {
        let Some(lua) = self.0.try_upgrade() else {
            return Err(Error::RuntimeError("Lua VM not open".into()));
        };

        lua.lock().has_lute_threads()
    }

    /// Returns if the lute scheduler has continuations to run
    pub fn has_continuations(&self) -> Result<bool> {
        let Some(lua) = self.0.try_upgrade() else {
            return Err(Error::RuntimeError("Lua VM not open".into()));
        };

        lua.lock().has_lute_continuations()
    }

    /// Runs a function on the lute handle if it is loaded.
    pub fn with_handle<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(LuteRuntimeHandle) -> Result<R>,
    {
        let Some(lua) = self.0.try_upgrade() else {
            return Err(Error::RuntimeError("Lua VM not open".into()));
        };

        let handle = lua.lock().lute_handle()
            .ok_or_else(|| Error::RuntimeError("Lute runtime is not loaded".into()))?;

        f(handle)
    }

    /// Returns the ``crypto`` library from the lute runtime, if it is loaded.
    #[cfg(feature = "luau-lute-crypto")]
    pub fn crypto(&self) -> Result<Option<Table>> {
        self.with_handle(|h| Ok(h.crypto))
    }

    /// Returns the ``fs`` library from the lute runtime, if it is loaded.
    pub fn fs(&self) -> Result<Option<Table>> {
        self.with_handle(|h| Ok(h.fs))
    }

    /// Returns the ``luau`` library from the lute runtime, if it is loaded.
    pub fn luau(&self) -> Result<Option<Table>> {
        self.with_handle(|h| Ok(h.luau))
    }

    /// Returns the ``net`` library from the lute runtime, if it is loaded.
    #[cfg(feature = "luau-lute-net")]
    pub fn net(&self) -> Result<Option<Table>> {
        self.with_handle(|h| Ok(h.net))
    }

    /// Returns the ``process`` library from the lute runtime, if it is loaded.
    pub fn process(&self) -> Result<Option<Table>> {
        self.with_handle(|h| Ok(h.process))
    }

    /// Returns the ``task`` library from the lute runtime, if it is loaded.
    pub fn task(&self) -> Result<Option<Table>> {
        self.with_handle(|h| Ok(h.task))
    }

    /// Returns the ``vm`` library from the lute runtime, if it is loaded.
    pub fn vm(&self) -> Result<Option<Table>> {
        self.with_handle(|h| Ok(h.vm))
    }

    /// Returns the ``system`` library from the lute runtime, if it is loaded.
    pub fn system(&self) -> Result<Option<Table>> {
        self.with_handle(|h| Ok(h.system))
    }

    /// Returns the ``time`` library from the lute runtime, if it is loaded.
    pub fn time(&self) -> Result<Option<Table>> {
        self.with_handle(|h| Ok(h.time))
    }

    /// Returns the ``scheduler_run_once`` function from the lute runtime, if it is loaded.
    pub fn scheduler_run_once(&self) -> Result<Function> {
        self.with_handle(|h| Ok(h.scheduler_run_once))
    }

    /// Run one iteration of the lute scheduler.
    pub fn run_scheduler_once(&self) -> Result<LuteSchedulerRunOnceResult> {
        let Some(lua) = self.0.try_upgrade() else {
            return Err(Error::RuntimeError("Lua VM not open".into()));
        };

        lua.lock().lute_run_once()
    }

    /// Returns a handle to the lute runtime, if it is loaded.
    /// 
    /// The handle will contain references to the loaded standard libraries.
    /// 
    /// Note that this will return a copy of the internal handle so updates
    /// via ``Lute::load_stdlib`` will not be reflected in this handle.
    pub fn handle(&self) -> Result<Option<LuteRuntimeHandle>> {
        let Some(lua) = self.0.try_upgrade() else {
            return Err(Error::RuntimeError("Lua VM not open".into()));
        };

        Ok(lua.lock().lute_handle())
    }

    /// Returns if a lute runtime is loaded into the client or not
    /// 
    /// This should always be true unless ``destroy_runtime`` has been called
    /// or the Lua state has been destroyed.
    pub fn is_loaded(&self) -> Result<bool> {
        let Some(lua) = self.0.try_upgrade() else {
            return Err(Error::RuntimeError("Lua VM not open".into()));
        };
        lua.lock().is_lute_loaded()
    }

    /// Sets up the lute runtime if it is not already loaded.
    /// 
    /// Should not be needed in most cases as the runtime is automatically set up
    /// but could be useful if ``Lute::destroy_runtime`` has been called.
    pub fn setup_runtime(&self) -> Result<()> {
        let Some(lua) = self.0.try_upgrade() else {
            return Err(Error::RuntimeError("Lua VM not open".into()));
        };

        let lua = lua.lock();
        if lua.is_lute_loaded()? {
            return Ok(());
        }

        lua.setup_lute_runtime()
    }

    /// Destroys the lute runtime on the current Lua state. This internally destroys the auxiliary
    /// VM created to act as the data VM as well
    /// 
    /// # Safety
    /// 
    /// This is unsafe if user code is holding any references to code from Lute
    /// 
    /// Most user code will never need to call this as the runtime is automatically destroyed
    /// when the Lua state is destroyed.
    pub unsafe fn destroy_runtime(self) -> Result<bool> {
        let Some(lua) = self.0.try_upgrade() else {
            return Err(Error::RuntimeError("Lute runtime is not loaded".into()));
        };

        lua.lock().destroy_lute_runtime()
    }
}

impl Lua {
    /// Returns a handle to the lute runtime
    /// 
    /// This will setup the lute runtime if it is not already loaded.
    pub fn lute(&self) -> Result<Lute> {
        Lute::new(self)
    }
}
