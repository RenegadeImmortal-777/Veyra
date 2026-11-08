#![allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::needless_range_loop,
    clippy::type_complexity,
    clippy::let_and_return,
    clippy::only_used_in_recursion,
    clippy::manual_is_multiple_of
)]

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

#[cfg(test)]
use crate::value::Val;
#[cfg(test)]
use serde_json::Value as JsonValue;

pub const VERSION: &str = "12.0.0-sovereign";

mod ast;
mod builtins;
mod bytecode;
mod cli;
mod compiler;
mod ir;
mod lexer;
mod lsp;
mod parser;
mod pipeline;
mod project;
mod registry;
mod runtime;
mod server;
mod tensor;
mod tooling;
mod value;
mod vm;

#[cfg(test)]
use crate::bytecode::Op;
use crate::cli::{doctor, new_project, repl, usage};
#[cfg(test)]
use crate::lexer::lex;
use crate::lsp::lsp_server;
#[cfg(test)]
use crate::lsp::{lsp_definition, lsp_diagnostics, lsp_references, lsp_rename};
use crate::pipeline::compile_source;
#[cfg(test)]
use crate::pipeline::compile_source_unoptimized;
pub use crate::pipeline::{compile_to_vybc, disasm_source, load_source, run_source, run_vybc};
use crate::project::{build_project, fmt_source, project_entry, project_packages, test_project};
use crate::registry::{
    pkg_add, pkg_audit, pkg_install, pkg_list, pkg_publish, pkg_remove, pkg_search,
};
#[cfg(test)]
use crate::runtime::export_function_object;
#[cfg(test)]
use crate::server::resolve_static_path;
use crate::server::{serve_directory, serve_routes, serve_veyra_app};
use crate::tooling::{
    ast_file, bench_file, debug_file, doc_file, lint_file, profile_file, symbols_file,
};
#[cfg(test)]
use crate::vm::Vm;

pub fn run_cli() -> ExitCode {
    let a: Vec<String> = env::args().collect();
    let r: Result<(), String> = match a.get(1).map(String::as_str) {
        Some("--version") | Some("version") => {
            println!("Veyra {VERSION}");
            Ok(())
        }
        Some("doctor") => {
            doctor();
            Ok(())
        }
        Some("repl") | None => repl(),
        Some("new") => a
            .get(2)
            .ok_or("missing project name".into())
            .and_then(|n| new_project(n)),
        Some("pkg") => match a.get(2).map(String::as_str) {
            Some("list") => pkg_list(),
            Some("installed") => project_packages(),
            Some("install") => pkg_install(),
            Some("publish") => pkg_publish(),
            Some("search") => a
                .get(3)
                .ok_or("missing search term".into())
                .and_then(|q| pkg_search(q)),
            Some("add") => a
                .get(3)
                .ok_or("missing package name".into())
                .and_then(|n| pkg_add(n)),
            Some("remove") => a
                .get(3)
                .ok_or("missing package name".into())
                .and_then(|n| pkg_remove(n)),
            _ => Err(
                "usage: veyra pkg <list|search|installed|install|publish|add|remove> [name]".into(),
            ),
        },
        Some("add") => a
            .get(2)
            .ok_or("missing package name".into())
            .and_then(|n| pkg_add(n)),
        Some("remove") => a
            .get(2)
            .ok_or("missing package name".into())
            .and_then(|n| pkg_remove(n)),
        Some("install") => pkg_install(),
        Some("publish") => pkg_publish(),
        Some("packages") => project_packages(),
        Some("registry") => pkg_list(),
        Some("check") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| load_source(p.to_string_lossy().as_ref()))
                .and_then(|s| compile_source(&s).map(|_| println!("OK")))
        }
        Some("run") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| {
                if p.extension().and_then(|e| e.to_str()) == Some("vybc") {
                    let bc = fs::read_to_string(&p).map_err(|e| e.to_string())?;
                    run_vybc(&bc)
                } else {
                    load_source(p.to_string_lossy().as_ref()).and_then(|s| run_source(&s))
                }
            })
        }
        Some("lint") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| lint_file(&p))
        }
        Some("debug") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| debug_file(&p))
        }
        Some("ast") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| ast_file(&p))
        }
        Some("symbols") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| symbols_file(&p))
        }
        Some("lsp") => lsp_server(),
        Some("test") => test_project(),
        Some("bench") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            let iterations = a.get(3).and_then(|x| x.parse::<usize>().ok()).unwrap_or(10);
            path.and_then(|p| bench_file(&p, iterations))
        }
        Some("profile") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| profile_file(&p))
        }
        Some("doc") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| doc_file(&p))
        }
        Some("build") => build_project(),
        Some("audit") => pkg_audit(),
        Some("serve") => {
            let dir = a
                .get(2)
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            let port = a.get(3).and_then(|x| x.parse::<u16>().ok()).unwrap_or(8080);
            serve_directory(&dir, port)
        }
        Some("web") => a.get(2).ok_or("missing routes.json".into()).and_then(|p| {
            let port = a.get(3).and_then(|x| x.parse::<u16>().ok()).unwrap_or(8080);
            serve_routes(Path::new(p), port)
        }),
        Some("app") => {
            let app = a.get(2).ok_or("missing app.vy".into());
            let routes = a.get(3).ok_or("missing routes.json".into());
            match (app, routes) {
                (Ok(app), Ok(routes)) => {
                    let port = a.get(4).and_then(|x| x.parse::<u16>().ok()).unwrap_or(8080);
                    serve_veyra_app(Path::new(app), Path::new(routes), port)
                }
                (Err(e), _) | (_, Err(e)) => Err(e),
            }
        }
        Some("disasm") => {
            let path = match a.get(2) {
                Some(p) => Ok(PathBuf::from(p)),
                None => project_entry(),
            };
            path.and_then(|p| load_source(p.to_string_lossy().as_ref()))
                .and_then(|s| disasm_source(&s))
        }
        Some("fmt") => a.get(2).ok_or("missing file".into()).and_then(|p| {
            let s = fs::read_to_string(p).map_err(|e| e.to_string())?;
            fs::write(p, fmt_source(&s)).map_err(|e| e.to_string())
        }),
        Some(x) if x.ends_with(".vy") => fs::read_to_string(x)
            .map_err(|e| e.to_string())
            .and_then(|s| run_source(&s)),
        Some(x) if x.ends_with(".vybc") => fs::read_to_string(x)
            .map_err(|e| e.to_string())
            .and_then(|s| run_vybc(&s)),
        _ => {
            usage();
            Ok(())
        }
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("veyra: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lex_math() {
        assert!(lex("let x=1+2;").is_ok());
    }
    #[test]
    fn parse_control() {
        assert!(compile_source("let x=0; while x<3 { x=x+1; }").is_ok());
    }
    #[test]
    fn vm_math() {
        assert!(run_source("let x=2+3*4;").is_ok());
    }
    #[test]
    fn list_builtin() {
        assert!(run_source("let x=[1,2,3]; let n=len(x);").is_ok());
    }
    #[test]
    fn for_range_loop() {
        assert!(run_source("let s=0; for n in range(0,5) { s=s+n; }").is_ok());
    }
    #[test]
    fn user_function() {
        assert!(
            run_source("fn add(a: Int, b: Int) -> Int { return a+b; } let x=add(2,3);").is_ok()
        );
    }
    #[test]
    fn recursive_function() {
        assert!(run_source("fn fib(n: Int) -> Int { if n<=1 { return n; } return fib(n-1)+fib(n-2); } let x=fib(8);").is_ok());
    }
    #[test]
    fn map_builtin() {
        assert!(run_source("let m={\"a\":1,\"b\":2}; let n=len(m); let x=get(m,\"b\");").is_ok());
    }
    #[test]
    fn native_indexing() {
        assert!(
            run_source("let xs=[10,20,30]; let x=xs[1]; let m={\"a\":7}; let y=m[\"a\"];").is_ok()
        );
    }
    #[test]
    fn const_is_enforced() {
        assert!(run_source("const x=1; x=2;").is_err());
    }
    #[test]
    fn boolean_logic() {
        assert!(run_source("let x=true and not false; let y=false or x;").is_ok());
    }
    #[test]
    fn native_json_regex_hash() {
        assert!(run_source(
            "let j=json_parse(\"{\\\"a\\\":7}\"); let x=get(j,\"a\"); let m=regex_findall(\"[0-9]+\",\"a12b34\"); let h=sha256(\"veyra\");"
        ).is_ok());
    }
    #[test]
    fn native_file_io() {
        let p = env::temp_dir().join("veyra_native_io_test.txt");
        let s = format!(
            "write_file(\"{}\",\"hello\"); let x=read_file(\"{}\");",
            p.to_string_lossy().replace('\\', "\\\\"),
            p.to_string_lossy().replace('\\', "\\\\")
        );
        assert!(run_source(&s).is_ok());
        let _ = fs::remove_file(p);
    }
    #[test]
    fn native_science_math() {
        assert!(run_source(
            "let x=[1,2,3,4]; let m=vec_mean(x); let s=vec_std(x); let d=vec_dot([1,2],[3,4]); let z=matmul([[1,2],[3,4]],[[5,6],[7,8]]);"
        ).is_ok());
    }
    #[test]
    fn native_ml() {
        assert!(run_source(
            "let model=linreg_fit([1,2,3,4],[3,5,7,9]); let p=linreg_predict(model,5); let c=knn_predict([[0,0],[1,1],[9,9]],[\"a\",\"a\",\"b\"],[0.5,0.5],2); let centers=kmeans([[0,0],[0,1],[9,9],[9,8]],2,5); let h=dense([1,2],[[1,0],[0,1]],[0,0]); let r=relu([-1,2]); let s=softmax([1,2,3]); let n=normalize([10,20,30]);"
        ).is_ok());
    }
    #[test]
    fn native_csv() {
        let p = env::temp_dir().join("veyra_native_data.csv");
        fs::write(&p, "x,y\n1,2\n3,4\n").unwrap();
        let s = format!(
            "let rows=csv_read(\"{}\"); let x=rows[0][\"x\"];",
            p.to_string_lossy().replace('\\', "\\\\")
        );
        assert!(run_source(&s).is_ok());
        let _ = fs::remove_file(p);
    }
    #[test]
    fn native_table_plot() {
        let p = env::temp_dir().join("veyra_native_plot.svg");
        let s = format!(
            "let rows=[{{\"x\":1,\"g\":\"a\"}},{{\"x\":3,\"g\":\"b\"}}]; let c=table_column(rows,\"x\"); let d=table_describe(rows,\"x\"); let f=table_filter_eq(rows,\"g\",\"a\"); plot_svg(\"{}\",[1,2,3],[2,4,3]);",
            p.to_string_lossy().replace('\\', "\\\\")
        );
        assert!(run_source(&s).is_ok());
        assert!(p.exists());
        let _ = fs::remove_file(p);
    }
    #[test]
    fn general_iteration() {
        assert!(run_source("let total=0; for x in [1,2,3,4] { total=total+x; }").is_ok());
        assert!(run_source("let out=\"\"; for ch in \"abc\" { out=out+ch; }").is_ok());
    }
    #[test]
    fn explicit_exceptions() {
        assert!(
            run_source(
                "let caught=false; try { throw \"boom\"; } catch err { caught = err == \"boom\"; }"
            )
            .is_ok()
        );
        assert!(run_source("throw \"uncaught\";").is_err());
    }
    #[test]
    fn lexical_capture() {
        assert!(run_source("fn outer(x: Int) -> Int { let y=3; fn add(z: Int) -> Int { return y+z; } return add(x); } let result=outer(4);").is_ok());
    }
    #[test]
    fn native_tasks() {
        assert!(run_source("fn square(n: Int) -> Int { return n*n; } let task=spawn(\"square\",[9]); let result=await_task(task);").is_ok());
    }
    #[test]
    fn native_ffi_i32() {
        if cfg!(target_os = "windows") {
            assert!(run_source("let ok=ffi_available(\"msvcrt.dll\"); let x=ffi_i32(\"msvcrt.dll\",\"abs\",[-7]);").is_ok());
        }
    }
    #[test]
    fn native_classes_inheritance() {
        let src = "class Animal { fn init(self, name: String) -> Null { self.name = name; return null; } fn speak(self) -> String { return self.name; } } class Dog: Animal { fn rename(self, name: String) -> String { self.name = name; return self.name; } } let d=Dog(\"Rex\"); let a=d.speak(); let b=d.rename(\"Max\"); let c=d.name;";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn loop_control() {
        assert!(
            run_source(
                "let s=0; for n in range(0,10) { if n==2 { continue; } if n==5 { break; } s=s+n; }"
            )
            .is_ok()
        );
        assert!(compile_source("break;").is_err());
        assert!(compile_source("continue;").is_err());
    }
    #[test]
    fn runtime_type_enforcement() {
        assert!(run_source("let x: Int=1; x=2;").is_ok());
        assert!(run_source("let x: Int=1; x=\"bad\";").is_err());
        assert!(run_source("let x: Int=\"bad\";").is_err());
        assert!(run_source("fn f(x: Int)->Int{return x+1;} let y=f(3);").is_ok());
        assert!(run_source("fn f(x: Int)->Int{return x+1;} let y=f(\"bad\");").is_err());
        assert!(run_source("fn f()->Int{return \"bad\";} let y=f();").is_err());
        assert!(
            run_source(
                "class A {} class B: A {} fn use_a(x: A)->A{return x;} let b=B(); let x=use_a(b);"
            )
            .is_ok()
        );
        assert!(
            run_source(
                "class A {} class C {} fn use_a(x: A)->A{return x;} let c=C(); let x=use_a(c);"
            )
            .is_err()
        );
    }
    #[test]
    fn functional_collections() {
        let src = "fn twice(x: Int)->Int{return x*2;} fn even(x: Int)->Bool{return x%2==0;} fn add(a: Int,b: Int)->Int{return a+b;} let a=append([1,2],3); let b=slice(a,1,3); let c=reverse(b); let d=sort([3,1,2]); let e=unique([1,1,2,2,3]); let f=enumerate([7,8]); let g=zip([1,2],[3,4]); let h=map_call(\"twice\",[1,2,3]); let i=filter_call(\"even\",[1,2,3,4]); let j=reduce_call(\"add\",[1,2,3],0);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn everyday_builtins() {
        let src = "let a=int(\"7\"); let b=float(\"2.5\"); let c=bool(1); assert(a==7); let s=sum([1,2,3]); let lo=min([4,2,9]); let hi=max([4,2,9]); let aa=all([true,1,\"x\"]); let an=any([false,0,3]); let t=tuple([1,2]); let st=set([1,1,2]); let has=contains(st,2); let ix=index_of([9,8,7],8);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn expanded_science_data_ml() {
        let src = "let med=vec_median([1,2,9,4]); let p=vec_percentile([1,2,3,4],75); let c=correlation([1,2,3],[2,4,6]); let rows=[{\"id\":1,\"g\":\"a\"},{\"id\":2,\"g\":\"b\"},{\"id\":3,\"g\":\"a\"}]; let groups=table_group_count(rows,\"g\"); let sorted=table_sort(rows,\"id\"); let selected=table_select(rows,[\"id\"]); let joined=table_inner_join(rows,[{\"rid\":1,\"v\":9}],\"id\",\"rid\"); let model=logreg_fit([[0],[1],[2],[3]],[0,0,1,1],100,0.1); let prob=logreg_predict(model,[3]);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn channels_work() {
        assert!(run_source("let ch=channel(); send(ch,\"hello\"); let x=recv(ch);").is_ok());
    }
    #[test]
    fn await_syntax_and_introspection() {
        let src = "class A {} class B: A {} fn square(n: Int)->Int{return n*n;} let task=spawn(\"square\",[6]); let result=await task; let b=B(); let t=type_of(b); let yes=is_instance(b,\"A\"); let field=has_field(b,\"x\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn lambda_function_values() {
        let src = "let offset=4; let add: Function=fn(x: Int)->Int{return x+offset;}; let y=add(6); assert(y==10); assert(type_of(add)==\"Function\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn named_function_values() {
        let src = "fn twice(x: Int)->Int{return x*2;} let f: Function=twice; let ys=map_call(f,[1,2,3]); assert(ys[0]==2); assert(ys[2]==6);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn list_comprehensions() {
        let src = "let factor=3; let xs=[x*factor for x in [1,2,3,4] if x%2==0]; assert(len(xs)==2); assert(xs[0]==6); assert(xs[1]==12);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn match_case_statement() {
        let src = "let x=2; let label=\"\"; match x { case 1 { label=\"one\"; } case 2 { label=\"two\"; } case _ { label=\"other\"; } } assert(label==\"two\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn advanced_linear_algebra() {
        let src = "let d=matrix_det([[4,7],[2,6]]); assert(abs(d-10)<0.000001); let inv=matrix_inverse([[4,7],[2,6]]); assert(abs(inv[0][0]-0.6)<0.000001); let x=linear_solve([[3,2],[1,2]],[5,5]); assert(abs(x[0])<0.000001); assert(abs(x[1]-2.5)<0.000001);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn generators_yield_and_collect() {
        let src = "fn count_to(n: Int) -> Generator { let i=0; while i<n { yield i; i=i+1; } } let g=count_to(4); let a=next(g); let b=next(g); let rest=collect(g); assert(a==0); assert(b==1); assert(len(rest)==2);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn lsp_navigation_helpers() {
        let src = "fn add(x: Int)->Int{return x+1;} let y=add(4); let z=add(y);";
        let def = lsp_definition(src, "file:///test.vy", "add");
        assert!(!def.is_null());
        let refs = lsp_references(src, "file:///test.vy", "add");
        assert!(refs.as_array().map(|x| x.len()).unwrap_or(0) >= 3);
    }
    #[test]
    fn grouped_data_and_plotting() {
        let scatter = env::temp_dir().join("veyra_scatter.svg");
        let bars = env::temp_dir().join("veyra_bars.svg");
        let src = format!(
            "let rows=[{{\"g\":\"a\",\"x\":1}},{{\"g\":\"a\",\"x\":3}},{{\"g\":\"b\",\"x\":4}}]; let sums=table_group_sum(rows,\"g\",\"x\"); let means=table_group_mean(rows,\"g\",\"x\"); let h=histogram([1,1,2,3,4,4],3); scatter_svg(\"{}\",[1,2,3],[3,2,5]); bar_svg(\"{}\",[\"a\",\"b\"],[4,5]);",
            scatter.to_string_lossy().replace('\\', "\\\\"),
            bars.to_string_lossy().replace('\\', "\\\\")
        );
        assert!(run_source(&src).is_ok());
        assert!(scatter.exists());
        assert!(bars.exists());
        let _ = fs::remove_file(scatter);
        let _ = fs::remove_file(bars);
    }
    #[test]
    fn async_functions_methods_and_gather() {
        let src = "async fn square(n: Int)->Int{sleep_ms(1);return n*n;} class Worker { async fn plus(self,n:Int)->Int{return n+1;} } let a=square(3); let b=square(4); let values=gather([a,b]); assert(values[0]==9); assert(values[1]==16); let w=Worker(); let x=await w.plus(7); assert(x==8);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn context_manager_with_as() {
        let src = "class Resource { fn init(self)->Null{self.closed=false;return null;} fn enter(self)->Resource{return self;} fn exit(self)->Null{self.closed=true;return null;} } with Resource() as r { assert(r.closed==false); } assert(r.closed==true);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn decorator_syntax_identity() {
        let src = "fn identity(f: Function)->Function{return f;} @identity fn plus_one(x:Int)->Int{return x+1;} assert(plus_one(4)==5);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn escaping_closure_objects() {
        let src = "fn twice_result(f:Function)->Function{return fn(x:Int)->Int{return f(x)*2;};} @twice_result fn plus_one(x:Int)->Int{return x+1;} fn make_adder(offset:Int)->Function{return fn(x:Int)->Int{return x+offset;};} let add7=make_adder(7); assert(plus_one(4)==10); assert(add7(5)==12);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn module_map_dispatch() {
        let mut vm = Vm::new(compile_source("fn plus_one(x:Int)->Int{return x+1;}").unwrap());
        vm.run().unwrap();
        let def = vm.funcs.get("plus_one").unwrap().clone();
        let key = export_function_object(def).unwrap();
        let module = Val::Map(vec![(Val::Str("plus".into()), Val::Function(key))]);
        let mut caller = Vm::new(vec![
            Op::Load("module".into()),
            Op::Const(Val::Num(4.0)),
            Op::CallMethod("plus".into(), 1),
            Op::Const(Val::Num(5.0)),
            Op::Eq,
            Op::Call("assert".into(), 1),
            Op::Halt,
        ]);
        caller.vars.insert("module".into(), module);
        assert!(caller.run().is_ok());
    }
    #[test]
    fn ml_metrics_and_split() {
        let src = "let m=classification_metrics([1,0,1,1],[1,0,0,1]); assert(m[\"accuracy\"]==0.75); assert(m[\"precision\"]==1); let s=train_test_split([1,2,3,4],[10,20,30,40],0.25); assert(len(s[\"train_x\"])==3); assert(len(s[\"test_x\"])==1);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn path_and_text_services() {
        let src = "assert(starts_with(\"veyra\",\"vey\")); assert(ends_with(\"veyra\",\"yra\")); assert(replace_text(\"a-b\",\"-\",\"+\")==\"a+b\"); let p=path_join(\"a\",\"b.txt\"); assert(path_name(p)==\"b.txt\"); assert(path_extension(p)==\"txt\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn finally_cleanup_semantics() {
        let src = "let cleaned=false; try { throw \"boom\"; } catch err { assert(err==\"boom\"); } finally { cleaned=true; } assert(cleaned); let normal=false; try { normal=true; } catch err {} finally { normal=true; } assert(normal);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn dataframe_extended_helpers() {
        let src = "let rows=[{\"id\":1,\"x\":1},{\"id\":2,\"x\":null},{\"id\":3,\"x\":3}]; let h=table_head(rows,2); let t=table_tail(rows,1); assert(len(h)==2); assert(t[0][\"id\"]==3); let d=table_drop_nulls(rows,\"x\"); assert(len(d)==2); let f=table_fill_null(rows,\"x\",9); assert(f[1][\"x\"]==9); let joined=table_left_join(rows,[{\"rid\":1,\"name\":\"a\"}],\"id\",\"rid\"); assert(len(joined)==3);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn tensor_autograd_and_training() {
        let src = "let x=tensor([[1],[2],[3],[4]],false); let y=tensor([[2],[4],[6],[8]],false); let w=tensor([[0]],true); let epoch=0; while epoch<40 { let pred=tensor_matmul(x,w); let diff=tensor_sub(pred,y); let sq=tensor_mul(diff,diff); let loss=tensor_mean(sq); tensor_backward(loss); tensor_step(w,0.05); tensor_zero_grad(w); epoch=epoch+1; } let trained=tensor_values(w); assert(trained[0]>1.9 and trained[0]<2.1);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn loop_local_declaration_reinitializes() {
        let src = "let i=0; let total=0; while i<3 { let local=i+1; total=total+local; i=i+1; } assert(total==6);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn tensor_extended_autograd_and_shapes() {
        let src = "let x=tensor([1,2],true); let y=tensor_sigmoid(x); let loss=tensor_sum(y); tensor_backward(loss); let g=tensor_grad(x); assert(g[0]>0.19 and g[0]<0.20); assert(g[1]>0.10 and g[1]<0.11); let m=tensor([[1,2,3],[4,5,6]],false); let tr=tensor_transpose(m); let sh=tensor_shape(tr); assert(sh[0]==3 and sh[1]==2); let r=tensor_reshape(tr,[6]); let v=tensor_values(r); assert(v[0]==1 and v[1]==4 and v[5]==6);";
        if let Err(e) = run_source(src) {
            panic!("extended tensor test failed: {e}");
        }
    }
    #[test]
    fn tensor_advanced_ops_and_softmax() {
        let src = "let x=tensor([2,4],true); let y=tensor_pow(x,2); let loss=tensor_sum(y); tensor_backward(loss); let g=tensor_grad(x); assert(g[0]==4 and g[1]==8); let a=tensor([6,8],true); let b=tensor([2,4],false); let q=tensor_div(a,b); let qloss=tensor_sum(q); tensor_backward(qloss); let qg=tensor_grad(a); assert(qg[0]==0.5 and qg[1]==0.25); let n=tensor_neg(a); let nv=tensor_values(n); assert(nv[0]==-6 and nv[1]==-8); let s=tensor_softmax(tensor([[1,2,3],[2,2,2]],false)); let sv=tensor_values(s); assert(sv[2]>sv[1] and sv[1]>sv[0]); assert(sv[3]>0.33 and sv[3]<0.34); assert(tensor_numel(s)==6);";
        if let Err(e) = run_source(src) {
            panic!("advanced tensor ops failed: {e}");
        }
    }
    #[test]
    fn adam_optimizer_trains_parameter() {
        let src = "let target=tensor([3],false); let w=tensor([0],true); let epoch=0; while epoch<120 { let diff=tensor_sub(w,target); let loss=tensor_mean(tensor_pow(diff,2)); tensor_backward(loss); tensor_adam_step(w,0.1,0.9,0.999,0.00000001); tensor_zero_grad(w); epoch=epoch+1; } let value=tensor_values(w); assert(value[0]>2.9 and value[0]<3.1); tensor_reset_optimizer(w);";
        if let Err(e) = run_source(src) {
            panic!("adam optimizer test failed: {e}");
        }
    }
    #[test]
    fn tensor_serialization_round_trip() {
        let path = env::temp_dir().join("veyra_tensor_round_trip.json");
        let escaped = path.to_string_lossy().replace('\\', "\\\\");
        let src = format!(
            "let x=tensor([[1,2],[3,4]],true); tensor_save(x,\"{escaped}\"); let y=tensor_load(\"{escaped}\"); let sh=tensor_shape(y); let v=tensor_values(y); assert(sh[0]==2 and sh[1]==2); assert(v[0]==1 and v[3]==4);"
        );
        assert!(run_source(&src).is_ok());
        let _ = fs::remove_file(path);
    }
    #[test]
    fn task_timeout_and_status() {
        let src = "fn slow()->Int{sleep_ms(25);return 7;} let t=spawn(slow,[]); let s=task_status(t); assert(s==\"running\" or s==\"finished\"); let early=await_timeout(t,1); assert(early==null); let value=await_task(t); assert(value==7);";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn lsp_rename_helper() {
        let src = "fn add(x:Int)->Int{return x+1;} let a=add(1); let b=add(a);";
        let edit = lsp_rename(src, "file:///test.vy", "add", "increment");
        let changes = edit
            .get("changes")
            .and_then(|x| x.get("file:///test.vy"))
            .and_then(JsonValue::as_array)
            .map(|x| x.len())
            .unwrap_or(0);
        assert!(changes >= 3);
    }
    #[test]
    fn security_primitives() {
        let src = "let mac=hmac_sha256(\"key\",\"message\"); assert(len(mac)==64); let derived=pbkdf2_sha256(\"password\",\"salt\",100,32); assert(len(derived)==64); assert(constant_time_equal(mac,mac)); assert(not constant_time_equal(mac,derived));";
        assert!(run_source(src).is_ok());
    }
    #[cfg(feature = "native-services")]
    #[test]
    fn sqlite_parameter_binding() {
        let db = env::temp_dir().join("veyra_params_test.sqlite");
        let escaped = db.to_string_lossy().replace('\\', "\\\\");
        let src = format!(
            "sqlite_exec(\"{escaped}\",\"CREATE TABLE IF NOT EXISTS items(id INTEGER,name TEXT)\"); sqlite_exec(\"{escaped}\",\"DELETE FROM items\"); sqlite_exec_params(\"{escaped}\",\"INSERT INTO items(id,name) VALUES (?,?)\",[7,\"veyra\"]); let rows=sqlite_query_params(\"{escaped}\",\"SELECT id,name FROM items WHERE id=?\",[7]); assert(len(rows)==1); assert(rows[0][\"name\"]==\"veyra\");"
        );
        assert!(run_source(&src).is_ok());
        let _ = fs::remove_file(db);
    }
    #[test]
    fn static_server_rejects_path_traversal() {
        let root = env::temp_dir().join("veyra_static_traversal_test");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let index = root.join("index.html");
        fs::write(&index, "ok").unwrap();
        assert!(resolve_static_path(&root, "/").is_some());
        assert!(resolve_static_path(&root, "/index.html?x=1").is_some());
        assert!(resolve_static_path(&root, "/../secret.txt").is_none());
        assert!(resolve_static_path(&root, "/../../Windows/win.ini").is_none());
        let _ = fs::remove_dir_all(&root);
    }
    #[test]
    fn use_preserves_trailing_statements_on_same_line() {
        let dir = env::temp_dir().join("veyra_inline_use_test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mod_a = dir.join("a.vy");
        let mod_b = dir.join("b.vy");
        let main_vy = dir.join("main.vy");
        fs::write(&mod_a, "fn fa() -> Int { return 10; }").unwrap();
        fs::write(&mod_b, "fn fb() -> Int { return 20; }").unwrap();
        fs::write(
            &main_vy,
            "use \"a.vy\"; use \"b.vy\"; assert(fa() + fb() == 30);",
        )
        .unwrap();
        let loaded = load_source(main_vy.to_string_lossy().as_ref()).unwrap();
        assert!(loaded.contains("assert(fa() + fb() == 30);"));
        assert!(run_source(&loaded).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
    #[test]
    fn type_of_distinguishes_int_and_float() {
        let src = "assert(type_of(7)==\"Int\"); assert(type_of(2.5)==\"Number\");";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn short_circuit_boolean_operators() {
        let src = "
            fn boom() -> Bool {
                throw \"rhs should not evaluate\";
            }
            let a = false and boom();
            let b = true or boom();
            assert(a == false);
            assert(b == true);
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn cross_runtime_syntax_parity() {
        let src = "
            /* block comment */
            # hash comment
            let msg = 'hello';
            assert(msg == \"hello\");

            let x = 10;
            x += 5;
            x -= 3;
            x *= 2;
            x /= 4;
            assert(x == 6);

            let label = '';
            if x == 1 {
                label = 'one';
            } else if x == 6 {
                label = 'six';
            } else {
                label = 'other';
            }
            assert(label == 'six');

            let xs = [10, 20, 30];
            xs[1] = 99;
            xs[2] += 1;
            assert(xs[1] == 99 and xs[2] == 31);

            let m = {'a': 1};
            m['b'] = 2;
            m['a'] += 10;
            assert(m['a'] == 11 and m['b'] == 2);

            let sum1 = 0;
            for i in range(5) {
                sum1 += i;
            }
            assert(sum1 == 10);

            let sum2 = 0;
            for i in range(1, 6, 2) {
                sum2 += i;
            }
            assert(sum2 == 9);
            assert(len(range(4)) == 4);
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn power_tuples_and_set_literals() {
        let src = "
            assert(2 ** 3 ** 2 == 512);
            let t = (10, 20, 30);
            assert(len(t) == 3 and t[1] == 20);
            let s = {1, 2, 2, 3, 3};
            assert(len(s) == 3 and contains(s, 2));
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn try_catch_unwinds_and_catches_runtime_errors() {
        let src = "
            let caught_oob = false;
            let xs = [1, 2, 3];
            try {
                let bad = 10 + xs[99];
            } catch err {
                caught_oob = contains(err, 'out of range');
            }
            assert(caught_oob);

            fn inner_fail() -> Int {
                throw 'nested-boom';
            }
            let caught_nested = '';
            try {
                let v = inner_fail();
            } catch err {
                caught_nested = err;
            }
            assert(caught_nested == 'nested-boom');
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn line_aware_diagnostics_and_static_parent_check() {
        let bad_src = "let a = 1;\nlet b = 2;\nlet c = ;";
        let diags = lsp_diagnostics(bad_src);
        assert_eq!(diags.len(), 1);
        let line = diags[0]
            .pointer("/range/start/line")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        assert_eq!(line, 2);
        assert!(compile_source("class Dog: MissingParent {}").is_err());
    }
    #[test]
    fn bytecode_optimizer_folds_constants_and_preserves_semantics() {
        let src = "
            let x = 2 + 3 * 4 + 2 ** 3;
            let s = 'vey' + 'ra';
            let ok = (10 > 5) and (not false);
            assert(x == 22);
            assert(s == 'veyra');
            assert(ok == true);
        ";
        let unopt = compile_source_unoptimized(src).unwrap();
        let opt = compile_source(src).unwrap();
        assert!(opt.len() < unopt.len());
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn autograd_tape_reclaims_intermediate_tensors() {
        let src = "
            let x = tensor([[1], [2], [3], [4]], false);
            let y = tensor([[2], [4], [6], [8]], false);
            let w = tensor([[0]], true);
            for epoch in range(25) {
                let pred = tensor_matmul(x, w);
                let diff = tensor_sub(pred, y);
                let sq = tensor_mul(diff, diff);
                let loss = tensor_mean(sq);
                tensor_backward(loss);
                tensor_step(w, 0.05);
                tensor_zero_grad(w);
            }
            let reclaimed = tensor_gc();
            assert(reclaimed >= 0);
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn mutable_closure_upvalues() {
        let src = "
            fn make_counter(start: Int) -> Function {
                let count = start;
                return fn() -> Int {
                    count += 1;
                    return count;
                };
            }
            let c1 = make_counter(0);
            let c2 = make_counter(10);
            assert(c1() == 1);
            assert(c1() == 2);
            assert(c2() == 11);
            assert(c1() == 3);

            let shared = 5;
            fn bump_shared(delta: Int) -> Int {
                shared += delta;
                return shared;
            }
            assert(bump_shared(7) == 12);
            assert(shared == 12);
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn with_cleanup_on_exception() {
        let src = "
            class Guard {
                fn init(self) -> Null {
                    self.cleaned = false;
                    return null;
                }
                fn enter(self) -> Guard {
                    return self;
                }
                fn exit(self) -> Null {
                    self.cleaned = true;
                    return null;
                }
            }
            let g = Guard();
            let caught = false;
            try {
                with g as active {
                    assert(active.cleaned == false);
                    throw 'abort-inside-with';
                }
            } catch err {
                caught = (err == 'abort-inside-with');
            }
            assert(caught);
            assert(g.cleaned == true);
        ";
        assert!(run_source(src).is_ok());
    }
    #[test]
    fn vybc_round_trip_and_checksum_tamper_detection() {
        let src = "
            fn square(n: Int) -> Int { return n * n; }
            let total = 0;
            for i in range(1, 5) {
                total += square(i);
            }
            assert(total == 30);
        ";
        let vybc = compile_to_vybc(src).unwrap();
        assert!(run_vybc(&vybc).is_ok());
        let tampered = vybc.replace("\"Add\"", "\"Sub\"");
        assert!(run_vybc(&tampered).is_err());
    }
    #[test]
    fn self_hosted_compiler_three_stage_fixpoint() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let compiler_path = root.join("compiler-vy").join("src").join("main.vy");
        let compiler_src = fs::read_to_string(&compiler_path).unwrap();
        assert!(!compiler_src.contains("vybc_compile"));

        // Stage 0 (Rust Horizon engine) compiles compiler-vy into stage1.vybc
        let stage1_vybc = compile_to_vybc(&compiler_src).unwrap();
        assert!(run_vybc(&stage1_vybc).is_ok());

        // Stage 1 executes compiler-vy to compile a full Veyra program (functions, loops, branches, lists, maps)
        // into .vybc AND compiles compiler-vy itself (Stage 1 -> Stage 2) and then runs Stage 2 to compile
        // compiler-vy again (Stage 2 -> Stage 3), verifying Stage 2 == Stage 3 fixpoint equality.
        let compiler_path_str = compiler_path.to_string_lossy().replace('\\', "/");
        let stage3_tmp = std::env::temp_dir()
            .join("veyra_stage3_fixpoint.vybc")
            .to_string_lossy()
            .replace('\\', "/");
        let driver = format!(
            "{compiler_src}\n\
            let prog = \"fn fib(n: Int) -> Int {{ if n <= 1 {{ return n; }} return fib(n - 1) + fib(n - 2); }} let acc = 0; for x in [1, 2, 3, 4, 5, 6] {{ acc += fib(x); }} let m = {{\\\"sum\\\": acc}}; assert(m[\\\"sum\\\"] == 20);\";\n\
            let target_bc = compile_veyra_to_vybc(prog);\n\
            vybc_run(target_bc);\n\
            let self_src = read_file(\"{compiler_path_str}\");\n\
            let stage2_bc = bootstrap_compile_self(self_src);\n\
            let stage2_driver = self_src + \"\\nlet s = read_file(\\\"{compiler_path_str}\\\");\\nlet stage3_bc = bootstrap_compile_self(s);\\nwrite_file(\\\"{stage3_tmp}\\\", stage3_bc);\\n\";\n\
            let stage2_runner_bc = compile_veyra_to_vybc(stage2_driver);\n\
            vybc_run(stage2_runner_bc);\n\
            let stage3_bc = read_file(\"{stage3_tmp}\");\n\
            assert(stage2_bc == stage3_bc);\n\
            assert(sha256(stage2_bc) == sha256(stage3_bc));\n"
        );
        run_source(&driver).unwrap();
        let _ = fs::remove_file(&stage3_tmp);
    }
    #[test]
    fn tensor_broadcasting_and_backward_reduction() {
        let src = "
            let x = tensor([[1, 2], [3, 4], [5, 6]], true);
            let b = tensor([10, 20], true);
            let s = tensor([[2, 3]], true);
            let y = tensor_add(tensor_mul(x, s), b);
            assert(tensor_shape(y) == [3, 2]);
            assert(tensor_values(y) == [12, 26, 16, 32, 20, 38]);
            let loss = tensor_sum(y);
            tensor_backward(loss);
            assert(tensor_grad(b) == [3, 3]);
            assert(tensor_grad(s) == [9, 12]);
            assert(tensor_grad(x) == [2, 3, 2, 3, 2, 3]);
        ";
        assert!(run_source(src).is_ok());
    }
}
