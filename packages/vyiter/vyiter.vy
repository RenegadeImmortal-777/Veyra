fn next_value(generator: Generator) -> Any { return next(generator); }
fn collect_all(iterable: Any) -> List { return collect(iterable); }
fn finished(generator: Generator) -> Bool { return generator_done(generator); }
fn map_values(function: Function, values: List) -> List { return map_call(function, values); }
fn filter_values(function: Function, values: List) -> List { return filter_call(function, values); }
fn reduce_values(function: Function, values: List, initial: Any) -> Any { return reduce_call(function, values, initial); }
