fn contains(xs: List, value: Any) -> Bool {
    for i in range(0, len(xs)) { if xs[i] == value { return true; } }
    return false;
}
fn index_of(xs: List, value: Any) -> Int {
    for i in range(0, len(xs)) { if xs[i] == value { return i; } }
    return -1;
}
fn binary_search(xs: List, value: Number) -> Int {
    let lo = 0;
    let hi = len(xs) - 1;
    let mid = 0;
    while lo <= hi {
        mid = floor((lo + hi) / 2);
        if xs[mid] == value { return mid; }
        if xs[mid] < value { lo = mid + 1; } else { hi = mid - 1; }
    }
    return -1;
}
