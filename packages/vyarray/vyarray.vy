fn zeros(count: Int) -> List {
    let out=[];
    let i=0;
    while i<count { out=append(out,0); i=i+1; }
    return out;
}
fn ones(count: Int) -> List {
    let out=[];
    let i=0;
    while i<count { out=append(out,1); i=i+1; }
    return out;
}
fn full(count: Int, value: Number) -> List {
    let out=[];
    let i=0;
    while i<count { out=append(out,value); i=i+1; }
    return out;
}
fn arange(start: Int, stop: Int, step: Int) -> List {
    let out=[];
    let x=start;
    if step==0 { throw "step cannot be zero"; }
    if step>0 { while x<stop { out=append(out,x); x=x+step; } }
    else { while x>stop { out=append(out,x); x=x+step; } }
    return out;
}
fn sum_values(values: List) -> Number { return vec_sum(values); }
fn mean(values: List) -> Number { return vec_mean(values); }
fn std(values: List) -> Number { return vec_std(values); }
fn median(values: List) -> Number { return vec_median(values); }
fn percentile(values: List, p: Number) -> Number { return vec_percentile(values,p); }
fn dot(a: List, b: List) -> Number { return vec_dot(a,b); }
fn add(a: List, b: List) -> List { return vec_add(a,b); }
fn scale(values: List, factor: Number) -> List { return vec_scale(values,factor); }
fn transpose2d(matrix: List) -> List { return transpose(matrix); }
fn matmul2d(a: List, b: List) -> List { return matmul(a,b); }
fn determinant(matrix: List) -> Number { return matrix_det(matrix); }
fn inverse(matrix: List) -> List { return matrix_inverse(matrix); }
fn solve(matrix: List, values: List) -> List { return linear_solve(matrix,values); }
fn normalize_values(values: List) -> List { return normalize(values); }
