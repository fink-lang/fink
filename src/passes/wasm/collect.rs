// Collect phase — walks lifted CPS IR and gathers module-level structure.
//
// Shared by the WASM binary emitter and the WAT text writer. This module
// contains only format-independent data structures and IR-walking logic;
// it has no dependency on text formatting or binary encoding.

use std::collections::{BTreeSet, HashSet};

use crate::ast::{AstId, Node as AstNode, NodeKind};
use crate::passes::cps::ir::{
  Arg, BuiltIn, Callable, Cont, CpsId, Expr, ExprKind,
  Param, Ref, ValKind,
};
use crate::propgraph::PropGraph;

// ---------------------------------------------------------------------------
// Context — origin map + AST index for name/loc recovery
// ---------------------------------------------------------------------------

/// Shared IR context for name and location recovery from CPS nodes.
///
/// Both the WASM emitter and WAT writer build on this. WAT-specific
/// concerns (MappedWriter marks, pipe separator locs) live in the WAT
/// writer's own wrapper.
pub struct IrCtx<'a, 'src> {
  pub origin: &'a PropGraph<CpsId, Option<AstId>>,
  pub ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  /// CpsIds that are module-level fn globals — rendered as global.get, not local.get.
  pub globals: HashSet<CpsId>,
}

impl<'a, 'src> IrCtx<'a, 'src> {
  pub fn new(
    origin: &'a PropGraph<CpsId, Option<AstId>>,
    ast_index: &'a PropGraph<AstId, Option<&'src AstNode<'src>>>,
  ) -> Self {
    Self { origin, ast_index, globals: HashSet::new() }
  }

  pub fn with_globals(mut self, globals: HashSet<CpsId>) -> Self {
    self.globals = globals;
    self
  }

  pub fn is_global(&self, id: CpsId) -> bool {
    self.globals.contains(&id)
  }

  /// Recover the AST node for a CPS node via the origin map.
  pub fn ast_node(&self, id: CpsId) -> Option<&'src AstNode<'src>> {
    self.origin.try_get(id)
      .and_then(|opt| *opt)
      .and_then(|ast_id| self.ast_index.try_get(ast_id))
      .and_then(|opt| *opt)
  }

  /// Recover the source name for a CPS bind/ref, or fall back to a synthetic label.
  pub fn label(&self, id: CpsId) -> String {
    match self.ast_node(id) {
      Some(node) => match &node.kind {
        NodeKind::Ident(s) => format!("{}_{}", s, id.0),
        NodeKind::SynthIdent(n) => format!("$_{}_{}", n, id.0),
        _ => format!("v_{}", id.0),
      },
      None => format!("v_{}", id.0),
    }
  }
}

// ---------------------------------------------------------------------------
// Collected data structures
// ---------------------------------------------------------------------------

/// A lifted function, ready to emit.
pub struct CollectedFn<'a, 'src> {
  /// WASM function label (e.g. "v_8").
  pub label: String,
  /// CpsId of the LetFn name — used to source-map the (func ...) header.
  pub fn_id: CpsId,
  /// Parameter (id, label) pairs in order (all anyref). Last is the cont.
  pub params: Vec<(CpsId, String)>,
  /// The fn body expression.
  pub body: &'a Expr<'src>,
  /// Whether this fn is exported under a user name.
  pub export_as: Option<String>,
  /// CpsId of the LetVal alias that names this export — used to source-map (export ...).
  pub export_bind_id: Option<CpsId>,
  /// LetVal alias for this fn (e.g. "add_0"), emitted as a global before (func ...).
  /// Set for all top-level LetVal aliases, not just exports.
  pub alias: Option<(CpsId, String)>,
}

/// Module-level collected data.
pub struct Module<'a, 'src> {
  pub funcs: Vec<CollectedFn<'a, 'src>>,
  /// All function arities encountered (= param count). Used to emit type section.
  pub arities: BTreeSet<usize>,
  /// CpsIds of LetVal aliases for module-level fns — these are globals, not locals.
  pub globals: HashSet<CpsId>,
}

// ---------------------------------------------------------------------------
// Collection logic
// ---------------------------------------------------------------------------

/// Walk the top-level chain and collect all lifted functions + the export list.
pub fn collect<'a, 'src>(root: &'a Expr<'src>, ctx: &IrCtx<'_, 'src>) -> Module<'a, 'src> {
  let mut funcs: Vec<CollectedFn<'a, 'src>> = Vec::new();
  let mut arities: BTreeSet<usize> = BTreeSet::new();

  let exports = collect_exports(root, ctx);
  collect_chain(root, ctx, &exports, &mut funcs, &mut arities);

  // Every module-level fn alias gets a global.
  let globals: HashSet<CpsId> = funcs.iter()
    .filter_map(|cf| cf.alias.as_ref().map(|(id, _)| *id))
    .collect();

  Module { funcs, arities, globals }
}

/// Scan the top-level chain for the terminal App and extract export pairs.
fn collect_exports<'src>(root: &Expr<'src>, ctx: &IrCtx<'_, 'src>) -> Vec<(CpsId, String)> {
  let mut expr = root;
  loop {
    match &expr.kind {
      ExprKind::LetFn { cont, .. } | ExprKind::LetVal { cont, .. } => {
        match cont {
          Cont::Expr { body, .. } => { expr = body; }
          Cont::Ref(_) => return vec![],
        }
      }
      ExprKind::App { func: Callable::BuiltIn(BuiltIn::Export), args } => {
        return args.iter().filter_map(|arg| {
          if let Arg::Val(v) = arg
            && let ValKind::Ref(Ref::Synth(id)) = v.kind {
              let name = export_name(ctx, id);
              return Some((id, name));
            }
          None
        }).collect();
      }
      _ => return vec![],
    }
  }
}

/// Extract the bare export name for a CpsId: the source Ident string, or "v_N".
pub fn export_name(ctx: &IrCtx<'_, '_>, id: CpsId) -> String {
  match ctx.ast_node(id) {
    Some(node) => match &node.kind {
      NodeKind::Ident(s) => s.to_string(),
      _ => format!("v_{}", id.0),
    },
    None => format!("v_{}", id.0),
  }
}

/// Recursively walk the top-level chain and populate `funcs`.
fn collect_chain<'a, 'src>(
  expr: &'a Expr<'src>,
  ctx: &IrCtx<'_, 'src>,
  exports: &[(CpsId, String)],
  funcs: &mut Vec<CollectedFn<'a, 'src>>,
  arities: &mut BTreeSet<usize>,
) {
  match &expr.kind {
    ExprKind::LetFn { name, params, fn_body, cont } => {
      let label = ctx.label(name.id);
      let param_labels: Vec<(CpsId, String)> = params.iter().map(|p| match p {
        Param::Name(b) => (b.id, ctx.label(b.id)),
        Param::Spread(b) => (b.id, ctx.label(b.id)),
      }).collect();
      arities.insert(param_labels.len());

      let export_as = exports.iter()
        .find(|(id, _)| *id == name.id)
        .map(|(_, n)| n.clone());

      funcs.push(CollectedFn { label, fn_id: name.id, params: param_labels, body: fn_body, export_as, export_bind_id: None, alias: None });

      match cont {
        Cont::Expr { body, .. } => collect_chain(body, ctx, exports, funcs, arities),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::LetVal { name, val, cont } => {
      if let ValKind::Ref(Ref::Synth(fn_id)) = val.kind
        && let Some(cf) = funcs.iter_mut().find(|cf| cf.label == ctx.label(fn_id))
      {
        cf.alias = Some((name.id, ctx.label(name.id)));
        if let Some((_, export_name)) = exports.iter().find(|(id, _)| *id == name.id) {
          cf.export_as = Some(export_name.clone());
          cf.export_bind_id = Some(name.id);
        }
      }
      match cont {
        Cont::Expr { body, .. } => collect_chain(body, ctx, exports, funcs, arities),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::App { .. } | ExprKind::If { .. } => {}
  }
}

// ---------------------------------------------------------------------------
// Local collection — pre-scan fn body for LetVal names
// ---------------------------------------------------------------------------

pub fn collect_locals<'src>(expr: &Expr<'_>, ctx: &IrCtx<'_, 'src>) -> Vec<String> {
  let mut locals = Vec::new();
  collect_locals_expr(expr, ctx, &mut locals);
  locals
}

fn collect_locals_expr<'src>(expr: &Expr<'_>, ctx: &IrCtx<'_, 'src>, out: &mut Vec<String>) {
  match &expr.kind {
    ExprKind::LetVal { name, cont, .. } => {
      out.push(ctx.label(name.id));
      match cont {
        Cont::Expr { body, .. } => collect_locals_expr(body, ctx, out),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::LetFn { cont, .. } => {
      match cont {
        Cont::Expr { body, .. } => collect_locals_expr(body, ctx, out),
        Cont::Ref(_) => {}
      }
    }
    ExprKind::If { then, else_, .. } => {
      collect_locals_expr(then, ctx, out);
      collect_locals_expr(else_, ctx, out);
    }
    ExprKind::App { func: Callable::BuiltIn(BuiltIn::FnClosure), args } => {
      for arg in args {
        if let Arg::Cont(Cont::Expr { args: bind_args, body }) = arg {
          for bind in bind_args {
            out.push(ctx.label(bind.id));
          }
          collect_locals_expr(body, ctx, out);
        }
      }
    }
    ExprKind::App { .. } => {}
  }
}

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

/// Split args into (value_args, Option<trailing_cont>).
pub fn split_args<'a>(args: &'a [Arg<'a>]) -> (&'a [Arg<'a>], Option<&'a Cont<'a>>) {
  match args.last() {
    Some(Arg::Cont(c)) => (&args[..args.len() - 1], Some(c)),
    _ => (args, None),
  }
}

// ---------------------------------------------------------------------------
// BuiltIn name mapping
// ---------------------------------------------------------------------------

pub fn builtin_name(op: BuiltIn) -> &'static str {
  match op {
    BuiltIn::Add      => "op_plus",
    BuiltIn::Sub      => "op_minus",
    BuiltIn::Mul      => "op_mul",
    BuiltIn::Div      => "op_div",
    BuiltIn::IntDiv   => "op_intdiv",
    BuiltIn::Mod      => "op_rem",
    BuiltIn::IntMod   => "op_intmod",
    BuiltIn::DivMod   => "op_divmod",
    BuiltIn::Pow      => "op_pow",
    BuiltIn::Eq       => "op_eq",
    BuiltIn::Neq      => "op_neq",
    BuiltIn::Lt       => "op_lt",
    BuiltIn::Lte      => "op_lte",
    BuiltIn::Gt       => "op_gt",
    BuiltIn::Gte      => "op_gte",
    BuiltIn::Cmp      => "op_cmp",
    BuiltIn::And      => "op_and",
    BuiltIn::Or       => "op_or",
    BuiltIn::Xor      => "op_xor",
    BuiltIn::Not      => "op_not",
    BuiltIn::BitAnd   => "op_bitand",
    BuiltIn::BitXor   => "op_bitxor",
    BuiltIn::Shl      => "op_shl",
    BuiltIn::Shr      => "op_shr",
    BuiltIn::RotL     => "op_rotl",
    BuiltIn::RotR     => "op_rotr",
    BuiltIn::BitNot   => "op_bitnot",
    BuiltIn::Range    => "op_rngex",
    BuiltIn::RangeIncl => "op_rngin",
    BuiltIn::In       => "op_in",
    BuiltIn::NotIn    => "op_notin",
    BuiltIn::Get      => "op_dot",
    BuiltIn::SeqAppend  => "seq_append",
    BuiltIn::SeqConcat  => "seq_concat",
    BuiltIn::RecPut     => "rec_put",
    BuiltIn::RecMerge   => "rec_merge",
    BuiltIn::StrFmt     => "str_fmt",
    BuiltIn::FnClosure  => "closure",
    BuiltIn::SeqPop        => "seq_pop",
    BuiltIn::RecPop        => "rec_pop",
    BuiltIn::Empty         => "empty",
    BuiltIn::MatchSeq      => "match_seq",
    BuiltIn::MatchNext     => "match_next",
    BuiltIn::MatchDone     => "match_done",
    BuiltIn::MatchNotDone  => "match_not_done",
    BuiltIn::MatchRest     => "match_rest",
    BuiltIn::MatchRec      => "match_rec",
    BuiltIn::MatchField    => "match_field",
    BuiltIn::Yield         => "yield",
    BuiltIn::Export        => "export",
    BuiltIn::Import        => "import",
  }
}
