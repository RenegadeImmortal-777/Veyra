fn parameter(values: Any) -> Map { return tensor(values,true); }
fn constant(values: Any) -> Map { return tensor(values,false); }
fn linear(input: Map, weights: Map) -> Map { return tensor_matmul(input,weights); }
fn relu(input: Map) -> Map { return tensor_relu(input); }
fn sigmoid(input: Map) -> Map { return tensor_sigmoid(input); }
fn tanh(input: Map) -> Map { return tensor_tanh(input); }
fn softmax(input: Map) -> Map { return tensor_softmax(input); }
fn mse(predicted: Map, actual: Map) -> Map {
    let difference=tensor_sub(predicted,actual);
    return tensor_mean(tensor_mul(difference,difference));
}
fn train_step(loss: Map, parameter: Map, learning_rate: Number) -> Null {
    tensor_backward(loss);
    tensor_step(parameter,learning_rate);
    tensor_zero_grad(parameter);
}
fn save(parameter: Map, path: String) -> Null { return tensor_save(parameter,path); }
fn load(path: String) -> Map { return tensor_load(path); }
