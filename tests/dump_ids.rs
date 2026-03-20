// Temporary test to dump CPS with IDs

#[test]
fn dump_cps_ids() {
    let src = "a = 2\n\nfoo = fn b:\n  b + bar a\n\n\nbar = fn d:\n  d * e\n";
    let r = fink::parser::parse(src).unwrap();
    let cps = fink::passes::cps::transform::lower_expr(&r.root);
    println!("\nnode_count: {}", cps.origin.len());
    dump_expr(&cps.root, 0);
}

fn indent(depth: usize) -> String { "  ".repeat(depth) }

fn variant_name(e: &fink::passes::cps::ir::Expr) -> &'static str {
    use fink::passes::cps::ir::ExprKind::*;
    match &e.kind {
        LetVal { .. } => "LetVal",
        LetFn { .. } => "LetFn",
        App { .. } => "App",
        If { .. } => "If",
        Yield { .. } => "Yield",
    }
}

fn dump_val(v: &fink::passes::cps::ir::Val, depth: usize) {
    use fink::passes::cps::ir::ValKind::*;
    let i = indent(depth);
    match &v.kind {
        Ref(r) => println!("{i}Val(#{}) Ref({:?})", v.id.0, r),
        Lit(l) => println!("{i}Val(#{}) Lit({:?})", v.id.0, l),
        Panic => println!("{i}Val(#{}) Panic", v.id.0),
        ContRef(id) => println!("{i}Val(#{}) ContRef(#{})", v.id.0, id.0),
        BuiltIn(op) => println!("{i}Val(#{}) BuiltIn({:?})", v.id.0, op),
    }
}

fn dump_expr(e: &fink::passes::cps::ir::Expr, depth: usize) {
    use fink::passes::cps::ir::ExprKind::*;
    let i = indent(depth);
    match &e.kind {
        LetVal { name, val, body } => {
            println!("{i}Expr(#{}) LetVal {:?}", e.id.0, name);
            dump_val(val, depth+1);
            if let fink::passes::cps::ir::Cont::Expr { body: inner, .. } = body {
                dump_expr(inner, depth+1);
            }
        }
        LetFn { name, params, fn_body, body, .. } => {
            let p: Vec<_> = params.iter().map(|p| format!("{:?}", p)).collect();
            println!("{i}Expr(#{}) LetFn {:?} [{}]", e.id.0, name, p.join(", "));
            println!("{i}  fn_body:");
            dump_expr(fn_body, depth+2);
            println!("{i}  body:");
            if let fink::passes::cps::ir::Cont::Expr { body: inner, .. } = body {
                dump_expr(inner, depth+1);
            }
        }
        App { func, args } => {
            println!("{i}Expr(#{}) App", e.id.0);
            match func {
                fink::passes::cps::ir::Callable::Val(v) => dump_val(v, depth+1),
                fink::passes::cps::ir::Callable::BuiltIn(b) => println!("{}  BuiltIn({:?})", i, b),
            }
            for a in args {
                match a {
                    fink::passes::cps::ir::Arg::Val(v) => dump_val(v, depth+1),
                    fink::passes::cps::ir::Arg::Spread(v) => dump_val(v, depth+1),
                    fink::passes::cps::ir::Arg::Cont(c) => {
                        if let fink::passes::cps::ir::Cont::Expr { args: ca, body } = c {
                            println!("{i}  cont args={:?}:", ca);
                            dump_expr(body, depth+1);
                        }
                    }
                    fink::passes::cps::ir::Arg::Expr(e) => dump_expr(e, depth+1),
                }
            }
        }
        If { cond, then, else_ } => {
            println!("{i}Expr(#{}) If", e.id.0);
            dump_val(cond, depth+1);
            dump_expr(then, depth+1);
            dump_expr(else_, depth+1);
        }
        Yield { value, cont } => {
            println!("{i}Expr(#{}) Yield", e.id.0);
            dump_val(value, depth+1);
            if let fink::passes::cps::ir::Cont::Expr { args, body } = cont {
                println!("{i}  cont args={:?}:", args);
                dump_expr(body, depth+1);
            }
        }
        _ => println!("{i}Expr(#{}) <other:{}>", e.id.0, variant_name(e)),
    }
}
