# Veyra 10.0 Zenith Showcase — Reverse-Mode Tensor Autograd & Adam Optimizer

let x = tensor([[1, 1], [2, 1], [3, 1], [4, 1]], false);
let y = tensor([[3], [5], [7], [9]], false);
let w = tensor([[0], [0]], true);

for epoch in range(150) {
    let pred = tensor_matmul(x, w);
    let diff = tensor_sub(pred, y);
    let loss = tensor_mean(tensor_pow(diff, 2));
    tensor_backward(loss);
    tensor_adam_step(w, 0.1, 0.9, 0.999, 0.00000001);
    tensor_zero_grad(w);
}

let learned = tensor_values(w);
assert(learned[0] > 1.9 and learned[0] < 2.1);
assert(learned[1] > 0.9 and learned[1] < 1.1);
print("Zenith autograd + Adam converged:", learned);
