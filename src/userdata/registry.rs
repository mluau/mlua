#![allow(clippy::await_holding_refcell_ref, clippy::await_holding_lock)]

use std::any::TypeId;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::os::raw::c_void;
use std::string::String as StdString;

use crate::error::{Error, Result};
use crate::state::{Lua, LuaGuard};
use crate::traits::{FromLua, FromLuaMulti, IntoLua, IntoLuaMulti};
use crate::types::{Callback, MaybeSend};
use crate::userdata::{
    borrow_userdata_scoped, borrow_userdata_scoped_mut, AnyUserData, MetaMethod, TypeIdHints, UserData,
    UserDataFields, UserDataMethods, UserDataStorage,
};
use crate::util::short_type_name;
use crate::value::Value;

#[derive(Clone, Copy)]
enum UserDataType {
    Shared(TypeIdHints),
    Unique(*mut c_void),
}

/// Handle to registry for userdata methods and metamethods.
pub struct UserDataRegistry<T> {
    lua: LuaGuard,
    raw: RawUserDataRegistry,
    r#type: UserDataType,
    _phantom: PhantomData<T>,
}

pub(crate) struct RawUserDataRegistry {
    // Fields
    pub(crate) fields: Vec<(String, Result<Value>)>,
    pub(crate) field_getters: Vec<(String, Callback)>,
    pub(crate) field_setters: Vec<(String, Callback)>,
    pub(crate) meta_fields: Vec<(String, Result<Value>)>,

    // Methods
    pub(crate) methods: Vec<(String, Callback)>,
    pub(crate) meta_methods: Vec<(String, Callback)>,

    pub(crate) destructor: ffi::lua_CFunction,
    pub(crate) type_id: Option<TypeId>,
    pub(crate) type_name: StdString,
}

impl UserDataType {
    #[inline]
    pub(crate) fn type_id(&self) -> Option<TypeId> {
        match self {
            UserDataType::Shared(hints) => Some(hints.type_id()),
            UserDataType::Unique(_) => None,
        }
    }
}

#[cfg(feature = "send")]
unsafe impl Send for UserDataType {}

impl<T: 'static> UserDataRegistry<T> {
    #[inline(always)]
    pub(crate) fn new(lua: &Lua) -> Self {
        Self::with_type(lua, UserDataType::Shared(TypeIdHints::new::<T>()))
    }
}

impl<T> UserDataRegistry<T> {
    #[inline(always)]
    pub(crate) fn new_unique(lua: &Lua, ud_ptr: *mut c_void) -> Self {
        Self::with_type(lua, UserDataType::Unique(ud_ptr))
    }

    #[inline(always)]
    fn with_type(lua: &Lua, r#type: UserDataType) -> Self {
        let raw = RawUserDataRegistry {
            fields: Vec::new(),
            field_getters: Vec::new(),
            field_setters: Vec::new(),
            meta_fields: Vec::new(),
            methods: Vec::new(),
            meta_methods: Vec::new(),
            destructor: super::util::destroy_userdata_storage::<T>,
            type_id: r#type.type_id(),
            type_name: short_type_name::<T>(),
        };

        UserDataRegistry {
            lua: lua.lock_arc(),
            raw,
            r#type,
            _phantom: PhantomData,
        }
    }

    fn box_method<M, A, R>(&self, name: &str, method: M) -> Callback
    where
        M: Fn(&Lua, &T, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let name = get_function_name::<T>(name);
        macro_rules! try_self_arg {
            ($res:expr) => {
                $res.map_err(|err| Error::bad_self_argument(&name, err))?
            };
        }

        let target_type = self.r#type;
        Box::new(move |rawlua, nargs| unsafe {
            if nargs == 0 {
                let err = Error::from_lua_conversion("missing argument", "userdata", None);
                try_self_arg!(Err(err));
            }
            let state = rawlua.state();
            // Find absolute "self" index before processing args
            let self_index = ffi::lua_absindex(state, -nargs);
            // Self was at position 1, so we pass 2 here
            let args = A::from_specified_stack_args(nargs - 1, 2, Some(&name), rawlua, state);

            match target_type {
                #[rustfmt::skip]
                UserDataType::Shared(type_hints) => {
                    let type_id = try_self_arg!(rawlua.get_userdata_type_id::<T>(state, self_index));
                    try_self_arg!(borrow_userdata_scoped(state, self_index, type_id, type_hints, |ud| {
                        method(rawlua.lua(), ud, args?)?.push_into_specified_stack_multi(rawlua, state)
                    }))
                }
                UserDataType::Unique(target_ptr) if ffi::lua_touserdata(state, self_index) == target_ptr => {
                    let ud = target_ptr as *mut UserDataStorage<T>;
                    try_self_arg!((*ud).try_borrow_scoped(|ud| {
                        method(rawlua.lua(), ud, args?)?.push_into_specified_stack_multi(rawlua, state)
                    }))
                }
                UserDataType::Unique(_) => {
                    try_self_arg!(rawlua.get_userdata_type_id::<T>(state, self_index));
                    Err(Error::bad_self_argument(&name, Error::UserDataTypeMismatch))
                }
            }
        })
    }

    fn box_method_mut<M, A, R>(&self, name: &str, method: M) -> Callback
    where
        M: FnMut(&Lua, &mut T, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let name = get_function_name::<T>(name);
        macro_rules! try_self_arg {
            ($res:expr) => {
                $res.map_err(|err| Error::bad_self_argument(&name, err))?
            };
        }

        let method = RefCell::new(method);
        let target_type = self.r#type;
        Box::new(move |rawlua, nargs| unsafe {
            let mut method = method.try_borrow_mut().map_err(|_| Error::RecursiveMutCallback)?;
            if nargs == 0 {
                let err = Error::from_lua_conversion("missing argument", "userdata", None);
                try_self_arg!(Err(err));
            }
            let state = rawlua.state();
            // Find absolute "self" index before processing args
            let self_index = ffi::lua_absindex(state, -nargs);
            // Self was at position 1, so we pass 2 here
            let args = A::from_specified_stack_args(nargs - 1, 2, Some(&name), rawlua, state);

            match target_type {
                #[rustfmt::skip]
                UserDataType::Shared(type_hints) => {
                    let type_id = try_self_arg!(rawlua.get_userdata_type_id::<T>(state, self_index));
                    try_self_arg!(borrow_userdata_scoped_mut(state, self_index, type_id, type_hints, |ud| {
                        method(rawlua.lua(), ud, args?)?.push_into_specified_stack_multi(rawlua, state)
                    }))
                }
                UserDataType::Unique(target_ptr) if ffi::lua_touserdata(state, self_index) == target_ptr => {
                    let ud = target_ptr as *mut UserDataStorage<T>;
                    try_self_arg!((*ud).try_borrow_scoped_mut(|ud| {
                        method(rawlua.lua(), ud, args?)?.push_into_specified_stack_multi(rawlua, state)
                    }))
                }
                UserDataType::Unique(_) => {
                    try_self_arg!(rawlua.get_userdata_type_id::<T>(state, self_index));
                    Err(Error::bad_self_argument(&name, Error::UserDataTypeMismatch))
                }
            }
        })
    }

    fn box_function<F, A, R>(&self, name: &str, function: F) -> Callback
    where
        F: Fn(&Lua, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let name = get_function_name::<T>(name);
        Box::new(move |lua, nargs| unsafe {
            let state = lua.state();
            let args = A::from_specified_stack_args(nargs, 1, Some(&name), lua, state)?;
            function(lua.lua(), args)?.push_into_specified_stack_multi(lua, state)
        })
    }

    fn box_function_mut<F, A, R>(&self, name: &str, function: F) -> Callback
    where
        F: FnMut(&Lua, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let name = get_function_name::<T>(name);
        let function = RefCell::new(function);
        Box::new(move |lua, nargs| unsafe {
            let function = &mut *function
                .try_borrow_mut()
                .map_err(|_| Error::RecursiveMutCallback)?;
            let state = lua.state();
            let args = A::from_specified_stack_args(nargs, 1, Some(&name), lua, state)?;
            function(lua.lua(), args)?.push_into_specified_stack_multi(lua, state)
        })
    }

    pub(crate) fn check_meta_field(lua: &Lua, name: &str, value: impl IntoLua) -> Result<Value> {
        let value = value.into_lua(lua)?;
        if name == MetaMethod::Index || name == MetaMethod::NewIndex {
            match value {
                Value::Nil | Value::Table(_) | Value::Function(_) => {}
                _ => {
                    return Err(Error::MetaMethodTypeError {
                        method: name.to_string(),
                        type_name: value.type_name(),
                        message: Some("expected nil, table or function".to_string()),
                    })
                }
            }
        }
        value.into_lua(lua)
    }

    #[inline(always)]
    pub(crate) fn into_raw(self) -> RawUserDataRegistry {
        self.raw
    }
}

// Returns function name for the type `T`, without the module path
fn get_function_name<T>(name: &str) -> StdString {
    format!("{}.{name}", short_type_name::<T>())
}

impl<T> UserDataFields<T> for UserDataRegistry<T> {
    fn add_field<V>(&mut self, name: impl ToString, value: V)
    where
        V: IntoLua + 'static,
    {
        let name = name.to_string();
        self.raw.fields.push((name, value.into_lua(self.lua.lua())));
    }

    fn add_field_method_get<M, R>(&mut self, name: impl ToString, method: M)
    where
        M: Fn(&Lua, &T) -> Result<R> + MaybeSend + 'static,
        R: IntoLua,
    {
        let name = name.to_string();
        let callback = self.box_method(&name, move |lua, data, ()| method(lua, data));
        self.raw.field_getters.push((name, callback));
    }

    fn add_field_method_set<M, A>(&mut self, name: impl ToString, method: M)
    where
        M: FnMut(&Lua, &mut T, A) -> Result<()> + MaybeSend + 'static,
        A: FromLua,
    {
        let name = name.to_string();
        let callback = self.box_method_mut(&name, method);
        self.raw.field_setters.push((name, callback));
    }

    fn add_field_function_get<F, R>(&mut self, name: impl ToString, function: F)
    where
        F: Fn(&Lua, AnyUserData) -> Result<R> + MaybeSend + 'static,
        R: IntoLua,
    {
        let name = name.to_string();
        let callback = self.box_function(&name, function);
        self.raw.field_getters.push((name, callback));
    }

    fn add_field_function_set<F, A>(&mut self, name: impl ToString, mut function: F)
    where
        F: FnMut(&Lua, AnyUserData, A) -> Result<()> + MaybeSend + 'static,
        A: FromLua,
    {
        let name = name.to_string();
        let callback = self.box_function_mut(&name, move |lua, (data, val)| function(lua, data, val));
        self.raw.field_setters.push((name, callback));
    }

    fn add_meta_field<V>(&mut self, name: impl ToString, value: V)
    where
        V: IntoLua + 'static,
    {
        let lua = self.lua.lua();
        let name = name.to_string();
        let field = Self::check_meta_field(lua, &name, value).and_then(|v| v.into_lua(lua));
        self.raw.meta_fields.push((name, field));
    }

    fn add_meta_field_with<F, R>(&mut self, name: impl ToString, f: F)
    where
        F: FnOnce(&Lua) -> Result<R> + 'static,
        R: IntoLua,
    {
        let lua = self.lua.lua();
        let name = name.to_string();
        let field = f(lua).and_then(|v| Self::check_meta_field(lua, &name, v).and_then(|v| v.into_lua(lua)));
        self.raw.meta_fields.push((name, field));
    }
}

impl<T> UserDataMethods<T> for UserDataRegistry<T> {
    fn add_method<M, A, R>(&mut self, name: impl ToString, method: M)
    where
        M: Fn(&Lua, &T, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let name = name.to_string();
        let callback = self.box_method(&name, method);
        self.raw.methods.push((name, callback));
    }

    fn add_method_mut<M, A, R>(&mut self, name: impl ToString, method: M)
    where
        M: FnMut(&Lua, &mut T, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let name = name.to_string();
        let callback = self.box_method_mut(&name, method);
        self.raw.methods.push((name, callback));
    }

    fn add_function<F, A, R>(&mut self, name: impl ToString, function: F)
    where
        F: Fn(&Lua, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let name = name.to_string();
        let callback = self.box_function(&name, function);
        self.raw.methods.push((name, callback));
    }

    fn add_function_mut<F, A, R>(&mut self, name: impl ToString, function: F)
    where
        F: FnMut(&Lua, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let name = name.to_string();
        let callback = self.box_function_mut(&name, function);
        self.raw.methods.push((name, callback));
    }

    fn add_meta_method<M, A, R>(&mut self, name: impl ToString, method: M)
    where
        M: Fn(&Lua, &T, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let name = name.to_string();
        let callback = self.box_method(&name, method);
        self.raw.meta_methods.push((name, callback));
    }

    fn add_meta_method_mut<M, A, R>(&mut self, name: impl ToString, method: M)
    where
        M: FnMut(&Lua, &mut T, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let name = name.to_string();
        let callback = self.box_method_mut(&name, method);
        self.raw.meta_methods.push((name, callback));
    }

    fn add_meta_function<F, A, R>(&mut self, name: impl ToString, function: F)
    where
        F: Fn(&Lua, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let name = name.to_string();
        let callback = self.box_function(&name, function);
        self.raw.meta_methods.push((name, callback));
    }

    fn add_meta_function_mut<F, A, R>(&mut self, name: impl ToString, function: F)
    where
        F: FnMut(&Lua, A) -> Result<R> + MaybeSend + 'static,
        A: FromLuaMulti,
        R: IntoLuaMulti,
    {
        let name = name.to_string();
        let callback = self.box_function_mut(&name, function);
        self.raw.meta_methods.push((name, callback));
    }
}

macro_rules! lua_userdata_impl {
    ($type:ty) => {
        impl<T: UserData + 'static> UserData for $type {
            fn register(registry: &mut UserDataRegistry<Self>) {
                let mut orig_registry = UserDataRegistry::new(registry.lua.lua());
                T::register(&mut orig_registry);

                // Copy all fields, methods, etc. from the original registry
                (registry.raw.fields).extend(orig_registry.raw.fields);
                (registry.raw.field_getters).extend(orig_registry.raw.field_getters);
                (registry.raw.field_setters).extend(orig_registry.raw.field_setters);
                (registry.raw.meta_fields).extend(orig_registry.raw.meta_fields);
                (registry.raw.methods).extend(orig_registry.raw.methods);
                (registry.raw.meta_methods).extend(orig_registry.raw.meta_methods);
            }
        }
    };
}

// A special proxy object for UserData
pub(crate) struct UserDataProxy<T>(pub(crate) PhantomData<T>);

lua_userdata_impl!(UserDataProxy<T>);

#[cfg(all(feature = "userdata-wrappers", not(feature = "send")))]
lua_userdata_impl!(std::rc::Rc<T>);
#[cfg(all(feature = "userdata-wrappers", not(feature = "send")))]
lua_userdata_impl!(std::rc::Rc<std::cell::RefCell<T>>);
#[cfg(feature = "userdata-wrappers")]
lua_userdata_impl!(std::sync::Arc<T>);
#[cfg(feature = "userdata-wrappers")]
lua_userdata_impl!(std::sync::Arc<std::sync::Mutex<T>>);
#[cfg(feature = "userdata-wrappers")]
lua_userdata_impl!(std::sync::Arc<std::sync::RwLock<T>>);
#[cfg(feature = "userdata-wrappers")]
lua_userdata_impl!(std::sync::Arc<parking_lot::Mutex<T>>);
#[cfg(feature = "userdata-wrappers")]
lua_userdata_impl!(std::sync::Arc<parking_lot::RwLock<T>>);

#[cfg(test)]
mod assertions {
    #[cfg(feature = "send")]
    static_assertions::assert_impl_all!(super::RawUserDataRegistry: Send);
}
