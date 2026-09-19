class Counter {
    fn init(self) -> Null {
        self.value = 0;
        return null;
    }

    fn next(self) -> Int {
        self.value = self.value + 1;
        return self.value;
    }
}

let counter = Counter();

fn health(req: Map) -> Map {
    return {"status":200, "content_type":"application/json", "body":json_stringify({"status":"ok","engine":"veyra-apex"})};
}

fn count(req: Map) -> Map {
    return {"status":200, "content_type":"application/json", "body":json_stringify({"count":counter.next()})};
}
