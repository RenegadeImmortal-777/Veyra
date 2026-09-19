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

## Modules

    use "other.vy";
