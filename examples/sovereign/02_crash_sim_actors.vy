// Sovereign Demo 02: Crash-Safe Deterministic VFS Simulation & TCP Actors
// Run:
//   .\bin\veyra-native.exe run examples\sovereign\02_crash_sim_actors.vy

print("=== Part 1: Deterministic Fault-Injectable Crash Simulation ===");
sim_start(42);
sim_fs_write("/wal/epoch1.ckpt", "{\"committed\":true,\"offset\":4096}");
let synced = sim_fs_sync();

// Simulate buffered, un-synced state right before an unexpected power loss.
sim_fs_write("/wal/inflight.tmp", "uncommitted-dirty-page");
let crash = sim_fs_crash();

print("synced files:", synced, "| lost dirty files on crash:", crash["lost_dirty"]);
print("durable checkpoint recovered:", sim_fs_read("/wal/epoch1.ckpt"));
print("virtual files present after crash:", sim_fs_list("/"));
sim_stop();

print("=== Part 2: Location-Transparent Supervised TCP Actors ===");
fn counter_actor(state: Int, msg: Int) -> Map {
    let next = state + msg;
    return {"state": next, "reply": next};
}

let aid = actor_spawn(counter_actor, {"init": 100});
let node = actor_node_listen(aid, 0);
print("listening on:", node["addr"]);

let r1 = actor_remote_ask(node["addr"], 15, 2000);
let r2 = actor_remote_ask(node["addr"], 27, 2000);
print("remote RPC replies:", r1, r2);

actor_node_stop(node);
actor_stop(aid);
print("cluster shutdown clean");
