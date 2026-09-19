fn mean(xs: List) -> Number { return vec_mean(xs); }
fn variance(xs: List) -> Number { return vec_variance(xs); }
fn stddev(xs: List) -> Number { return vec_std(xs); }
fn dot(a: List, b: List) -> Number { return vec_dot(a, b); }
fn scale(xs: List, k: Number) -> List { return vec_scale(xs, k); }
fn matrix_multiply(a: List, b: List) -> List { return matmul(a, b); }
fn matrix_transpose(a: List) -> List { return transpose(a); }
