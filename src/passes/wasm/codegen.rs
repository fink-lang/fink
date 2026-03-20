// CPS IR → WAT codegen.
//
// Produces a WAT text string from the fully-lifted CPS IR.
// All functions are CPS: no return values, tail-call via return_call.
//
// Calling convention:
//   Every Fink function: (param $args (ref $AnyArray)) (param $cont anyref)
//   Continuation call: return_call $__call_closure (cont, result_array)
//   Built-in ops: inlined; result passed to cont
//
// Module structure:
//   - Fixed heap type hierarchy ($Any, $Int, $FnClosure, etc.)
//   - $FinkFn function type shared by all Fink functions
//   - $fink_main export — entry point called by runtime
//   - $__halt — terminal cont; stores result in $result global
//
// Post-lifting invariant:
//   All Arg::Cont entries are Cont::Ref (never Cont::Expr).
//   LetVal/LetFn body conts may be Cont::Ref or Cont::Expr.

use crate::ast::{AstId, Node as AstNode};
use crate::passes::cps::ir::{
  Arg, Bind, Callable, Cont, CpsId, CpsResult, Expr, ExprKind,
  Lit, Val, ValKind,
};
use crate::passes::name_res::ResolveResult;
use crate::propgraph::PropGraph;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// A WAT source mapping: WAT line number → Fink source location.
/// Converted to WASM byte offset mappings after WAT→WASM compilation.
#[derive(Debug, Clone)]
pub struct WatMapping {
  /// 0-indexed line in the emitted WAT text.
  pub wat_line: u32,
  /// 0-indexed line in the Fink source.
  pub src_line: u32,
  /// 0-indexed column in the Fink source.
  pub src_col: u32,
}

/// Codegen result: WAT text + source mappings.
pub struct CodegenResult {
  pub wat: String,
  pub mappings: Vec<WatMapping>,
}

/// Compile fully-lifted CPS IR to WAT text with source mappings.
pub fn codegen(
  cps: &CpsResult,
  _resolve: &ResolveResult,
  ast_index: &PropGraph<AstId, Option<&AstNode<'_>>>,
) -> CodegenResult {
  let mut ctx = Ctx::new(&cps.origin, ast_index);
  emit_module(&cps.root, &mut ctx);
  CodegenResult { wat: ctx.out, mappings: ctx.mappings }
}

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

struct Ctx<'a, 'src> {
  out: String,
  indent: usize,
  line_count: u32,
  mappings: Vec<WatMapping>,
  _origin: &'a PropGraph<CpsId, Option<AstId>>,
  _ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
}

impl<'a, 'src> Ctx<'a, 'src> {
  fn new(
    origin: &'a PropGraph<CpsId, Option<AstId>>,
    ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  ) -> Self {
    Self { out: String::new(), indent: 0, line_count: 0, mappings: Vec::new(), _origin: origin, _ast_index: ast_index }
  }

  fn line(&mut self, s: &str) {
    for _ in 0..self.indent { self.out.push_str("  "); }
    self.out.push_str(s);
    self.out.push('\n');
    self.line_count += 1;
  }

  /// Emit a WAT line with a source mapping from a CPS node.
  #[allow(dead_code)] // infrastructure for source map integration
  fn line_mapped(&mut self, s: &str, cps_id: CpsId) {
    let wat_line = self.line_count;
    self.line(s);
    if let Some(Some(ast_id)) = self._origin.try_get(cps_id)
      && let Some(Some(ast_node)) = self._ast_index.try_get(*ast_id)
    {
      self.mappings.push(WatMapping {
        wat_line,
        src_line: ast_node.loc.start.line,
        src_col: ast_node.loc.start.col,
      });
    }
  }

  fn push(&mut self) { self.indent += 1; }
  fn pop(&mut self) { self.indent -= 1; }
}

// ---------------------------------------------------------------------------
// Module emission
// ---------------------------------------------------------------------------

fn emit_module(root: &Expr<'_>, ctx: &mut Ctx) {
  ctx.line("(module");
  ctx.push();

  emit_types(ctx);
  emit_imports(ctx);
  emit_globals(ctx);
  emit_builtins(ctx);
  emit_funcs(root, ctx);
  emit_start(root, ctx);

  ctx.pop();
  ctx.line(")");
}

// ---------------------------------------------------------------------------
// Fixed type definitions
// ---------------------------------------------------------------------------

fn emit_types(ctx: &mut Ctx) {
  // $Any — abstract base for all Fink values
  ctx.line("(type $Any (sub (struct)))");
  // $AnyArray — backing array (anyref to hold i31ref and struct refs)
  ctx.line("(type $AnyArray (array (mut anyref)))");
  // $Int — boxed i64
  ctx.line("(type $Int (sub $Any (struct (field i64))))");
  // $FinkFn — all Fink functions: (args, cont) → void
  ctx.line("(type $FinkFn (func (param (ref $AnyArray)) (param anyref)))");
  // $FnClosure — closure: function ref + captures array
  ctx.line("(type $FnClosure (sub $Any (struct (field (ref $FinkFn)) (field (ref $AnyArray)))))");
}

fn emit_imports(ctx: &mut Ctx) {
  ctx.line("(import \"env\" \"print\" (func $print (param i32)))");
}

fn emit_globals(ctx: &mut Ctx) {
  ctx.line("(global $result (export \"result\") (mut i32) (i32.const 0))");
}

// ---------------------------------------------------------------------------
// Built-in functions
// ---------------------------------------------------------------------------

fn emit_builtins(ctx: &mut Ctx) {
  // $__halt — terminal continuation; extracts i31ref from args[0], stores as result
  ctx.line("(func $__halt (type $FinkFn)");
  ctx.push();
  ctx.line("(param $args (ref $AnyArray))");
  ctx.line("(param $cont anyref)");
  ctx.line("(global.set $result (i31.get_s (ref.cast i31ref (array.get $AnyArray (local.get $args) (i32.const 0)))))");
  ctx.pop();
  ctx.line(")");

  // $__call_closure — call a FnClosure: prepend captures to args, tail-call fn
  emit_call_closure(ctx);
}

fn emit_call_closure(ctx: &mut Ctx) {
  ctx.line("(func $__call_closure");
  ctx.push();
  ctx.line("(param $closure anyref)");
  ctx.line("(param $args (ref $AnyArray))");
  ctx.line("(param $cont anyref)");
  // Extract fn ref and captures from closure
  ctx.line("(local $fn (ref $FinkFn))");
  ctx.line("(local $caps (ref $AnyArray))");
  ctx.line("(local.set $fn (struct.get $FnClosure 0 (ref.cast (ref $FnClosure) (local.get $closure))))");
  ctx.line("(local.set $caps (struct.get $FnClosure 1 (ref.cast (ref $FnClosure) (local.get $closure))))");
  // For now: just call fn with args directly (no capture prepending yet)
  ctx.line("(return_call_ref $FinkFn (local.get $args) (local.get $cont) (local.get $fn))");
  ctx.pop();
  ctx.line(")");
}

// ---------------------------------------------------------------------------
// Function emission
// ---------------------------------------------------------------------------

fn emit_funcs(root: &Expr<'_>, ctx: &mut Ctx) {
  // Walk the CPS tree: emit each LetFn as a Wasm function,
  // then emit $__main which runs the module init code.
  collect_funcs(root, ctx);

  // $__main — module init; runs the root CPS chain.
  // For `main = fn: 42`, this defines the `main` fn and calls it.
  ctx.line("(func $__main (type $FinkFn)");
  ctx.push();
  ctx.line("(param $args (ref $AnyArray))");
  ctx.line("(param $cont anyref)");
  emit_expr(root, ctx);
  ctx.pop();
  ctx.line(")");
}

/// Collect and emit all LetFn definitions as top-level Wasm functions.
fn collect_funcs(expr: &Expr<'_>, ctx: &mut Ctx) {
  match &expr.kind {
    ExprKind::LetFn { name, fn_body, body, .. } => {
      let fname = wat_name(name.id, name.kind);
      emit_func(&fname, fn_body, ctx);
      // Also collect nested LetFns inside fn_body
      collect_funcs(fn_body, ctx);
      if let Cont::Expr { body: cont_body, .. } = body {
        collect_funcs(cont_body, ctx);
      }
    }
    ExprKind::LetVal { body: Cont::Expr { body: cont_body, .. }, .. } => {
      collect_funcs(cont_body, ctx);
    }
    _ => {}
  }
}

fn emit_func(name: &str, body: &Expr<'_>, ctx: &mut Ctx) {
  ctx.line(&format!("(func {} (type $FinkFn)", name));
  ctx.push();
  ctx.line("(param $args (ref $AnyArray))");
  ctx.line("(param $cont anyref)");
  emit_expr(body, ctx);
  ctx.pop();
  ctx.line(")");
}

// ---------------------------------------------------------------------------
// Expression emission
// ---------------------------------------------------------------------------

fn emit_expr(expr: &Expr<'_>, ctx: &mut Ctx) {
  match &expr.kind {
    ExprKind::LetVal { val, body, .. } => {
      // For now: the val becomes the result passed to cont.
      // Push val, then emit body cont call.
      match body {
        Cont::Ref(cont_id) => {
          // Build a 1-element args array with the val, call cont
          emit_val_to_result(val, *cont_id, ctx);
        }
        Cont::Expr { body: cont_body, .. } => {
          // Inline: just emit the continuation body
          emit_expr(cont_body, ctx);
        }
      }
    }

    ExprKind::LetFn { body, .. } => {
      // LetFn inside a function body — the fn was already emitted at top level.
      // Just continue with the body cont.
      if let Cont::Expr { body: cont_body, .. } = body {
        emit_expr(cont_body, ctx);
      }
    }

    ExprKind::App { func, args } => {
      emit_app(func, args, ctx);
    }

    _ => {
      ctx.line(";; TODO: unhandled expr kind");
      ctx.line("unreachable");
    }
  }
}

/// Emit code to pass a single value to a continuation.
/// Builds a 1-element $AnyArray, then tail-calls the cont via $__call_closure.
fn emit_val_to_result(val: &Val<'_>, _cont_id: CpsId, ctx: &mut Ctx) {
  // Build args array with single element
  ctx.line("(return_call $__call_closure");
  ctx.push();
  // cont (from local — we need to figure out where it's stored)
  // For now, use the $cont param directly
  ctx.line("(local.get $cont)");
  // args: 1-element array
  ctx.line("(array.new_fixed $AnyArray 1");
  ctx.push();
  emit_val(val, ctx);
  ctx.pop();
  ctx.line(")");
  // cont passthrough (unused by __call_closure result)
  ctx.line("(ref.null none)");
  ctx.pop();
  ctx.line(")");
}

fn emit_val(val: &Val<'_>, ctx: &mut Ctx) {
  match &val.kind {
    ValKind::Lit(Lit::Int(n)) => {
      // Small ints use i31ref
      ctx.line(&format!("(ref.i31 (i32.const {}))", n));
    }
    ValKind::Lit(Lit::Bool(b)) => {
      ctx.line(&format!("(ref.i31 (i32.const {}))", if *b { 1 } else { 0 }));
    }
    _ => {
      ctx.line(&format!(";; TODO: emit_val {:?}", val.kind));
      ctx.line("(ref.i31 (i32.const 0))");
    }
  }
}

fn emit_app(_func: &Callable<'_>, _args: &[Arg<'_>], ctx: &mut Ctx) {
  ctx.line(";; TODO: App codegen");
  ctx.line("unreachable");
}

// ---------------------------------------------------------------------------
// Start function
// ---------------------------------------------------------------------------

fn emit_start(root: &Expr<'_>, ctx: &mut Ctx) {
  // Find the name of the `main` function in the root CPS chain.
  // For `main = fn: 42`, the root is LetFn { name: ·v_1, ... }
  let main_fn = find_main_fn(root);

  // $fink_main — no-arg export; calls main with halt cont
  ctx.line("(func (export \"fink_main\")");
  ctx.push();
  ctx.line(&format!("(call {}", main_fn));
  ctx.push();
  ctx.line("(array.new_fixed $AnyArray 0)  ;; no args");
  ctx.line("(struct.new $FnClosure (ref.func $__halt) (array.new_fixed $AnyArray 0))  ;; halt cont");
  ctx.pop();
  ctx.line(")");
  ctx.pop();
  ctx.line(")");

  ctx.line(&format!("(elem declare func $__halt {})", main_fn));
}

/// Find the WAT name of the first LetFn in the root chain (the entry point).
fn find_main_fn(expr: &Expr<'_>) -> String {
  match &expr.kind {
    ExprKind::LetFn { name, .. } => wat_name(name.id, name.kind),
    _ => "$__main".to_string(),
  }
}

// ---------------------------------------------------------------------------
// Naming
// ---------------------------------------------------------------------------

fn wat_name(id: CpsId, bind: Bind) -> String {
  match bind {
    Bind::Name => format!("$__name_{}", id.0),
    Bind::Synth => format!("$__v_{}", id.0),
    Bind::Cont => format!("$__cont_{}", id.0),
  }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
  use wasmtime::{Config, Engine, Linker, Module, Store};

  use crate::ast::build_index;
  use crate::parser::parse;
  use crate::passes::closure_lifting::lift_all;
  use crate::passes::cont_lifting::lift;
  use crate::passes::cps::transform::lower_expr;
  use super::codegen;

  fn compile_wat(src: &str) -> String {
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

    linker.func_wrap("env", "print", |v: i32| { println!("{v}"); })
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
}
