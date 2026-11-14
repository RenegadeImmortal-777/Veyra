// Sovereign Demo 03: Tiled GPU Kernels, Causal Flash Attention, Mixed Precision & Autograd
// Run:
//   .\bin\veyra-native.exe run examples\sovereign\03_gpu_flash_autograd.vy
//   .\bin\veyra-native.exe profile examples\sovereign\03_gpu_flash_autograd.vy

let tm = gpu_kernel_inspect("tiled_matmul");
let ak = gpu_kernel_inspect("attention_causal");
print("tiled_matmul uses groupshared:", tm["uses_workgroup_shared"], "| SPIR-V words:", len(tm["spirv_words"]));
print("attention_causal SPIR-V words:", len(ak["spirv_words"]));

let a = [[1.0, 2.0], [3.0, 4.0]];
let b = [[2.0, 0.0], [1.0, 2.0]];
let prod = gpu_tiled_matmul(a, b);
print("gpu_tiled_matmul result:", prod);

let attn = gpu_attention([[1.0, 0.0], [0.0, 1.0]], [[1.0, 0.0], [0.0, 1.0]], [[10.0, 0.0], [0.0, 20.0]], true);
print("causal flash attention output:", attn);

let normed = gpu_fused_layernorm([[1.0, 3.0]], [[0.0, 0.0]], [1.0, 1.0], [0.0, 0.0], 0.00001, "gelu");
print("fused Residual+LayerNorm+GELU:", normed);

let w_orig = tensor([[1.5, -2.25]], false);
let w_bf16 = tensor_cast_dtype(w_orig, "bf16");
print("bf16 quantized values:", tensor_values(w_bf16));

// Autograd + Adam 5-step convergence under zero-overhead flight recorder
let x = tensor([[1.0, 2.0], [3.0, 4.0]], false);
let w = tensor([[0.5, -0.5], [1.0, 0.25]], true);
let bias = tensor([0.1, -0.2], true);

fn train_step(x, w, bias) -> Float {
    tensor_zero_grad(w);
    tensor_zero_grad(bias);
    let pred = tensor_relu(tensor_add(tensor_matmul(x, w), bias));
    let loss = tensor_mean(tensor_pow(pred, 2.0));
    tensor_backward(loss);
    tensor_adam_step(w, 0.08, 0.9, 0.999, 0.00000001);
    tensor_adam_step(bias, 0.08, 0.9, 0.999, 0.00000001);
    return tensor_values(loss)[0];
}

profile_start();
let epoch = 1;
while epoch <= 5 {
    let loss_val = train_step(x, w, bias);
    print("step", epoch, "loss =", round(loss_val, 5));
    epoch += 1;
}
let summary = profile_stop();
print("profiled calls:", summary["total_calls"]);
