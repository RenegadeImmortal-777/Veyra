fn available() -> Bool { return gpu_available(); }
fn info() -> Map { return gpu_info(); }
fn devices() -> List { return gpu_devices(); }
fn backend() -> String {
    let m = gpu_info();
    return m["backend"];
}
fn matmul(a: Any, b: Any) -> Any { return gpu_matmul(a, b); }
fn add(a: List, b: List) -> List { return gpu_vec_add(a, b); }
fn sub(a: List, b: List) -> List { return gpu_vec_sub(a, b); }
fn mul(a: List, b: List) -> List { return gpu_vec_mul(a, b); }
fn relu(a: List) -> List { return gpu_relu(a); }
fn sigmoid(a: List) -> List { return gpu_sigmoid(a); }
fn scale(a: List, factor: Number) -> List { return gpu_scale(a, factor); }
fn softmax(m: List) -> List { return gpu_softmax(m); }
fn run_hlsl(source: String, entry: String, buf_a: List, buf_b: List, out_len: Number) -> List {
    return gpu_run_hlsl(source, entry, buf_a, buf_b, out_len);
}
fn to_gpu(t: Map) -> Map { return tensor_to(t, "gpu"); }
fn to_cpu(t: Map) -> Map { return tensor_to(t, "cpu"); }
fn device(t: Map) -> String { return tensor_device(t); }
fn set_default_device(dev: String) -> String { return tensor_default_device(dev); }
