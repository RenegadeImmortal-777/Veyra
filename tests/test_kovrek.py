import io, os, sys, unittest
from contextlib import redirect_stdout
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
import kovrek

class KovrekTests(unittest.TestCase):
    def execute(self, source):
        out=io.StringIO()
        with redirect_stdout(out): kovrek.run_source(source)
        return out.getvalue().strip()

    def test_arithmetic(self):
        self.assertEqual(self.execute('print(2 + 3 * 4);'), '14')

    def test_function_recursion(self):
        src='fn f(n: Int) -> Int { if n <= 1 { return n; } return f(n-1)+f(n-2); } print(f(10));'
        self.assertEqual(self.execute(src), '55')

    def test_loop(self):
        src='let s: Int = 0; for n in range(1,6) { s += n; } print(s);'
        self.assertEqual(self.execute(src), '15')

    def test_map_assignment(self):
        src='let x = {"a": 1}; x["b"] = 2; print(x["b"]);'
        self.assertEqual(self.execute(src), '2')

    def test_const(self):
        with self.assertRaises(kovrek.KovrekError): self.execute('const x = 1; x = 2;')

    def test_type_check(self):
        with self.assertRaises(kovrek.KovrekError): self.execute('let x: Int = "bad";')

    def test_class_and_method(self):
        src='class C { fn init(self, x: Int) -> Void { self.x = x; } fn inc(self) -> Int { self.x += 1; return self.x; } } let c=C(4); print(c.inc());'
        self.assertEqual(self.execute(src), '5')

    def test_try_throw(self):
        src='try { throw "boom"; } catch err { print(err); }'
        self.assertEqual(self.execute(src), 'boom')

    def test_tuple_set(self):
        src='let t=(1,2,3); let s={1,2,3}; print(len(t)); print(len(s));'
        self.assertEqual(self.execute(src), '3\n3')

    def test_python_bridge(self):
        src='let m=py_import("math"); print(m.sqrt(81));'
        self.assertEqual(self.execute(src), '9.0')

    def test_spawn_await(self):
        src='fn square(n: Int) -> Int { return n*n; } let t=spawn(square,9); print(await_task(t));'
        self.assertEqual(self.execute(src), '81')

    def test_semantic_undefined(self):
        with self.assertRaises(kovrek.KovrekError):
            kovrek.semantic_check('print(missing_name);')

    def test_semantic_valid(self):
        kovrek.semantic_check('fn f(x: Int) -> Int { return x+1; } let y=f(4); print(y);')

    def test_json_regex(self):
        src=r'let x=json_parse("{\"a\": 7}"); print(x.a); print(len(regex("[0-9]+","a12b34")));'
        self.assertEqual(self.execute(src), '7\n2')

    def test_sqlite_bridge(self):
        src='let db=db_open(":memory:"); db.execute("create table t(x integer)"); db.close(); print("ok");'
        self.assertEqual(self.execute(src), 'ok')

if __name__ == '__main__': unittest.main()
