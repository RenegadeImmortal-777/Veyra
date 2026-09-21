let tensor = import_package("vytensor");
let autograd = import_package("vyautograd");
let optim = import_package("vyoptim");

let x = tensor.create([[1],[2],[3],[4]], false);
let y = tensor.create([[2],[4],[6],[8]], false);
let w = tensor.create([[0]], true);

let epoch = 0;
while epoch < 40 {
    let predicted = tensor.matmul(x, w);
    let difference = tensor.sub(predicted, y);
    let loss = tensor.mean(tensor.mul(difference, difference));
    autograd.backward(loss);
    optim.step(w, 0.05);
    optim.zero_grad(w);
    epoch = epoch + 1;
}

let trained = tensor.values(w);
assert(trained[0] > 1.9 and trained[0] < 2.1);
print("ASCENDANT_AUTOGRAD_OK", trained[0]);
