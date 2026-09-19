#!/usr/bin/env python3
from __future__ import annotations
import argparse, hashlib, json, os, platform, shutil, subprocess, sys, time
from pathlib import Path
import veyra as legacy

VERSION = "2.0.0-horizon"
ROOT = Path(__file__).resolve().parent
NATIVE = ROOT / "bin" / "veyra-native.exe"

def native(args, check=False):
    if not NATIVE.exists():
        raise legacy.VeyraError(f"native engine not installed: {NATIVE}")
    return subprocess.run([str(NATIVE), *args], check=check)

def fmt_text(src: str) -> str:
    out=[]; indent=0
    for raw in src.splitlines():
        line=raw.strip()
        if not line: out.append(""); continue
        if line.startswith("}"): indent=max(0,indent-1)
        out.append("    "*indent+line)
        if line.endswith("{"): indent+=1
    return "\n".join(out).rstrip()+"\n"

def cmd_new(name):
    p=Path(name)
    if p.exists(): raise legacy.VeyraError(f"{p} already exists")
    (p/"src").mkdir(parents=True); (p/"tests").mkdir(); (p/"vendor").mkdir()
    (p/"veyra.json").write_text(json.dumps({
        "name":p.name,"version":"0.1.0","entry":"src/main.vy",
        "edition":"2026","engine":VERSION
    },indent=2)+"\n",encoding="utf-8")
    (p/"src"/"main.vy").write_text(
        'const language: String = "Veyra";\nprint("Hello from", language);\n',encoding="utf-8")
    (p/".gitignore").write_text("dist/\n.veyra/\n*.vyc\n",encoding="utf-8")
    print(f"Created {p}"); print(f"  cd {p}"); print("  veyra run")

def manifest(cwd=Path.cwd()):
    p=cwd/"veyra.json"
    if not p.exists(): raise legacy.VeyraError("veyra.json not found")
    return json.loads(p.read_text(encoding="utf-8")),p

def resolve_file(arg=None):
    if arg: return Path(arg)
    m,p=manifest(); return p.parent/m.get("entry","src/main.vy")

def cmd_check(path):
    source=Path(path).read_text(encoding="utf-8")
    legacy.parse(source,str(path))
    print(f"{path}: OK")

def cmd_fmt(path,check=False):
    p=Path(path); before=p.read_text(encoding="utf-8"); after=fmt_text(before)
    if check:
        if before!=after: raise legacy.VeyraError(f"{p} needs formatting")
        print(f"{p}: formatted")
    else:
        p.write_text(after,encoding="utf-8"); print(f"formatted {p}")

def cmd_test():
    tests=sorted(Path("tests").glob("*.vy"))
    if not tests:
        py=ROOT/"tests"/"test_veyra.py"
        if py.exists():
            r=subprocess.run([sys.executable,"-m","unittest","discover","-s",str(ROOT/"tests"),"-v"])
            raise SystemExit(r.returncode)
        print("No Veyra tests found"); return
    passed=0
    for p in tests:
        try: legacy.run_file(str(p)); print(f"PASS {p}"); passed+=1
        except Exception as e: print(f"FAIL {p}: {e}")
    print(f"{passed}/{len(tests)} passed")
    if passed!=len(tests): raise SystemExit(1)

def cmd_build(native_mode=False):
    m,mp=manifest(); root=mp.parent; entry=root/m.get("entry","src/main.vy")
    cmd_check(entry)
    dist=root/"dist"; dist.mkdir(exist_ok=True)
    name=m.get("name",root.name); ver=m.get("version","0.0.0")
    pkg=dist/f"{name}-{ver}"; 
    if pkg.exists(): shutil.rmtree(pkg)
    pkg.mkdir()
    shutil.copytree(root/"src",pkg/"src")
    shutil.copy2(mp,pkg/"veyra.json")
    runbat=pkg/"run.bat"
    runbat.write_text(f'@echo off\nveyra "{m.get("entry","src/main.vy")}" %*\n',encoding="ascii")
    digest=hashlib.sha256(entry.read_bytes()).hexdigest()
    (pkg/"BUILD.json").write_text(json.dumps({
        "veyra":VERSION,"built":time.strftime("%Y-%m-%dT%H:%M:%SZ",time.gmtime()),
        "entry_sha256":digest,"native_requested":native_mode
    },indent=2)+"\n",encoding="utf-8")
    if native_mode:
        r=native(["check",str(entry)])
        if r.returncode!=0: print("Native compiler does not yet cover this source; compatibility build preserved.")
    print(f"Built {pkg}")

def cmd_doctor():
    checks=[]
    checks.append(("Python",sys.version.split()[0],True))
    checks.append(("Legacy runtime",legacy.VERSION,True))
    checks.append(("Native VM",str(NATIVE),NATIVE.exists()))
    if NATIVE.exists():
        r=subprocess.run([str(NATIVE),"doctor"],capture_output=True,text=True)
        checks.append(("Native self-test",r.stdout.strip().replace("\n"," | "),r.returncode==0))
    checks.append(("Platform",f"{platform.system()} {platform.machine()}",True))
    print(f"Veyra {VERSION} doctor")
    for n,v,ok in checks: print(f"[{'OK' if ok else 'FAIL'}] {n}: {v}")
    if not all(x[2] for x in checks): raise SystemExit(1)

def cmd_info():
    print(f"Veyra {VERSION}")
    print("architecture: compatibility runtime + native Rust bytecode VM")
    print(f"root: {ROOT}")
    print(f"native: {NATIVE if NATIVE.exists() else 'not installed'}")
    print("edition: 2026")

def main():
    a=sys.argv[1:]
    try:
        if not a: return legacy.repl()
        if a[0] in ("--version","version"): print(f"Veyra {VERSION}"); return
        if a[0]=="doctor": return cmd_doctor()
        if a[0]=="info": return cmd_info()
        if a[0]=="new":
            if len(a)<2: raise legacy.VeyraError("usage: veyra new <name>")
            return cmd_new(a[1])
        if a[0]=="run":
            p=resolve_file(a[1] if len(a)>1 else None); return legacy.run_file(str(p))
        if a[0]=="check":
            p=resolve_file(a[1] if len(a)>1 else None); return cmd_check(p)
        if a[0]=="fmt":
            p=resolve_file(a[1] if len(a)>1 else None); return cmd_fmt(p,"--check" in a)
        if a[0]=="test": return cmd_test()
        if a[0]=="build": return cmd_build("--native" in a)
        if a[0]=="clean":
            d=Path("dist")
            if d.exists(): shutil.rmtree(d)
            print("clean"); return
        if a[0]=="native":
            if len(a)==1: return native(["repl"])
            raise SystemExit(native(a[1:]).returncode)
        if a[0].endswith(".vy"): return legacy.run_file(a[0])
        print("Veyra commands: run, check, fmt, test, build, clean, new, native, doctor, info, --version")
    except (legacy.VeyraError,OSError,json.JSONDecodeError) as e:
        print(f"veyra: error: {e}",file=sys.stderr); raise SystemExit(1)

if __name__=="__main__": main()
