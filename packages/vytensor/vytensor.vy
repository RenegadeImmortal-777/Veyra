fn create(values: Any, requires_grad: Bool) -> Map { return tensor(values, requires_grad); }
fn values(t: Map) -> List { return tensor_values(t); }
fn shape(t: Map) -> List { return tensor_shape(t); }
fn grad(t: Map) -> List { return tensor_grad(t); }
fn add(a: Map, b: Map) -> Map { return tensor_add(a, b); }
fn sub(a: Map, b: Map) -> Map { return tensor_sub(a, b); }
fn mul(a: Map, b: Map) -> Map { return tensor_mul(a, b); }
fn matmul(a: Map, b: Map) -> Map { return tensor_matmul(a, b); }
fn relu(t: Map) -> Map { return tensor_relu(t); }
fn sum(t: Map) -> Map { return tensor_sum(t); }
fn mean(t: Map) -> Map { return tensor_mean(t); }
