// Partial application pass — desugars `?` (Partial nodes) into `Fn` nodes.
//
// e.g.  add 5, ?        =>  fn _0: add 5, _0
//       ? + 5           =>  fn _0: _0 + 5
//       ? + ?           =>  fn _0, _1: _0 + _1

use crate::transform::Transform;
