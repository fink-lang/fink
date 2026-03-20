// End-to-end WASM codegen tests.
//
// Each test compiles a Fink source string through the full pipeline
// (parse → CPS → cont_lifting → closure_lifting → codegen)
// then runs the WAT via wasmtime and asserts the integer result.

use wasmtime::{Config, Engine, Linker, Module, Store};

fn compile_wat(src: &str) -> String {
  use fink::ast::build_index;
  use fink::parser::parse;
  use fink::passes::closure_lifting::lift_all;
  use fink::passes::cont_lifting::lift;
  use fink::passes::cps::transform::lower_expr;
  use fink::passes::wasm::codegen::codegen;

  let r = parse(src).expect("parse failed");
  let ast_index = build_index(&r);
  let cps = lower_expr(&r.root);
  let cps = lift(cps);
  let (lifted, resolved) = lift_all(cps, &ast_index);
  let lifted = lift(lifted);
  codegen(&lifted, &resolved, &ast_index).wat
}

fn run(src: &str) -> i32 {
  let wat = compile_wat(src);
  eprintln!("--- WAT ---\n{}\n-----------", wat);
  exec_wat(&wat)
}

fn exec_wat(wat: &str) -> i32 {
  let mut config = Config::new();
  config.wasm_gc(true);
  config.wasm_function_references(true);
  config.wasm_tail_call(true);
  let engine = Engine::new(&config).expect("engine");
  let module = Module::new(&engine, wat).expect("module");
  let mut store = Store::new(&engine, ());
  let mut linker = Linker::new(&engine);

  // Provide env.print stub
  linker.func_wrap("env", "print", |v: i32| { println!("{}", v); })
    .expect("define print");

  let instance = linker.instantiate(&mut store, &module).expect("instance");
  let main = instance.get_func(&mut store, "fink_main").expect("fink_main");
  main.call(&mut store, &[], &mut []).expect("call fink_main");
  let result = instance.get_global(&mut store, "result").expect("result");
  match result.get(&mut store) {
    wasmtime::Val::I32(v) => v,
    v => panic!("expected i32 result, got {:?}", v),
  }
}

#[test]
fn literal_int() {
  assert_eq!(run("main = fn: 42"), 42);
}
