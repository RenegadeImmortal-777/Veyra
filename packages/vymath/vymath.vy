fn clamp(x: Number, lo: Number, hi: Number) -> Number {
    if x < lo { return lo; }
    if x > hi { return hi; }
    return x;
}
fn mean2(a: Number, b: Number) -> Number { return (a + b) / 2; }
fn factorial(n: Int) -> Int {
    let out = 1;
    let i = 2;
    while i <= n { out = out * i; i = i + 1; }
    return out;
}
fn gcd(a: Int, b: Int) -> Int {
    let x = a;
    let y = b;
    let r = 0;
    while y != 0 { r = x % y; x = y; y = r; }
    return x;
}
