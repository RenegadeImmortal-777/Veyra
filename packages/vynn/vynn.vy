fn parameter(values: Any) -> Map { return tensor(values, true); }
fn constant(values: Any) -> Map { return tensor(values, false); }
fn linear(x: Map, weights: Map) -> Map { return tensor_matmul(x, weights); }
fn relu(x: Map) -> Map { return tensor_relu(x); }
fn mse(predicted: Map, actual: Map) -> Map {
    let difference = tensor_sub(predicted, actual);
    let squared = tensor_mul(difference, difference);
    return tensor_mean(squared);
}
