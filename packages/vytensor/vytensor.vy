fn create(values: Any, requires_grad: Bool) -> Map { return tensor(values, requires_grad); }
fn values(t: Map) -> List { return tensor_values(t); }
fn shape(t: Map) -> List { return tensor_shape(t); }
fn grad(t: Map) -> List { return tensor_grad(t); }
fn add(a: Map, b: Map) -> Map { return tensor_add(a, b); }
fn sub(a: Map, b: Map) -> Map { return tensor_sub(a, b); }
fn mul(a: Map, b: Map) -> Map { return tensor_mul(a, b); }
fn matmul(a: Map, b: Map) -> Map { return tensor_matmul(a, b); }
fn relu(t: Map) -> Map { return tensor_relu(t); }
fn sigmoid(t: Map) -> Map { return tensor_sigmoid(t); }
fn tanh(t: Map) -> Map { return tensor_tanh(t); }
fn exp(t: Map) -> Map { return tensor_exp(t); }
fn log(t: Map) -> Map { return tensor_log(t); }
fn reshape(t: Map, shape: List) -> Map { return tensor_reshape(t, shape); }
fn transpose(t: Map) -> Map { return tensor_transpose(t); }
fn save(t: Map, path: String) -> Null { return tensor_save(t, path); }
fn load(path: String) -> Map { return tensor_load(path); }
fn sum(t: Map) -> Map { return tensor_sum(t); }
fn mean(t: Map) -> Map { return tensor_mean(t); }
