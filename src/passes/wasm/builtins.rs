// Built-in function implementations — emitted as defined WASM functions.
//
// Each builtin follows the CPS calling convention:
//   (func $op_plus (param $a (ref null any)) (param $b (ref null any)) (param $cont (ref null any))
//     ;; unbox args, compute, box result, tail-call cont
//   )
//
// The cont is a $Closure0 (or $ClosureN) — dispatched via _croc_1 or
// unboxed directly before return_call_ref.
//
// Type conventions:
//   - Numbers: $Num struct (f64 field)
//   - Booleans: i31ref (0 = false, 1 = true)
//   - Functions: $Closure0 (funcref field) or $ClosureN (funcref + captures)

use wasm_encoder::{AbstractHeapType, Function, HeapType, Instruction};

/// Indices into the type section, passed from the emitter.
pub struct TypeIndices {
  pub num: u32,
  pub closure: u32,
  pub captures: u32,
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

    // Comparison: unbox two $Num, f64 compare, box as i31ref (0 or 1).
    "op_eq"  => emit_binary_cmp(indices, Instruction::F64Eq),
    "op_neq" => emit_binary_cmp(indices, Instruction::F64Ne),
    "op_lt"  => emit_binary_cmp(indices, Instruction::F64Lt),
    "op_lte" => emit_binary_cmp(indices, Instruction::F64Le),
    "op_gt"  => emit_binary_cmp(indices, Instruction::F64Gt),
    "op_gte" => emit_binary_cmp(indices, Instruction::F64Ge),

    // Logic: i31ref bool ops.
    "op_not" => emit_unary_not(indices),
    "op_and" => emit_binary_bool(indices, Instruction::I32And),
    "op_or"  => emit_binary_bool(indices, Instruction::I32Or),
    "op_xor" => emit_binary_bool(indices, Instruction::I32Xor),

    _ => panic!("builtin '{}' not implemented", name),
  }
}

// ---------------------------------------------------------------------------
// Shared tail: unbox cont from $Closure0, return_call_ref $Fn1.
// Assumes the result value is already on the stack.
// ---------------------------------------------------------------------------

fn emit_cont_call(f: &mut Function, idx: &TypeIndices, cont_param: u32) {
  // Stack: [result]. Tail-call the continuation.
  if let Some(croc1) = idx.croc1 {
    // Dispatch through $_croc_1 — handles closures with any capture count.
    f.instruction(&Instruction::LocalGet(cont_param));
    f.instruction(&Instruction::ReturnCall(croc1));
  } else {
    // No closures — direct $Closure unbox (captures are null).
    f.instruction(&Instruction::LocalGet(cont_param));
    f.instruction(&Instruction::RefCastNonNull(HeapType::Concrete(idx.closure)));
    f.instruction(&Instruction::StructGet { struct_type_index: idx.closure, field_index: 0 });
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
// Binary comparison: (a, b, cont) → cont(i31(unbox(a) CMP unbox(b)))
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
  // Compare → i32 (0 or 1), box as i31ref.
  f.instruction(&op);
  f.instruction(&Instruction::RefI31);
  emit_cont_call(&mut f, idx, 2);
  f
}

// ---------------------------------------------------------------------------
// Binary bool: (a, b, cont) → cont(i31(i31.get_s(a) OP i31.get_s(b)))
// ---------------------------------------------------------------------------

fn emit_binary_bool(idx: &TypeIndices, op: Instruction<'_>) -> Function {
  let mut f = Function::new(vec![]);
  let i31_ht = HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 };
  // Unbox a: ref.cast i31, i31.get_s → i32.
  f.instruction(&Instruction::LocalGet(0));
  f.instruction(&Instruction::RefCastNonNull(i31_ht));
  f.instruction(&Instruction::I31GetS);
  // Unbox b: ref.cast i31, i31.get_s → i32.
  f.instruction(&Instruction::LocalGet(1));
  f.instruction(&Instruction::RefCastNonNull(i31_ht));
  f.instruction(&Instruction::I31GetS);
  // Op + box as i31ref.
  f.instruction(&op);
  f.instruction(&Instruction::RefI31);
  emit_cont_call(&mut f, idx, 2);
  f
}

// ---------------------------------------------------------------------------
// Unary not: (a, cont) → cont(i31(i31.get_s(a) == 0 ? 1 : 0))
// ---------------------------------------------------------------------------

fn emit_unary_not(idx: &TypeIndices) -> Function {
  let mut f = Function::new(vec![]);
  let i31_ht = HeapType::Abstract { shared: false, ty: AbstractHeapType::I31 };
  // Unbox a: ref.cast i31, i31.get_s → i32.
  f.instruction(&Instruction::LocalGet(0));
  f.instruction(&Instruction::RefCastNonNull(i31_ht));
  f.instruction(&Instruction::I31GetS);
  // i32: 0 → 1, nonzero → 0. Box as i31ref.
  f.instruction(&Instruction::I32Eqz);
  f.instruction(&Instruction::RefI31);
  // not has 2 params (a, cont), cont is param 1.
  emit_cont_call(&mut f, idx, 1);
  f
}
