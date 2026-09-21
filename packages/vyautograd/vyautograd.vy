fn backward(loss: Map) -> Null { return tensor_backward(loss); }
fn gradient(parameter: Map) -> List { return tensor_grad(parameter); }
