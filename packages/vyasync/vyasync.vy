fn spawn_task(function_name: String, args: List) -> Number { return spawn(function_name, args); }
fn await_result(task_id: Number) -> Any { return await_task(task_id); }
fn gather_all(task_ids: List) -> List { return gather(task_ids); }
fn done(task_id: Number) -> Bool { return task_done(task_id); }
fn sleep(milliseconds: Number) -> Null { return sleep_ms(milliseconds); }
