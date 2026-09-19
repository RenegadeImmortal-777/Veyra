#!/usr/bin/env python3
from __future__ import annotations
import argparse, hashlib, json, os, platform, shutil, subprocess, sys, time
from pathlib import Path
import veyra as legacy

VERSION = "3.0.0-convergence"
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
        "edition":"2026","engine":VERSION,"dependencies":{}
    },indent=2)+"\n",encoding="utf-8")
    (p/"src"/"main.vy").write_text(
        'const language: String = "Veyra";\nprint("Hello from", language);\n',encoding="utf-8")
    (p/".gitignore").write_text("dist/\n.veyra/\n*.vyc\n",encoding="utf-8")
    print(f"Created {p}"); print(f"  cd {p}"); print("  veyra run")

def find_project(start=None):
    p=Path(start or Path.cwd()).resolve()
    for candidate in [p,*p.parents]:
        m=candidate/"veyra.json"
        if m.exists(): return candidate,m
    raise legacy.VeyraError("veyra.json not found")

def manifest(cwd=Path.cwd()):
    root,p=find_project(cwd)
    return json.loads(p.read_text(encoding="utf-8")),p

def activate_env(start=None):
    try: root,_=find_project(start)
    except legacy.VeyraError: return None
    site=root/".veyra"/"venv"/"Lib"/"site-packages"
    if site.exists() and str(site) not in sys.path: sys.path.insert(0,str(site))
    return root

def ensure_env():
    root,_=find_project()
    envdir=root/".veyra"/"venv"
    py=envdir/"Scripts"/"python.exe"
    if not py.exists():
        print("Creating Veyra project environment...")
        subprocess.run([sys.executable,"-m","venv",str(envdir)],check=True)
    return root,py

def cmd_add(package):
    root,py=ensure_env()
    subprocess.run([str(py),"-m","pip","install",package],check=True)
    m,mp=manifest(root)
    probe=subprocess.run([str(py),"-m","pip","show",package],capture_output=True,text=True)
    version="*"
    for line in probe.stdout.splitlines():
        if line.startswith("Version:"): version=line.split(":",1)[1].strip()
    m.setdefault("dependencies",{})[package]=version
    mp.write_text(json.dumps(m,indent=2)+"\n",encoding="utf-8")
    print(f"Added {package} {version}")

def cmd_remove(package):
    root,py=ensure_env()
    subprocess.run([str(py),"-m","pip","uninstall","-y",package],check=True)
    m,mp=manifest(root); m.setdefault("dependencies",{}).pop(package,None)
    mp.write_text(json.dumps(m,indent=2)+"\n",encoding="utf-8")
    print(f"Removed {package}")

def cmd_packages():
    root,py=ensure_env()
    subprocess.run([str(py),"-m","pip","list"])
    m,_=manifest(root)
    print("Manifest dependencies:",json.dumps(m.get("dependencies",{}),indent=2))

def cmd_lock():
    root,py=ensure_env()
    r=subprocess.run([str(py),"-m","pip","freeze"],capture_output=True,text=True,check=True)
    lock=root/"veyra.lock"
    lock.write_text(r.stdout,encoding="utf-8")
    print(f"Locked dependencies to {lock}")

def cmd_install():
    root,py=ensure_env()
    lock=root/"veyra.lock"
    m,_=manifest(root)
    if lock.exists():
        subprocess.run([str(py),"-m","pip","install","-r",str(lock)],check=True)
    else:
        for name,ver in m.get("dependencies",{}).items():
            spec=name if ver in ("","*") else f"{name}=={ver}"
            subprocess.run([str(py),"-m","pip","install",spec],check=True)
    print("Dependencies synchronized")

def cmd_audit():
    root,py=ensure_env()
    r=subprocess.run([str(py),"-m","pip","check"])
    if r.returncode: raise SystemExit(r.returncode)
    print("Dependency graph: OK")

def cmd_lint(path):
    source=Path(path).read_text(encoding="utf-8")
    legacy.parse(source,str(path))
    issues=[]
    for n,line in enumerate(source.splitlines(),1):
        if "\t" in line: issues.append(f"{path}:{n}: tab indentation")
        if len(line)>120: issues.append(f"{path}:{n}: line exceeds 120 characters")
        if line.rstrip()!=line: issues.append(f"{path}:{n}: trailing whitespace")
    if issues:
        print("\n".join(issues)); raise SystemExit(1)
    print(f"{path}: lint OK")

def cmd_bench(path,iterations=10):
    activate_env(Path(path).parent)
    times=[]
    for _ in range(int(iterations)):
        t=time.perf_counter(); legacy.run_file(str(path)); times.append(time.perf_counter()-t)
    print(f"{path}: {len(times)} runs | best={min(times)*1000:.3f}ms | mean={sum(times)/len(times)*1000:.3f}ms")

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
            p=resolve_file(a[1] if len(a)>1 else None); activate_env(p.parent); return legacy.run_file(str(p))
        if a[0]=="check":
            p=resolve_file(a[1] if len(a)>1 else None); return cmd_check(p)
        if a[0]=="fmt":
            p=resolve_file(a[1] if len(a)>1 else None); return cmd_fmt(p,"--check" in a)
        if a[0]=="test": activate_env(); return cmd_test()
        if a[0]=="add":
            if len(a)<2: raise legacy.VeyraError("usage: veyra add <python-package>")
            return cmd_add(a[1])
        if a[0]=="remove":
            if len(a)<2: raise legacy.VeyraError("usage: veyra remove <python-package>")
            return cmd_remove(a[1])
        if a[0] in ("packages","deps"): return cmd_packages()
        if a[0]=="lock": return cmd_lock()
        if a[0]=="install": return cmd_install()
        if a[0]=="audit": return cmd_audit()
        if a[0]=="lint":
            p=resolve_file(a[1] if len(a)>1 else None); return cmd_lint(p)
        if a[0]=="bench":
            p=resolve_file(a[1] if len(a)>1 else None)
            n=a[2] if len(a)>2 else 10
            return cmd_bench(p,n)
        if a[0]=="env":
            root,py=ensure_env(); print(f"project={root}\npython={py}"); return
        if a[0]=="build": activate_env(); return cmd_build("--native" in a)
        if a[0]=="clean":
            d=Path("dist")
            if d.exists(): shutil.rmtree(d)
            print("clean"); return
        if a[0]=="native":
            if len(a)==1: return native(["repl"])
            raise SystemExit(native(a[1:]).returncode)
        if a[0].endswith(".vy"): return legacy.run_file(a[0])
        print("Veyra commands: run, check, fmt, lint, test, bench, build, clean, new, add, remove, packages, lock, install, audit, env, native, doctor, info, --version")
    except (legacy.VeyraError,OSError,json.JSONDecodeError) as e:
        print(f"veyra: error: {e}",file=sys.stderr); raise SystemExit(1)

if __name__=="__main__": main()
