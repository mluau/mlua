#![cfg(feature = "luau-lute")]

use mlua::prelude::*;

#[test]
fn test_lute_runtime() -> LuaResult<()> {
    let lua = Lua::new();
    lua.set_memory_limit(1024 * 1024 * 100)?; // Set memory limit to 100 MB

    pub struct B {
        v: i32,
    }

    impl LuaUserData for B {
        fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
            fields.add_field_method_get("value", |_, this| Ok(this.v));
        }
    }

    // Register the B type in Luau
    lua.load("b = ...; return b")
        .call::<LuaUserDataRef<B>>(B { v: 42 })?;
    assert_eq!(lua.load("return b.value").eval::<i32>()?, 42);

    // Load the lute runtime
    lua.lute()?.load_stdlib(LuaLuteStdLib::TIME)?;
    assert!(lua.lute()?.is_loaded()?);
    let time = lua.lute()?
        .time()?
        .expect("Time library is not loaded");

    lua.lute()?.set_runtime_initter(|parent, child| {
        let my_vec = vec![1, 2, 3, 4, 5];
        println!("my_vec created");
        println!("set_runtime_initter method called!");
        parent.set_memory_limit(1024 * 1024 * 50)?;
        println!("Parent Lua state memory limit set to 50 MB");
        parent.globals().set("test_mluau_var", 42)?;
        println!("test_mluau_var set to 42 in parent Lua state");
        
        let th = child.create_thread(
            child.load("return 'Hello from child Lua state!'").into_function()?,
        )?;
        
        child.set_memory_limit(1024 * 1024)?;
        println!("Child Lua state memory limit");
        child.globals().set("test_mluau_var", 132)?;
        println!("test_mluau_var set to 132 in child Lua state");
        parent.set_app_data::<Lua>(child.clone());
        println!("Child Lua state created");
        Ok(())
    })?;

    lua.globals().set("time", time)?;

    pub struct A {
        v: i32,
    }

    impl LuaUserData for A {
        fn add_fields<F: LuaUserDataFields<Self>>(fields: &mut F) {
            fields.add_field_method_get("value", |_, this| Ok(this.v));
        }
    }

    // Register the A type in Luau
    lua.load("a = ...; return a")
        .call::<LuaUserDataRef<A>>(A { v: 100 })?;
    assert_eq!(lua.load("return a.value").eval::<i32>()?, 100);

    let res = lua.load("aud = ...; assert(aud.value == 32, 'aud is invalid'); a = time.duration.seconds(2) + time.duration.seconds(3); return a").call::<LuaAnyUserData>(A {
        v: 32
    })?;

    unsafe {
        // Check for lute's special metatable
        let metatable = res.underlying_metatable()?;
        assert_eq!(
            metatable.get::<LuaString>("__metatable")?.to_str()?,
            "The metatable is locked"
        );
    }

    lua.lute()?.load_stdlib(LuaLuteStdLib::VM)?;
    let vm = lua.lute()?
        .vm()?
        .expect("VM library is not loaded");
    lua.globals().set("vm", vm)?;

    // Print current working directory
    let res = lua.load("local a = vm.create('./mluau/tests/lute/test').l(); print(a); return a")
    .set_name("=stdin")
    .into_function()?;

    println!("res: {:?}", res);

    let th = lua.create_thread(res)?;
    let res = th.resume::<LuaMultiValue>(())?;

    let child = lua.remove_app_data::<Lua>()
    .expect("Child Lua state not set in parent Lua state");

    // Run VM scheduler until it has no work left
    println!("Running VM scheduler until no work left...");
    while child.lute()?.has_work()? {
        println!("Running VM scheduler once...");
        let res = child.lute()?.run_scheduler_once()?;
        println!("VM scheduler run once result: {:?}", res);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    assert!(lua.lute()?.has_work()?);

    let scheduler_run_once_fn = lua.lute()?.scheduler_run_once()?;
    while lua.lute()?.has_work()? {
        println!("Running scheduler once...");
        let res = lua.lute()?.run_scheduler_once()?;
        println!("Scheduler run once result: {:?}", res);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    let res = unsafe {
        th.pop_results::<i32>()?
    };

    assert_eq!(res, 78);



    Ok(())
}
