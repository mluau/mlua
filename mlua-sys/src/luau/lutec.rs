use super::lua::lua_State;
use core::ffi::c_int;

extern "C" {
    //pub fn lutec_opencrypto(state: *mut lua_State);
    pub fn lutec_openfs(state: *mut lua_State);
    pub fn lutec_openluau(state: *mut lua_State);
    //pub fn lutec_opennet(state: *mut lua_State);
    pub fn lutec_openprocess(state: *mut lua_State);
    pub fn lutec_opentask(state: *mut lua_State);
    pub fn lutec_openvm(state: *mut lua_State);
    pub fn lutec_opensystem(state: *mut lua_State);
    pub fn lutec_opentime(state: *mut lua_State) -> c_int;
    pub fn lutec_isruntimeloaded(state: *mut lua_State) -> c_int;
    pub fn lutec_setup_runtime(state: *mut lua_State);
    pub fn lutec_destroy_runtime(state: *mut lua_State) -> c_int;
    pub fn lutec_set_runtimeinitter(callback: lutec_setupState_init) -> c_int;
}

extern "C-unwind" {
    pub fn lutec_run_once(state: *mut lua_State) -> RunOnceResult;
    pub fn lutec_run_once_lua(state: *mut lua_State) -> c_int;
    pub fn lutec_has_work(state: *mut lua_State) -> c_int;
    pub fn lutec_has_threads(state: *mut lua_State) -> c_int;
    pub fn lutec_has_continuation(state: *mut lua_State) -> c_int;
}

pub const LUTE_STATE_MISSING_ERROR: c_int = 0;
pub const LUTE_STATE_ERROR: c_int = 1;
pub const LUTE_STATE_SUCCESS: c_int = 2;
pub const LUTE_STATE_EMPTY: c_int = 3;
pub const LUTE_STATE_UNSUPPORTED_OP: c_int = 4;

#[repr(C)]
#[allow(non_camel_case_types)]
pub struct RunOnceResult {
    pub op: c_int,             // Operation result code
    pub state: *mut lua_State, // The lua_State that was run, if applicable
}

#[repr(C)]
pub struct lua_State_wrapper {
    pub parent: *mut lua_State, // Pointer to the parent lua_State (if any)
    pub L: *mut lua_State,
}

#[allow(non_camel_case_types)]
#[repr(C)]
pub struct lutec_setupState {
    pub setup_lua_state: unsafe extern "C-unwind" fn(wrapper: *mut lua_State_wrapper),
}

// Populates function pointers in the given lutec_setupState.
pub type lutec_setupState_init = unsafe extern "C" fn(config: *mut lutec_setupState);
