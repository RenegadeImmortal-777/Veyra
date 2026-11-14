fn step(parameter: Map, learning_rate: Number) -> Null { return tensor_step(parameter, learning_rate); }
fn adam(parameter: Map, learning_rate: Number) -> Null {
    return tensor_adam_step(parameter, learning_rate, 0.9, 0.999, 0.00000001);
}
fn adam_custom(parameter: Map, learning_rate: Number, beta1: Number, beta2: Number, epsilon: Number) -> Null {
    return tensor_adam_step(parameter, learning_rate, beta1, beta2, epsilon);
}
fn reset(parameter: Map) -> Null { return tensor_reset_optimizer(parameter); }
fn zero_grad(parameter: Map) -> Null { return tensor_zero_grad(parameter); }
