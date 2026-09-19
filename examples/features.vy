let total: Int = 0;
for n in range(1, 6) { total += n; }
let person = {"name": "Veyra", "version": 1};
person["stable"] = true;
print("sum =", total);
print(person);
assert(total == 15, "loop failed");
