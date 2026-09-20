fn utc_now() -> String { return now_utc(); }
fn local_now() -> String { return now_local(); }
fn format_timestamp(timestamp: Number, format: String) -> String { return datetime_format(timestamp, format); }
fn unix_time() -> Number { return clock(); }
