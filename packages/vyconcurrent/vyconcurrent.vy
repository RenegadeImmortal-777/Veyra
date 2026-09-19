fn task(function_name: String, args: List) -> Number { return spawn(function_name, args); }
fn await_result(task_id: Number) -> Any { return await_task(task_id); }
fn done(task_id: Number) -> Bool { return task_done(task_id); }
fn make_channel() -> Number { return channel(); }
fn channel_send(id: Number, value: Any) -> Null { return send(id, value); }
fn channel_recv(id: Number) -> Any { return recv(id); }
fn channel_try_recv(id: Number) -> Any { return try_recv(id); }
fn sleep(milliseconds: Number) -> Null { return sleep_ms(milliseconds); }
