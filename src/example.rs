use ecs::logger::{self};
use mlua::{
    // Compiler,
    Function,
    Lua,
    Table,
    Value,
};

#[derive(Debug)]
struct ComponentProxy {}
impl ComponentProxy {}

impl Clone for ComponentProxy {
    fn clone(&self) -> Self {
        println!("CLONED");
        Self {}
    }
}

fn main() {
    // if let Err(error) = devtools::server::init() {
    //     println!("App# can't connect to devtools {}", error);
    // }

    logger::init_logger("./logs");

    ecs::log_sync!(ERROR, "Something bad heppened, please restart application");
    ecs::log!(INFO, "User tried to get out map borders");
    ecs::log!(WARNING, "Model not found");

    ecs::log_sync!(ERROR, "Last log in queue");

    //     let mut engine = Engine::new();
    //     engine
    //         .register_type::<ComponentProxy>()
    //         .register_get_set("id", ComponentProxy::get_id, ComponentProxy::set_id)
    //         .register_fn("get_component", ComponentProxy::new)
    //         .register_fn("get_time", || 123456)
    //         .register_fn("is_paused", || true);
    //
    //     let result = engine.eval::<()>(include_str!("./greyscale.rhai"));
    //
    //     println!("{:?}, {:?}", result, unsafe { COMPONENT_GLOB.id });

    let lua = Lua::new();

    let table = lua.create_table().unwrap();
    let metatable = lua.create_table().unwrap();
    metatable
        .set(
            "__index",
            lua.create_function(|_, (data, index): (Table, String)| {
                println!("{index}");
                println!("{data:?}");
                Ok(12)
            })
            .unwrap(),
        )
        .unwrap();
    metatable
        .set(
            "__newindex",
            lua.create_function(|_, (_, index, value): (Table, String, Value)| {
                println!("{index}");
                println!("{value:?}");
                Ok(())
            })
            .unwrap(),
        )
        .unwrap();
    table.set_metatable(Some(metatable));

    let code = format!(
        r#"
            function main(args) 
                {}
            end

            setfenv(main, {{}});

            return function()
                return main();
            end
        "#,
        include_str!("./log.lua")
    );
    println!("{code}");
    let main1: Function = lua.load(&code).eval().unwrap();
    let main2: Function = lua.load(&code).eval().unwrap();

    // according to lua and mlua code we can pass lua instance between threads
    // but we should lock, and don't call function for single isnstance in same thread
    // the basic implementation will spawn 100 lua environments
    // and assign each system uniueq envronment, in order of execvution
    // 1,2,3,4,5,6,7,8,9,10,11 etc...
    // Registration process must be done dynamiccaly in folowing way
    //
    // types.Position <-
    //       access to Position will check Table with types registers
    //       and register type Position now will be constructor for position proxy
    //       and could be accessed via (types.Position)
    //
    //      iterating and accessing componetn shouldbe also very simple
    //
    //      types.Position.iterate_with(types.Hero)
    //
    //      in background we will get TypeId for position
    //      and TypeId for hero
    //      the we could fetch thios collection
    //      and iterate over them, we can't get exaclty type
    //      but we can get entity id or even array with positions of both compkonetns
    //      then we just need to call type.Position.get(entity), types.Hero.get(entity)
    //      which will return proxy over target objectm, the we can materilaizze it
    //      or change object
    //
    //      immutable borrow should exist until the end of system
    //      i.e next call will be invalid
    //      it's meant that iteration lgick must be part of sytem function/system state
    //
    //      iterate(Positon, Hero)
    //      iterate(Postion, Hero)  // <- on next call collection will be unlocked and eve
    //      types.Hero.get_mut()    // <- only if current system has access to this
    //
    //      I will use LuaJIT because of memory usage, but it's possible to switch to
    //      luau with more info about application memory consumption

    lua.gc_stop();
    let _ = lua.gc_collect();
    let mut v1: Value = main1.call(Value::Nil).unwrap();
    let mut v2: Value = main2.call(Value::Nil).unwrap();

    println!("v1:{v1:?}");
    println!("v2:{v2:?}");

    println!("memory-start: {}", lua.used_memory());

    for _ in 0..10 {
        v1 = main1.call(v1).unwrap();
        v2 = main1.call(v2).unwrap();

        println!("v1: {v1:?}");
        println!("v2: {v2:?}");

        println!("memory: {}", lua.used_memory());
    }

    let _ = lua.gc_collect();
    println!("memory: {}", lua.used_memory());

    // we could use global access, but it will be super costly
    // we need to return an index of enity id and access tarray directl
    // for iteration we must have acces directly via

    // what to do:
    //   1. no shared state -> then there no difference where script will be used
    //
    //   2. immutable hash map state HashMap<"", HashMap<>>
    //   share("root", "key", String / Number / Boolean / None)
    //
    //   3. create lua jit instace for each system
    //
    //   4. create 100 instances and intervaled them
    //      so if instance currently locked skip system
    //

    // on windows message doesn't send immediately
    loop {}
}

// what the cost of putting bitfield directly in bucket ????
// iterating over then will be much slower beccause we also need to load bucket 
// but we don't need additional 128kb of memory, and we could skip access to additional memory region
// we also could use relaxed 
