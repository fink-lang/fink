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
        LetRec { .. } => "LetRec",
        App { .. } => "App",
        If { .. } => "If",
        Panic => "Panic",
        FailCont => "FailCont",
        MatchLetVal { .. } => "MatchLetVal",
        MatchApp { .. } => "MatchApp",
        MatchIf { .. } => "MatchIf",
        MatchValue { .. } => "MatchValue",
        MatchSeq { .. } => "MatchSeq",
        MatchNext { .. } => "MatchNext",
        MatchDone { .. } => "MatchDone",
        MatchNotDone { .. } => "MatchNotDone",
        MatchRest { .. } => "MatchRest",
        MatchRec { .. } => "MatchRec",
        MatchField { .. } => "MatchField",
        MatchBlock { .. } => "MatchBlock",
        Yield { .. } => "Yield",
    }
}

fn dump_val(v: &fink::passes::cps::ir::Val, depth: usize) {
    use fink::passes::cps::ir::ValKind::*;
    let i = indent(depth);
    match &v.kind {
        Ref(r) => println!("{i}Val(#{}) Ref({:?})", v.id.0, r),
        Lit(l) => println!("{i}Val(#{}) Lit({:?})", v.id.0, l),
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
        LetRec { bindings, body } => {
            println!("{i}Expr(#{}) LetRec ({} bindings)", e.id.0, bindings.len());
            for b in bindings {
                let p: Vec<_> = b.params.iter().map(|p| format!("{:?}", p)).collect();
                println!("{i}  binding {:?} [{}]:", b.name, p.join(", "));
                dump_expr(&b.fn_body, depth+2);
            }
            println!("{i}  rec_body:");
            if let fink::passes::cps::ir::Cont::Expr { body: inner, .. } = body {
                dump_expr(inner, depth+1);
            }
        }
        App { func, args, cont } => {
            println!("{i}Expr(#{}) App", e.id.0);
            match func {
                fink::passes::cps::ir::Callable::Val(v) => dump_val(v, depth+1),
                fink::passes::cps::ir::Callable::BuiltIn(b) => println!("{}  BuiltIn({:?})", i, b),
            }
            for a in args {
                match a {
                    fink::passes::cps::ir::Arg::Val(v) => dump_val(v, depth+1),
                    fink::passes::cps::ir::Arg::Spread(v) => dump_val(v, depth+1),
                }
            }
            if let fink::passes::cps::ir::Cont::Expr { args, body } = cont {
                println!("{i}  cont args={:?}:", args);
                dump_expr(body, depth+1);
            }
        }
        If { cond, then, else_ } => {
            println!("{i}Expr(#{}) If", e.id.0);
            dump_val(cond, depth+1);
            dump_expr(then, depth+1);
            dump_expr(else_, depth+1);
        }
        Panic => println!("{i}Expr(#{}) Panic", e.id.0),
        FailCont => println!("{i}Expr(#{}) FailCont", e.id.0),
        MatchLetVal { name, val, fail, body } => {
            println!("{i}Expr(#{}) MatchLetVal {:?}", e.id.0, name);
            dump_val(val, depth+1);
            dump_expr(fail, depth+1);
            if let fink::passes::cps::ir::Cont::Expr { body: inner, .. } = body {
                dump_expr(inner, depth+1);
            }
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
