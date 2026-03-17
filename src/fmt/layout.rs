// Stage 2 — AST → AST (formatter)
//
// Takes an input AST (possibly with synthetic or conflicting locs) and
// produces a new Node tree with canonical locs that satisfy the formatting
// rules in FmtConfig. Also produces a PropGraph<FmtId, Option<AstId>>
// origin map tracing each output node back to its input counterpart.
//
// Not yet implemented.
