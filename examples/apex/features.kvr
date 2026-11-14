class Counter {
    fn init(self, value: Int) -> Null {
        self.value = value;
        return null;
    }

    fn next(self) -> Int {
        self.value = self.value + 1;
        return self.value;
    }
}

fn square(n: Int) -> Int {
    return n * n;
}

let counter: Counter = Counter(0);
let first = counter.next();

let task = spawn("square", [9]);
let squared = await task;

let ch = channel();
send(ch, "native");
let message = recv(ch);

let values = sort(set([3, 1, 3, 2]));
let total = sum(values);

print(first, squared, message, values, total);
