import io, os, sys, unittest
from contextlib import redirect_stdout
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
import veyra

class VeyraTests(unittest.TestCase):
    def execute(self, source):
        out=io.StringIO()
        with redirect_stdout(out): veyra.run_source(source)
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
        with self.assertRaises(veyra.VeyraError): self.execute('const x = 1; x = 2;')

    def test_type_check(self):
        with self.assertRaises(veyra.VeyraError): self.execute('let x: Int = "bad";')

if __name__ == '__main__': unittest.main()
