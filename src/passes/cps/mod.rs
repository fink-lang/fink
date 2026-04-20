//! CPS intermediate representation — AST → CPS lowering and the IR itself.
//!
//! Every intermediate result gets an explicit name; control flow is
//! explicit via continuations; metadata lives in property graphs keyed
//! by [`ir::CpsId`], not on IR nodes.
//!
//! See `ir-design.md`, `transform-contract.md`, and
//! `node-unification.md` next to this module.

pub mod ir;
pub mod fmt;
pub mod transform;
