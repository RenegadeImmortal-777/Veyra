#!/usr/bin/env python3
from __future__ import annotations
import argparse, math, os, sys, time, json, re, csv, sqlite3, subprocess, importlib, random
import urllib.request
from concurrent.futures import ThreadPoolExecutor, Future
from pathlib import Path
from dataclasses import dataclass
from typing import Any

VERSION = "3.2.0-convergence"

class VeyraError(Exception):
    pass

@dataclass
class Token:
    kind: str
    value: Any
    line: int
    col: int

KEYWORDS = {
    "let","const","fn","class","if","else","while","for","in","return","true","false",
    "null","break","continue","use","and","or","not","try","catch","throw"
}

class Lexer:
    def __init__(self, source: str, filename: str = "<input>"):
        self.s = source
        self.file = filename
        self.i = 0
        self.line = 1
        self.col = 1

    def err(self, msg):
        raise VeyraError(f"{self.file}:{self.line}:{self.col}: {msg}")

    def peek(self, n=0):
        j = self.i + n
        return self.s[j] if j < len(self.s) else "\0"

    def advance(self):
        c = self.peek()
        if c == "\0":
            return c
        self.i += 1
        if c == "\n":
            self.line += 1
            self.col = 1
        else:
            self.col += 1
        return c

    def tokens(self):
        out = []
        while self.i < len(self.s):
            c = self.peek()
            if c in " \t\r\n":
                self.advance(); continue
            if c == "/" and self.peek(1) == "/":
                while self.peek() not in ("\n", "\0"):
                    self.advance()
                continue
            if c == "/" and self.peek(1) == "*":
                self.advance(); self.advance()
                while not (self.peek() == "*" and self.peek(1) == "/"):
                    if self.peek() == "\0":
                        self.err("unterminated block comment")
                    self.advance()
                self.advance(); self.advance(); continue
            line, col = self.line, self.col
            if c.isalpha() or c == "_":
                text = ""
                while self.peek().isalnum() or self.peek() == "_":
                    text += self.advance()
                out.append(Token(text if text in KEYWORDS else "IDENT", text, line, col)); continue
            if c.isdigit():
                text = ""
                while self.peek().isdigit():
                    text += self.advance()
                if self.peek() == "." and self.peek(1).isdigit():
                    text += self.advance()
                    while self.peek().isdigit():
                        text += self.advance()
                    out.append(Token("NUMBER", float(text), line, col))
                else:
                    out.append(Token("NUMBER", int(text), line, col))
                continue
            if c in "\"'":
                q = self.advance(); text = ""
                while self.peek() != q:
                    if self.peek() == "\0":
                        self.err("unterminated string")
                    if self.peek() == "\\":
                        self.advance(); e = self.advance()
                        text += {"n":"\n","t":"\t","r":"\r","\\":"\\","\"":"\"","'":"'","0":"\0"}.get(e, e)
                    else:
                        text += self.advance()
                self.advance(); out.append(Token("STRING", text, line, col)); continue
            matched = False
            for op in ("==","!=","<=",">=","&&","||","->","**","+=","-=","*=","/="):
                if self.s.startswith(op, self.i):
                    for _ in op:
                        self.advance()
                    out.append(Token(op, op, line, col)); matched = True; break
            if matched:
                continue
            if c in "+-*/%(){}[],:;.=<>!":
                self.advance(); out.append(Token(c, c, line, col)); continue
            self.err(f"unexpected character {c!r}")
        out.append(Token("EOF", None, self.line, self.col))
        return out

@dataclass
class Literal: value: Any
@dataclass
class Var: name: str
@dataclass
class Unary: op: str; right: Any
@dataclass
class Binary: left: Any; op: str; right: Any
@dataclass
class Call: fn: Any; args: list
@dataclass
class Index: obj: Any; key: Any
@dataclass
class Member: obj: Any; name: str
@dataclass
class ListExpr: items: list
@dataclass
class TupleExpr: items: list
@dataclass
class SetExpr: items: list
@dataclass
class MapExpr: pairs: list
@dataclass
class LetStmt: name: str; type_name: str|None; value: Any; const: bool
@dataclass
class AssignStmt: target: Any; op: str; value: Any
@dataclass
class ExprStmt: expr: Any
@dataclass
class Block: body: list
@dataclass
class IfStmt: cond: Any; then: Block; otherwise: Any
@dataclass
class WhileStmt: cond: Any; body: Block
@dataclass
class ForStmt: name: str; iterable: Any; body: Block
@dataclass
class FunctionStmt: name: str; params: list; return_type: str|None; body: Block
@dataclass
class ClassStmt: name: str; body: Block
@dataclass
class TryStmt: body: Block; error_name: str; handler: Block
@dataclass
class ThrowStmt: value: Any
@dataclass
class ReturnStmt: value: Any
@dataclass
class FlowStmt: kind: str
@dataclass
class UseStmt: path: str

class Parser:
    def __init__(self, tokens, filename="<input>"):
        self.t = tokens
        self.i = 0
        self.file = filename
    def cur(self): return self.t[self.i]
    def match(self, *kinds):
        if self.cur().kind in kinds:
            x = self.cur(); self.i += 1; return x
        return None
    def expect(self, kind, msg=None):
        x = self.match(kind)
        if not x:
            c = self.cur()
            raise VeyraError(f"{self.file}:{c.line}:{c.col}: {msg or ('expected '+kind)}, got {c.kind}")
        return x
    def program(self):
        body = []
        while self.cur().kind != "EOF":
            body.append(self.statement())
        return Block(body)
    def semi(self): self.match(";")
    def statement(self):
        if self.match("let"): return self.let_stmt(False)
        if self.match("const"): return self.let_stmt(True)
        if self.match("fn"): return self.fn_stmt()
        if self.match("class"): return self.class_stmt()
        if self.match("if"): return self.if_stmt()
        if self.match("while"): return self.while_stmt()
        if self.match("for"): return self.for_stmt()
        if self.match("try"): return self.try_stmt()
        if self.match("throw"):
            v = self.expr(); self.semi(); return ThrowStmt(v)
        if self.match("return"):
            v = None if self.cur().kind in (";","}") else self.expr()
            self.semi(); return ReturnStmt(v)
        if self.match("break"): self.semi(); return FlowStmt("break")
        if self.match("continue"): self.semi(); return FlowStmt("continue")
        if self.match("use"):
            p = self.expect("STRING", "expected module path string").value
            self.semi(); return UseStmt(p)
        if self.cur().kind == "{":
            return self.block()
        e = self.expr()
        if self.cur().kind in ("=","+=","-=","*=","/="):
            op = self.cur().kind; self.i += 1; v = self.expr(); self.semi(); return AssignStmt(e, op, v)
        self.semi(); return ExprStmt(e)
    def let_stmt(self, const):
        name = self.expect("IDENT", "expected variable name").value
        typ = None
        if self.match(":"):
            typ = self.expect("IDENT", "expected type name").value
        self.expect("=", "expected '=' after variable declaration")
        v = self.expr(); self.semi(); return LetStmt(name, typ, v, const)
    def fn_stmt(self):
        name = self.expect("IDENT", "expected function name").value
        self.expect("("); params = []
        if self.cur().kind != ")":
            while True:
                pn = self.expect("IDENT", "expected parameter name").value; pt = None
                if self.match(":"):
                    pt = self.expect("IDENT", "expected parameter type").value
                params.append((pn, pt))
                if not self.match(","):
                    break
        self.expect(")")
        ret = None
        if self.match("->"):
            ret = self.expect("IDENT", "expected return type").value
        return FunctionStmt(name, params, ret, self.block())
    def class_stmt(self):
        name = self.expect("IDENT", "expected class name").value
        return ClassStmt(name, self.block())
    def try_stmt(self):
        body = self.block()
        self.expect("catch", "expected 'catch'")
        error_name = self.expect("IDENT", "expected catch variable").value
        return TryStmt(body, error_name, self.block())
    def block(self):
        self.expect("{"); body = []
        while self.cur().kind != "}":
            if self.cur().kind == "EOF":
                self.expect("}", "unterminated block")
            body.append(self.statement())
        self.expect("}"); return Block(body)
    def if_stmt(self):
        cond = self.expr(); then = self.block(); otherwise = None
        if self.match("else"):
            otherwise = self.if_stmt() if self.match("if") else self.block()
        return IfStmt(cond, then, otherwise)
    def while_stmt(self): return WhileStmt(self.expr(), self.block())
    def for_stmt(self):
        name = self.expect("IDENT", "expected loop variable").value
        self.expect("in", "expected 'in'")
        return ForStmt(name, self.expr(), self.block())
    def expr(self): return self.logic_or()
    def logic_or(self):
        e = self.logic_and()
        while self.cur().kind in ("||","or"):
            op = self.cur().kind; self.i += 1; e = Binary(e, op, self.logic_and())
        return e
    def logic_and(self):
        e = self.equality()
        while self.cur().kind in ("&&","and"):
            op = self.cur().kind; self.i += 1; e = Binary(e, op, self.equality())
        return e
    def equality(self):
        e = self.compare()
        while self.cur().kind in ("==","!="):
            op = self.cur().kind; self.i += 1; e = Binary(e, op, self.compare())
        return e
    def compare(self):
        e = self.term()
        while self.cur().kind in ("<","<=",">",">="):
            op = self.cur().kind; self.i += 1; e = Binary(e, op, self.term())
        return e
    def term(self):
        e = self.factor()
        while self.cur().kind in ("+","-"):
            op = self.cur().kind; self.i += 1; e = Binary(e, op, self.factor())
        return e
    def factor(self):
        e = self.power()
        while self.cur().kind in ("*","/","%"):
            op = self.cur().kind; self.i += 1; e = Binary(e, op, self.power())
        return e
    def power(self):
        e = self.unary()
        if self.match("**"):
            e = Binary(e, "**", self.power())
        return e
    def unary(self):
        if self.cur().kind in ("!","not","-","+"):
            op = self.cur().kind; self.i += 1; return Unary(op, self.unary())
        return self.call()
    def call(self):
        e = self.primary()
        while True:
            if self.match("("):
                args = []
                if self.cur().kind != ")":
                    while True:
                        args.append(self.expr())
                        if not self.match(","):
                            break
                self.expect(")"); e = Call(e, args)
            elif self.match("["):
                k = self.expr(); self.expect("]"); e = Index(e, k)
            elif self.match("."):
                e = Member(e, self.expect("IDENT", "expected member name").value)
            else:
                break
        return e
    def primary(self):
        x = self.match("NUMBER")
        if x: return Literal(x.value)
        x = self.match("STRING")
        if x: return Literal(x.value)
        if self.match("true"): return Literal(True)
        if self.match("false"): return Literal(False)
        if self.match("null"): return Literal(None)
        x = self.match("IDENT")
        if x: return Var(x.value)
        if self.match("("):
            if self.match(")"): return TupleExpr([])
            e = self.expr()
            if self.match(","):
                items = [e]
                if self.cur().kind != ")":
                    while True:
                        items.append(self.expr())
                        if not self.match(","): break
                self.expect(")"); return TupleExpr(items)
            self.expect(")"); return e
        if self.match("["):
            a = []
            if self.cur().kind != "]":
                while True:
                    a.append(self.expr())
                    if not self.match(","):
                        break
            self.expect("]"); return ListExpr(a)
        if self.match("{"):
            if self.match("}"): return MapExpr([])
            first = self.expr()
            if self.match(":"):
                pairs = [(first, self.expr())]
                while self.match(","):
                    if self.cur().kind == "}": break
                    k = self.expr(); self.expect(":"); pairs.append((k, self.expr()))
                self.expect("}"); return MapExpr(pairs)
            items = [first]
            while self.match(","):
                if self.cur().kind == "}": break
                items.append(self.expr())
            self.expect("}"); return SetExpr(items)
        c = self.cur()
        raise VeyraError(f"{self.file}:{c.line}:{c.col}: expected expression, got {c.kind}")

class Env:
    def __init__(self, parent=None):
        self.parent = parent
        self.values = {}
        self.const = set()
        self.types = {}
    def define(self, n, v, const=False, type_name=None):
        if n in self.values:
            raise VeyraError(f"name '{n}' already defined in this scope")
        self.values[n] = v; self.types[n] = type_name
        if const: self.const.add(n)
    def find(self, n):
        if n in self.values: return self
        if self.parent: return self.parent.find(n)
        raise VeyraError(f"undefined name '{n}'")
    def get(self, n): return self.find(n).values[n]
    def set(self, n, v):
        e = self.find(n)
        if n in e.const:
            raise VeyraError(f"cannot assign to constant '{n}'")
        if e.types.get(n):
            check_type(v, e.types[n], n)
        e.values[n] = v

def type_of(v):
    if v is None: return "Null"
    if isinstance(v, bool): return "Bool"
    if isinstance(v, int) and not isinstance(v, bool): return "Int"
    if isinstance(v, float): return "Float"
    if isinstance(v, str): return "String"
    if isinstance(v, list): return "List"
    if isinstance(v, tuple): return "Tuple"
    if isinstance(v, set): return "Set"
    if isinstance(v, dict): return "Map"
    if isinstance(v, VeyraInstance): return v.klass.name
    if isinstance(v, VeyraClass): return "Class"
    if isinstance(v, Function): return "Function"
    if callable(v): return "Function"
    return type(v).__name__

def check_type(v, t, name="value"):
    aliases = {"Integer":"Int","Str":"String","Boolean":"Bool","Array":"List","Dict":"Map"}
    t = aliases.get(t, t)
    if t in ("Any", None): return
    if t == "Number" and type_of(v) in ("Int","Float"): return
    if type_of(v) != t:
        raise VeyraError(f"{name} expected {t}, got {type_of(v)}")

class Signal(Exception):
    def __init__(self, kind, value=None):
        self.kind = kind; self.value = value

class VeyraThrown(Exception):
    def __init__(self, value):
        self.value = value
        super().__init__(format_value(value))

class BoundMethod:
    def __init__(self, fn, instance):
        self.fn = fn; self.instance = instance
    def __call__(self, *args):
        return self.fn(self.instance, *args)

class VeyraInstance:
    def __init__(self, klass):
        self.klass = klass; self.fields = {}
    def get(self, name):
        if name in self.fields: return self.fields[name]
        if name in self.klass.methods: return BoundMethod(self.klass.methods[name], self)
        raise VeyraError(f"undefined member '{name}' on {self.klass.name}")
    def set(self, name, value):
        self.fields[name] = value
    def __repr__(self):
        return f"<{self.klass.name} instance>"

class VeyraClass:
    def __init__(self, name, methods):
        self.name = name; self.methods = methods
    def __call__(self, *args):
        inst = VeyraInstance(self)
        init = self.methods.get("init")
        if init: BoundMethod(init, inst)(*args)
        elif args: raise VeyraError(f"{self.name} expects 0 arguments")
        return inst
    def __repr__(self):
        return f"<class {self.name}>"

class Function:
    def __init__(self, node, closure, rt):
        self.node = node; self.closure = closure; self.rt = rt
    def __call__(self, *args):
        if len(args) != len(self.node.params):
            raise VeyraError(f"{self.node.name} expects {len(self.node.params)} arguments, got {len(args)}")
        e = Env(self.closure)
        for (name, typ), value in zip(self.node.params, args):
            if typ: check_type(value, typ, name)
            e.define(name, value, type_name=typ)
        try:
            self.rt.exec_block(self.node.body, e)
        except Signal as s:
            if s.kind == "return":
                if self.node.return_type and self.node.return_type != "Void":
                    check_type(s.value, self.node.return_type, "return value")
                return s.value
            raise
        if self.node.return_type and self.node.return_type != "Void":
            check_type(None, self.node.return_type, "return value")
        return None
    def __repr__(self): return f"<fn {self.node.name}>"

def format_value(v):
    if v is None: return "null"
    if v is True: return "true"
    if v is False: return "false"
    if isinstance(v, list): return "[" + ", ".join(format_value(x) for x in v) + "]"
    if isinstance(v, tuple): return "(" + ", ".join(format_value(x) for x in v) + ")"
    if isinstance(v, set): return "{" + ", ".join(format_value(x) for x in v) + "}"
    if isinstance(v, dict): return "{" + ", ".join(f"{format_value(k)}: {format_value(x)}" for k, x in v.items()) + "}"
    return str(v)

class Runtime:
    def __init__(self, filename="<input>", global_env=None):
        self.filename = filename
        if global_env is None:
            builtins_env = Env()
            self.globals = Env(builtins_env)
            self.install_builtins(builtins_env)
        else:
            self.globals = global_env
    def install_builtins(self, target):
        def p(*xs): print(*[format_value(x) for x in xs])
        def vrange(*xs): return list(range(*[int(x) for x in xs]))
        def vassert(cond, msg="assertion failed"):
            if not cond: raise VeyraError(str(msg))
            return True
        def push(a, x): a.append(x); return a
        def pop(a): return a.pop()
        def read(path):
            with open(path, "r", encoding="utf-8") as f: return f.read()
        def write(path, data):
            with open(path, "w", encoding="utf-8") as f: f.write(str(data))
            return None
        def py_import(name):
            try: return importlib.import_module(str(name))
            except Exception as ex: raise VeyraError(f"python import {name}: {ex}")
        def parallel_map(fn, items, workers=None):
            with ThreadPoolExecutor(max_workers=None if workers is None else int(workers)) as pool:
                return list(pool.map(fn, list(items)))
        _task_pool = ThreadPoolExecutor()
        def spawn(fn, *args):
            if not callable(fn): raise VeyraError("spawn expects a function")
            return _task_pool.submit(fn, *args)
        def await_task(task):
            if not isinstance(task, Future): raise VeyraError("await_task expects a task")
            return task.result()
        def task_done(task):
            if not isinstance(task, Future): raise VeyraError("task_done expects a task")
            return task.done()
        def shell(command):
            r = subprocess.run(str(command), shell=True, text=True, capture_output=True)
            return {"code":r.returncode,"out":r.stdout,"err":r.stderr}
        def http_get(url):
            with urllib.request.urlopen(str(url), timeout=30) as r:
                return r.read().decode("utf-8")
        def db_open(path):
            return sqlite3.connect(str(path))
        builtins = {
            "print":p,"len":len,"str":lambda x:format_value(x),"int":int,"float":float,
            "bool":bool,"type":type_of,"range":vrange,"assert":vassert,"push":push,"pop":pop,
            "keys":lambda d:list(d.keys()),"values":lambda d:list(d.values()),"input":input,
            "clock":time.time,"sqrt":math.sqrt,"abs":abs,"min":min,"max":max,"round":round,
            "tuple":tuple,"set":set,"list":list,"map":dict,"sorted":sorted,"sum":sum,
            "read":read,"write":write,"json":lambda x:json.dumps(x, ensure_ascii=False),
            "json_parse":json.loads,"regex":lambda pattern,text: re.findall(str(pattern),str(text)),
            "py_import":py_import,
            "py_call":lambda fn,args=None,kwargs=None: fn(*(args or []), **(kwargs or {})),
            "py_get":lambda obj,name:getattr(obj,str(name)),
            "py_set":lambda obj,name,value:(setattr(obj,str(name),value) or value),
            "py_dir":lambda obj:list(dir(obj)),
            "parallel_map":parallel_map,"spawn":spawn,"await_task":await_task,"task_done":task_done,
            "shell":shell,"http_get":http_get,
            "db_open":db_open,"random":random.random,"randint":random.randint,
            "path_exists":lambda p:Path(str(p)).exists(),"cwd":lambda:os.getcwd(),
            "env":lambda k:os.environ.get(str(k))
        }
        for n, v in builtins.items(): target.define(n, v, const=True)
    def eval(self, e, env):
        if isinstance(e, Literal): return e.value
        if isinstance(e, Var): return env.get(e.name)
        if isinstance(e, ListExpr): return [self.eval(x, env) for x in e.items]
        if isinstance(e, TupleExpr): return tuple(self.eval(x, env) for x in e.items)
        if isinstance(e, SetExpr): return set(self.eval(x, env) for x in e.items)
        if isinstance(e, MapExpr): return {self.eval(k, env): self.eval(v, env) for k, v in e.pairs}
        if isinstance(e, Index): return self.eval(e.obj, env)[self.eval(e.key, env)]
        if isinstance(e, Member):
            obj = self.eval(e.obj, env)
            if isinstance(obj, VeyraInstance): return obj.get(e.name)
            if isinstance(obj, dict) and e.name in obj: return obj[e.name]
            try: return getattr(obj, e.name)
            except AttributeError: raise VeyraError(f"undefined member '{e.name}'")
        if isinstance(e, Unary):
            r = self.eval(e.right, env)
            if e.op in ("!","not"): return not bool(r)
            if e.op == "-": return -r
            if e.op == "+": return +r
        if isinstance(e, Binary):
            if e.op in ("&&","and"):
                l = self.eval(e.left, env); return self.eval(e.right, env) if l else l
            if e.op in ("||","or"):
                l = self.eval(e.left, env); return l if l else self.eval(e.right, env)
            a, b = self.eval(e.left, env), self.eval(e.right, env)
            ops = {
                "+":lambda:a+b,"-":lambda:a-b,"*":lambda:a*b,"/":lambda:a/b,"%":lambda:a%b,
                "**":lambda:a**b,"==":lambda:a==b,"!=":lambda:a!=b,"<":lambda:a<b,
                "<=":lambda:a<=b,">":lambda:a>b,">=":lambda:a>=b
            }
            try: return ops[e.op]()
            except Exception as ex: raise VeyraError(f"operator {e.op}: {ex}")
        if isinstance(e, Call):
            fn = self.eval(e.fn, env); args = [self.eval(a, env) for a in e.args]
            if not callable(fn): raise VeyraError(f"{format_value(fn)} is not callable")
            try: return fn(*args)
            except VeyraError: raise
            except Exception as ex: raise VeyraError(str(ex))
        raise VeyraError(f"cannot evaluate {type(e).__name__}")
    def exec_block(self, b, env):
        value = None
        for s in b.body:
            value = self.exec(s, env)
        return value
    def exec(self, s, env):
        if isinstance(s, Block): return self.exec_block(s, Env(env))
        if isinstance(s, ExprStmt): return self.eval(s.expr, env)
        if isinstance(s, LetStmt):
            v = self.eval(s.value, env)
            if s.type_name: check_type(v, s.type_name, s.name)
            env.define(s.name, v, s.const, s.type_name); return None
        if isinstance(s, AssignStmt):
            if isinstance(s.target, Var):
                old = env.get(s.target.name); nv = self.eval(s.value, env)
                if s.op == "+=": nv = old + nv
                elif s.op == "-=": nv = old - nv
                elif s.op == "*=": nv = old * nv
                elif s.op == "/=": nv = old / nv
                env.set(s.target.name, nv); return None
            if isinstance(s.target, Index):
                obj = self.eval(s.target.obj, env); key = self.eval(s.target.key, env); nv = self.eval(s.value, env)
                if s.op != "=":
                    old = obj[key]
                    if s.op == "+=": nv = old + nv
                    elif s.op == "-=": nv = old - nv
                    elif s.op == "*=": nv = old * nv
                    elif s.op == "/=": nv = old / nv
                obj[key] = nv; return None
            if isinstance(s.target, Member):
                obj = self.eval(s.target.obj, env); nv = self.eval(s.value, env)
                if isinstance(obj, VeyraInstance):
                    old = obj.fields.get(s.target.name)
                    if s.op == "+=": nv = old + nv
                    elif s.op == "-=": nv = old - nv
                    elif s.op == "*=": nv = old * nv
                    elif s.op == "/=": nv = old / nv
                    obj.set(s.target.name, nv); return None
                if isinstance(obj, dict):
                    old = obj.get(s.target.name)
                    if s.op == "+=": nv = old + nv
                    elif s.op == "-=": nv = old - nv
                    elif s.op == "*=": nv = old * nv
                    elif s.op == "/=": nv = old / nv
                    obj[s.target.name] = nv; return None
                if s.op != "=": raise VeyraError("compound member assignment unsupported for this object")
                setattr(obj, s.target.name, nv); return None
            raise VeyraError("invalid assignment target")
        if isinstance(s, IfStmt):
            if self.eval(s.cond, env): return self.exec(s.then, env)
            if s.otherwise: return self.exec(s.otherwise, env)
            return None
        if isinstance(s, WhileStmt):
            while self.eval(s.cond, env):
                try: self.exec(s.body, env)
                except Signal as q:
                    if q.kind == "break": break
                    if q.kind == "continue": continue
                    raise
            return None
        if isinstance(s, ForStmt):
            for item in self.eval(s.iterable, env):
                loop = Env(env); loop.define(s.name, item)
                try: self.exec_block(s.body, loop)
                except Signal as q:
                    if q.kind == "break": break
                    if q.kind == "continue": continue
                    raise
            return None
        if isinstance(s, FunctionStmt):
            env.define(s.name, Function(s, env, self), const=True); return None
        if isinstance(s, ClassStmt):
            methods = {}
            for item in s.body.body:
                if not isinstance(item, FunctionStmt):
                    raise VeyraError("class bodies currently contain methods only")
                methods[item.name] = Function(item, env, self)
            env.define(s.name, VeyraClass(s.name, methods), const=True); return None
        if isinstance(s, TryStmt):
            try:
                return self.exec_block(s.body, Env(env))
            except VeyraThrown as ex:
                catch_env = Env(env); catch_env.define(s.error_name, ex.value)
                return self.exec_block(s.handler, catch_env)
            except VeyraError as ex:
                catch_env = Env(env); catch_env.define(s.error_name, str(ex))
                return self.exec_block(s.handler, catch_env)
        if isinstance(s, ThrowStmt): raise VeyraThrown(self.eval(s.value, env))
        if isinstance(s, ReturnStmt): raise Signal("return", None if s.value is None else self.eval(s.value, env))
        if isinstance(s, FlowStmt): raise Signal(s.kind)
        if isinstance(s, UseStmt):
            path = s.path
            if not os.path.isabs(path):
                path = os.path.join(os.path.dirname(os.path.abspath(self.filename)), path)
            run_file(path, env); return None
        raise VeyraError(f"cannot execute {type(s).__name__}")

def parse(source, filename="<input>"):
    return Parser(Lexer(source, filename).tokens(), filename).program()

BUILTIN_NAMES = {
    "print","len","str","int","float","bool","type","range","assert","push","pop","keys","values",
    "input","clock","sqrt","abs","min","max","round","tuple","set","list","map","sorted","sum",
    "read","write","json","json_parse","regex","py_import","py_call","py_get","py_set","py_dir",
    "parallel_map","spawn","await_task","task_done","shell","http_get","db_open","random","randint",
    "path_exists","cwd","env"
}

def semantic_check(source, filename="<input>"):
    tree = parse(source, filename)
    errors = []

    def check_expr(e, scopes):
        if isinstance(e, Literal): return
        if isinstance(e, Var):
            if not any(e.name in s for s in reversed(scopes)) and e.name not in BUILTIN_NAMES:
                errors.append(f"undefined name '{e.name}'")
            return
        if isinstance(e, (ListExpr, TupleExpr, SetExpr)):
            for x in e.items: check_expr(x, scopes)
            return
        if isinstance(e, MapExpr):
            for k,v in e.pairs: check_expr(k, scopes); check_expr(v, scopes)
            return
        if isinstance(e, (Unary,)):
            check_expr(e.right, scopes); return
        if isinstance(e, Binary):
            check_expr(e.left, scopes); check_expr(e.right, scopes); return
        if isinstance(e, Call):
            check_expr(e.fn, scopes)
            for a in e.args: check_expr(a, scopes)
            return
        if isinstance(e, Index):
            check_expr(e.obj, scopes); check_expr(e.key, scopes); return
        if isinstance(e, Member):
            check_expr(e.obj, scopes); return

    def check_block(block, scopes, predeclare=True):
        local = set()
        scopes = scopes + [local]
        if predeclare:
            for s in block.body:
                if isinstance(s, (FunctionStmt, ClassStmt)): local.add(s.name)
        for s in block.body:
            if isinstance(s, LetStmt):
                check_expr(s.value, scopes)
                if s.name in local: errors.append(f"duplicate declaration '{s.name}'")
                local.add(s.name)
            elif isinstance(s, AssignStmt):
                check_expr(s.target, scopes); check_expr(s.value, scopes)
            elif isinstance(s, ExprStmt): check_expr(s.expr, scopes)
            elif isinstance(s, IfStmt):
                check_expr(s.cond, scopes)
                check_block(s.then, scopes)
                if s.otherwise:
                    if isinstance(s.otherwise, Block): check_block(s.otherwise, scopes)
                    else: check_block(Block([s.otherwise]), scopes)
            elif isinstance(s, WhileStmt):
                check_expr(s.cond, scopes); check_block(s.body, scopes)
            elif isinstance(s, ForStmt):
                check_expr(s.iterable, scopes)
                check_block(s.body, scopes + [{s.name}], predeclare=True)
            elif isinstance(s, FunctionStmt):
                fn_scope = {n for n,_ in s.params}
                check_block(s.body, scopes + [fn_scope], predeclare=True)
            elif isinstance(s, ClassStmt):
                for item in s.body.body:
                    if isinstance(item, FunctionStmt):
                        fn_scope = {n for n,_ in item.params}
                        check_block(item.body, scopes + [fn_scope], predeclare=True)
            elif isinstance(s, ReturnStmt):
                if s.value is not None: check_expr(s.value, scopes)
            elif isinstance(s, TryStmt):
                check_block(s.body, scopes)
                check_block(s.handler, scopes + [{s.error_name}], predeclare=True)
            elif isinstance(s, ThrowStmt): check_expr(s.value, scopes)
            elif isinstance(s, UseStmt): pass
        return scopes

    root = set()
    for s in tree.body:
        if isinstance(s, (FunctionStmt, ClassStmt)): root.add(s.name)
    check_block(tree, [root], predeclare=False)
    if errors:
        unique = list(dict.fromkeys(errors))
        raise VeyraError(f"{filename}: semantic check failed: " + "; ".join(unique))
    return tree

def run_source(source, filename="<input>", env=None):
    rt = Runtime(filename, env)
    tree = parse(source, filename)
    rt.exec_block(tree, rt.globals)
    return rt.globals

def run_file(path, env=None):
    if not os.path.exists(path): raise VeyraError(f"module/file not found: {path}")
    with open(path, "r", encoding="utf-8") as f: source = f.read()
    return run_source(source, os.path.abspath(path), env)

def repl():
    print(f"Veyra {VERSION} REPL — :quit to exit")
    env = Runtime().globals
    buf = ""
    while True:
        try:
            line = input("... " if buf else "vy> ")
            if line.strip() in (":q",":quit","exit"): break
            buf += line + "\n"
            if buf.count("{") > buf.count("}"): continue
            try:
                tree = parse(buf, "<repl>"); rt = Runtime("<repl>", env)
                for s in tree.body:
                    value = rt.exec(s, env)
                    if isinstance(s, ExprStmt) and value is not None: print(format_value(value))
            finally:
                buf = ""
        except (VeyraError, Signal) as e:
            print(f"error: {e}", file=sys.stderr); buf = ""
        except (EOFError, KeyboardInterrupt):
            print(); break

def main(argv=None):
    ap = argparse.ArgumentParser(prog="veyra", description="Veyra programming language")
    ap.add_argument("file", nargs="?")
    ap.add_argument("--version", action="store_true")
    ap.add_argument("--check", action="store_true", help="parse without running")
    ap.add_argument("--tokens", action="store_true", help="print lexer tokens")
    ns = ap.parse_args(argv)
    if ns.version:
        print(f"Veyra {VERSION}"); return 0
    if not ns.file:
        repl(); return 0
    try:
        with open(ns.file, "r", encoding="utf-8") as f: source = f.read()
        if ns.tokens:
            for t in Lexer(source, ns.file).tokens(): print(t)
            return 0
        tree = parse(source, ns.file)
        if ns.check:
            print(f"{ns.file}: OK"); return 0
        rt = Runtime(os.path.abspath(ns.file))
        rt.exec_block(tree, rt.globals)
        return 0
    except (OSError, VeyraError) as e:
        print(f"veyra: error: {e}", file=sys.stderr); return 1

if __name__ == "__main__":
    raise SystemExit(main())
