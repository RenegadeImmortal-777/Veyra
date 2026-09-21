async fn square(n: Int) -> Int {
    sleep_ms(2);
    return n * n;
}

class Resource {
    fn init(self) -> Null { self.closed=false; return null; }
    fn enter(self) -> Resource { return self; }
    fn exit(self) -> Null { self.closed=true; return null; }
}

let a=square(3);
let b=square(4);
let values=gather([a,b]);
assert(values[0]==9 and values[1]==16);

with Resource() as r {
    assert(r.closed==false);
}
assert(r.closed==true);

try {
    throw "boom";
} catch err {
    assert(err=="boom");
} finally {
    print("cleanup");
}

print("ELEVATION_ASYNC_OK");
