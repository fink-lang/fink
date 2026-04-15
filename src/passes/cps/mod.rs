pub mod ir;
// `fmt` depends on `ast::fmt`, which is gated until its 750-line
// s-expression printer is ported. Comes back together with that.
#[cfg(not(feature = "flat-ast-wip"))]
pub mod fmt;
pub mod transform;
