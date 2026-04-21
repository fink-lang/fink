//! IR-level linker — merges user-level `Fragment`s into a single
//! linked `Fragment`.
//!
//! # Scope
//!
//! * **Input:** a list of user-level Fragments. The list is expected
//!   to be the result of walking the source-import graph — every
//!   dep Fragment the user sources pull in is included; cross-
//!   fragment user imports (`./foo.fnk:bar`, `std/io:stdin`,
//!   `https://…`, `reg:…`) resolve against *other Fragments in the
//!   list*.
//! * **Output:** one linked Fragment. User-to-user imports are
//!   resolved (rewritten to merged symbol ids). `rt/*` imports —
//!   the compiler's runtime ABI — pass through **unchanged**;
//!   resolving those is `ir_emit`'s job, against whatever runtime
//!   backend is in play.
//!
//! The linker is pure IR → IR. It does not touch `wasm-encoder`,
//! does not parse `runtime.wasm`, does not emit bytes. Output is
//! still a Fragment, still formattable via `fmt_fragment`.
//!
//! # Current coverage (tracer phase 0)
//!
//! Single-fragment passthrough. Multi-fragment merge is a future
//! step — tracer programs today are single-module. The API takes a
//! slice so when that step lands it's a non-breaking change.

use super::ir::Fragment;

/// Link a set of user-level Fragments into a single linked Fragment.
///
/// Tracer-phase stub: supports only `fragments.len() == 1`. Panics
/// with a clear message on multi-fragment input; add merge logic
/// when the first multi-module tracer test demands it.
pub fn link(fragments: &[Fragment]) -> Fragment {
  match fragments {
    [only] => only.clone(),
    [] => panic!("ir_link: empty fragment list"),
    _ => panic!(
      "ir_link: multi-fragment linking not yet implemented \
       (got {} fragments). Tracer currently supports single-module \
       programs only; add merge logic when first multi-module \
       fixture lands.",
      fragments.len()
    ),
  }
}
