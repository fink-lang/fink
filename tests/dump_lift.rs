#[test]
fn dump() {
    let src = "fib = fn n:\n  r = match n:\n    0: 1\n    _: fib n + 1\n  r";
    let r = fink::parser::parse(src).unwrap();
    let ai = fink::ast::build_index(&r);
    let cps = fink::passes::cps::transform::lower_expr(&r.root);
    let lifted = fink::passes::lifting::lift(cps, &ai);
    let ctx = fink::passes::cps::fmt::Ctx { origin: &lifted.origin, ast_index: &ai, captures: None };
    eprintln!("{}", fink::passes::lifting::fmt::fmt_flat(&lifted.root, &ctx));
}
