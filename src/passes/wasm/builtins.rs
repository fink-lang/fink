// Built-in function implementations — emitted as defined WASM functions.
//
// Each builtin follows the CPS calling convention:
//   (func $op_plus (param $a (ref null $Any)) (param $b (ref null $Any)) (param $cont (ref null $Any))
//     ;; unbox args, compute, box result, tail-call cont
//   )
//
// The cont is a $FuncBox — must be unboxed before return_call_ref.

use wasm_encoder::{Function, HeapType, Instruction};

/// Indices into the type section, passed from the emitter.
pub struct TypeIndices {
  pub any: u32,
  pub num: u32,
  pub bool_: u32,
  pub funcbox: u32,
  pub fn1: u32,
  /// Function index of $_croc_1 dispatch helper, if closures exist.
  pub croc1: Option<u32>,
}

/// Check if a builtin has a known WASM implementation.
pub fn is_implemented(name: &str) -> bool {
  matches!(name,
    // Arithmetic
    "op_plus" | "op_minus" | "op_mul" | "op_div"
    | "op_intdiv" | "op_rem" | "op_intmod"
    // Comparison
    | "op_eq" | "op_neq" | "op_lt" | "op_lte" | "op_gt" | "op_gte"
    // Logic
    | "op_not" | "op_and" | "op_or" | "op_xor"
  )
}

/// Emit a builtin body. Panics if the builtin is not implemented.
pub fn emit_builtin(name: &str, indices: &TypeIndices) -> Function {
  match name {
    // Arithmetic: unbox two $Num, f64 op, box result.
    "op_plus"  => emit_binary_arith(indices, Instruction::F64Add),
    "op_minus" => emit_binary_arith(indices, Instruction::F64Sub),
    "op_mul"   => emit_binary_arith(indices, Instruction::F64Mul),
    "op_div"   => emit_binary_arith(indices, Instruction::F64Div),

    // Integer arithmetic: cast to i64, op, cast back.
    "op_intdiv" => emit_binary_int(indices, Instruction::I64DivS),
    "op_rem"    => emit_binary_int(indices, Instruction::I64RemS),
    "op_intmod" => emit_binary_int(indices, Instruction::I64RemS), // same as rem for now

    // Comparison: unbox two $Num, f64 compare, box 1.0 or 0.0.
    "op_eq"  => emit_binary_cmp(indices, Instruction::F64Eq),
    "op_neq" => emit_binary_cmp(indices, Instruction::F64Ne),
    "op_lt"  => emit_binary_cmp(indices, Instruction::F64Lt),
    "op_lte" => emit_binary_cmp(indices, Instruction::F64Le),
    "op_gt"  => emit_binary_cmp(indices, Instruction::F64Gt),
    "op_gte" => emit_binary_cmp(indices, Instruction::F64Ge),

    // Logic: $Bool ops.
    "op_not" => emit_unary_not(indices),
    "op_and" => emit_binary_bool(indices, Instruction::I32And),
    "op_or"  => emit_binary_bool(indices, Instruction::I32Or),
    "op_xor" => emit_binary_bool(indices, Instruction::I32Xor),

    _ => panic!("builtin '{}' not implemented", name),
  }
}

// ---------------------------------------------------------------------------
// Shared tail: unbox cont from $FuncBox, return_call_ref $Fn1.
// Assumes the result value is already on the stack.
// ---------------------------------------------------------------------------

fn emit_cont_call(f: &mut Function, idx: &TypeIndices, cont_param: u32) {
  // Stack: [result]. Tail-call the continuation.
  if let Some(croc1) = idx.croc1 {
    // Dispatch through $_croc_1 — handles both $FuncBox and $ClosureN.
    f.instruction(&Instruction::LocalGet(cont_param));
    f.instruction(&Instruction::ReturnCall(croc1));
  } else {
    // No closures — direct $FuncBox unbox.
    f.instruction(&Instruction::LocalGet(cont_param));
    f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(idx.funcbox)));
    f.instruction(&Instruction::StructGet { struct_type_index: idx.funcbox, field_index: 0 });
    f.instruction(&Instruction::RefCastNullable(HeapType::Concrete(idx.fn1)));
    f.instruction(&Instruction::ReturnCallRef(idx.fn1));
  }
  f.instruction(&Instruction::End);
}

// ---------------------------------------------------------------------------
// Binary arithmetic: (a, b, cont) → cont(box(unbox(a) OP unbox(b)))
// ---------------------------------------------------------------------------

fn emit_binary_arith(idx: &TypeIndices, op: Instruction<'_>) -> Function {
  let mut f = Function::new(vec![]);
  // Unbox a.
  f.instruction(&Instruction::LocalGet(0));
  f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(idx.num)));
  f.instruction(&Instruction::StructGet { struct_type_index: idx.num, field_index: 0 });
  // Unbox b.
  f.instruction(&Instruction::LocalGet(1));
  f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(idx.num)));
  f.instruction(&Instruction::StructGet { struct_type_index: idx.num, field_index: 0 });
  // Op + box.
  f.instruction(&op);
  f.instruction(&Instruction::StructNew(idx.num));
  emit_cont_call(&mut f, idx, 2);
  f
}

// ---------------------------------------------------------------------------
// Binary integer arithmetic: (a, b, cont) → cont(box(i64_op(trunc(a), trunc(b))))
// ---------------------------------------------------------------------------

fn emit_binary_int(idx: &TypeIndices, op: Instruction<'_>) -> Function {
  let mut f = Function::new(vec![]);
  // Unbox a → f64 → i64.
  f.instruction(&Instruction::LocalGet(0));
  f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(idx.num)));
  f.instruction(&Instruction::StructGet { struct_type_index: idx.num, field_index: 0 });
  f.instruction(&Instruction::I64TruncF64S);
  // Unbox b → f64 → i64.
  f.instruction(&Instruction::LocalGet(1));
  f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(idx.num)));
  f.instruction(&Instruction::StructGet { struct_type_index: idx.num, field_index: 0 });
  f.instruction(&Instruction::I64TruncF64S);
  // i64 op → f64 → box.
  f.instruction(&op);
  f.instruction(&Instruction::F64ConvertI64S);
  f.instruction(&Instruction::StructNew(idx.num));
  emit_cont_call(&mut f, idx, 2);
  f
}

// ---------------------------------------------------------------------------
// Binary comparison: (a, b, cont) → cont(box(unbox(a) CMP unbox(b) ? 1.0 : 0.0))
// ---------------------------------------------------------------------------

fn emit_binary_cmp(idx: &TypeIndices, op: Instruction<'_>) -> Function {
  let mut f = Function::new(vec![]);
  // Unbox a.
  f.instruction(&Instruction::LocalGet(0));
  f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(idx.num)));
  f.instruction(&Instruction::StructGet { struct_type_index: idx.num, field_index: 0 });
  // Unbox b.
  f.instruction(&Instruction::LocalGet(1));
  f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(idx.num)));
  f.instruction(&Instruction::StructGet { struct_type_index: idx.num, field_index: 0 });
  // Compare → i32 (0 or 1), box as $Bool.
  f.instruction(&op);
  f.instruction(&Instruction::StructNew(idx.bool_));
  emit_cont_call(&mut f, idx, 2);
  f
}

// ---------------------------------------------------------------------------
// Binary bool: (a, b, cont) → cont(box(unbox(a) OP unbox(b)))
// ---------------------------------------------------------------------------

fn emit_binary_bool(idx: &TypeIndices, op: Instruction<'_>) -> Function {
  let mut f = Function::new(vec![]);
  // Unbox a.
  f.instruction(&Instruction::LocalGet(0));
  f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(idx.bool_)));
  f.instruction(&Instruction::StructGet { struct_type_index: idx.bool_, field_index: 0 });
  // Unbox b.
  f.instruction(&Instruction::LocalGet(1));
  f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(idx.bool_)));
  f.instruction(&Instruction::StructGet { struct_type_index: idx.bool_, field_index: 0 });
  // Op + box.
  f.instruction(&op);
  f.instruction(&Instruction::StructNew(idx.bool_));
  emit_cont_call(&mut f, idx, 2);
  f
}

// ---------------------------------------------------------------------------
// Unary not: (a, cont) → cont(box(unbox(a) == 0.0 ? 1.0 : 0.0))
// ---------------------------------------------------------------------------

fn emit_unary_not(idx: &TypeIndices) -> Function {
  let mut f = Function::new(vec![]);
  // Unbox a — could be $Bool (i32) or $Num (f64). Try $Bool first.
  // For now, assume $Bool input: struct.get $Bool 0.
  f.instruction(&Instruction::LocalGet(0));
  f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(idx.bool_)));
  f.instruction(&Instruction::StructGet { struct_type_index: idx.bool_, field_index: 0 });
  // i32: 0 → 1, nonzero → 0.
  f.instruction(&Instruction::I32Eqz);
  f.instruction(&Instruction::StructNew(idx.bool_));
  // not has 2 params (a, cont), cont is param 1.
  emit_cont_call(&mut f, idx, 1);
  f
}
