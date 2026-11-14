# Kovrek Language Guide

File extension: .kvr

## Variables

    let age: Int = 20;
    const name: String = "Kovrek";

Built-in types include Int, Float, Number, String, Bool, List, Map, Function, Null and Any.

## Functions

    fn add(a: Int, b: Int) -> Int { return a + b; }

## Control flow

    if x > 0 { print("positive"); } else { print("other"); }
    while x < 10 { x += 1; }
    for n in range(5) { print(n); }

## Collections

    let xs = [1, 2, 3];
    let user = {"name": "Ada"};

## Contracts & Effects

    @pure
    fn add(a: Int, b: Int) -> Int {
        requires(a >= 0, "non-negative a");
        let res = a + b;
        ensures(res >= b, "monotonic sum");
        return res;
    }

    @effect("io", "net")
    fn fetch_data(url) { return http_get(url); }

## Static Safety: Taint, Linear Ownership & Verification

    @secret
    fn load_token() -> String { return "sk-live"; }
    let clean = sanitize(load_token());
    print(clean);

    @linear
    fn open_conn(name: String) -> String { return "conn:" + name; }
    let c = open_conn("primary");
    linear_consume(c);

    @verify
    fn safe_div(a: Int, b: Int) -> Int {
        requires(a >= 0, "a >= 0");
        requires(b >= 2, "b >= 2");
        return a / b;
    }

## Distributed Actors, Crash-Safe Simulation & Profiling

    fn counter_svc(state: Int, msg: Int) -> Map {
        return {"state": state + msg, "reply": state + msg};
    }
    let aid = actor_spawn(counter_svc, {"init": 100});
    let node = actor_node_listen(aid, 0);
    let reply = actor_remote_ask(node["addr"], 23, 2000);
    actor_node_stop(node);
    actor_stop(aid);

    sim_start(42);
    sim_fs_write("/wal/entry", "committed");
    sim_fs_sync();
    sim_fs_write("/wal/temp", "uncommitted");
    sim_fs_crash(); // drops uncommitted writes while keeping /wal/entry

    profile_start();
    let work = safe_div(100, 5);
    let report = profile_stop();

## Modules

    use "other.kvr";
