# Veyra Language Guide

File extension: .vy

## Variables

    let age: Int = 20;
    const name: String = "Veyra";

Built-in types include Int, Float, Number, String, Bool, List, Map, Function, Null and Any.

## Functions

    fn add(a: Int, b: Int) -> Int { return a + b; }

## Control flow

    if x > 0 { print("positive"); } else { print("other"); }
    while x < 10 { x += 1; }
    for n in range(5) { print(n); }

## Collections

    let xs = [1, 2, 3];
    let user = {"name": "Ada"};

## Contracts & Effects

    @pure
    fn add(a: Int, b: Int) -> Int {
        requires(a >= 0, "non-negative a");
        let res = a + b;
        ensures(res >= b, "monotonic sum");
        return res;
    }

    @effect("io", "net")
    fn fetch_data(url) { return http_get(url); }

## Static Safety: Taint, Linear Ownership & Verification

    @secret
    fn load_token() -> String { return "sk-live"; }
    let clean = sanitize(load_token());
    print(clean);

    @linear
    fn open_conn(name: String) -> String { return "conn:" + name; }
    let c = open_conn("primary");
    linear_consume(c);

    @verify
    fn safe_div(a: Int, b: Int) -> Int {
        requires(a >= 0, "a >= 0");
        requires(b >= 2, "b >= 2");
        return a / b;
    }

## Modules

    use "other.vy";
