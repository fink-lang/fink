// Runtime support modules for fink's WASM output.
//
// These are standalone WAT modules that implement core data structures
// (HAMT for records/dicts, cons lists, etc.) using WASM GC types.
// The compiler's codegen emits calls into these runtime functions.

#[cfg(test)]
mod tests {
  use wasmtime::*;

  /// Load the HAMT module and return (store, instance).
  fn load_hamt() -> (Store<()>, Instance) {
    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    config.wasm_multi_value(true);

    let engine = Engine::new(&config).unwrap();
    let wat = include_bytes!("hamt.wat");
    let module = Module::new(&engine, &wat[..]).unwrap();
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    (store, instance)
  }

  /// Create an i31ref key from an integer.
  fn i31_key(store: &mut Store<()>, val: u32) -> Val {
    let any = AnyRef::from_i31(&mut *store, I31::wrapping_u32(val));
    Val::AnyRef(Some(any))
  }

  /// Extract an i31 value from an AnyRef Val, or None.
  fn get_i31(store: &Store<()>, val: &Val) -> Option<u32> {
    match val {
      Val::AnyRef(Some(any)) => {
        any.as_i31(&*store).ok().flatten().map(|i| i.get_u32())
      }
      _ => None,
    }
  }

  /// Helper: call hamt_set(node, key, val) → new_node
  fn hamt_set(store: &mut Store<()>, set_fn: &Func, node: &Val, key: u32, val: u32) -> Val {
    let k = i31_key(store, key);
    let v = i31_key(store, val);
    let mut result = [Val::AnyRef(None)];
    set_fn.call(store, &[node.clone(), k, v], &mut result).unwrap();
    result[0].clone()
  }

  /// Helper: call hamt_get(node, key) → val or None
  fn hamt_get(store: &mut Store<()>, get_fn: &Func, node: &Val, key: u32) -> Option<u32> {
    let k = i31_key(store, key);
    let mut result = [Val::AnyRef(None)];
    get_fn.call(&mut *store, &[node.clone(), k], &mut result).unwrap();
    get_i31(store, &result[0])
  }

  /// Helper: call hamt_delete(node, key) → new_node
  fn hamt_delete(store: &mut Store<()>, del_fn: &Func, node: &Val, key: u32) -> Val {
    let k = i31_key(store, key);
    let mut result = [Val::AnyRef(None)];
    del_fn.call(store, &[node.clone(), k], &mut result).unwrap();
    result[0].clone()
  }

  /// Helper: call hamt_pop(node, key) → (val, rest_node)
  fn hamt_pop(store: &mut Store<()>, pop_fn: &Func, node: &Val, key: u32) -> (Option<u32>, Val) {
    let k = i31_key(store, key);
    let mut result = [Val::AnyRef(None), Val::AnyRef(None)];
    pop_fn.call(&mut *store, &[node.clone(), k], &mut result).unwrap();
    (get_i31(store, &result[0]), result[1].clone())
  }

  /// Helper: call hamt_merge(dest, src) → merged
  fn hamt_merge(store: &mut Store<()>, merge_fn: &Func, dest: &Val, src: &Val) -> Val {
    let mut result = [Val::AnyRef(None)];
    merge_fn.call(store, &[dest.clone(), src.clone()], &mut result).unwrap();
    result[0].clone()
  }

  /// Helper: call hamt_size(node) → i32
  fn hamt_size(store: &mut Store<()>, size_fn: &Func, node: &Val) -> i32 {
    let mut result = [Val::I32(0)];
    size_fn.call(store, &[node.clone()], &mut result).unwrap();
    match &result[0] { Val::I32(n) => *n, _ => panic!("expected i32") }
  }

  /// Helper: call hamt_empty() → node
  fn hamt_empty(store: &mut Store<()>, empty_fn: &Func) -> Val {
    let mut result = [Val::AnyRef(None)];
    empty_fn.call(store, &[], &mut result).unwrap();
    result[0].clone()
  }

  #[test]
  fn test_empty() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let node = hamt_empty(&mut store, &empty_fn);
    assert!(matches!(&node, Val::AnyRef(Some(_))));
  }

  #[test]
  fn test_set_and_get() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let get_fn = instance.get_func(&mut store, "hamt_get").unwrap();

    let node = hamt_empty(&mut store, &empty_fn);
    let node = hamt_set(&mut store, &set_fn, &node, 1, 10);
    let node = hamt_set(&mut store, &set_fn, &node, 2, 20);

    assert_eq!(hamt_get(&mut store, &get_fn, &node, 1), Some(10));
    assert_eq!(hamt_get(&mut store, &get_fn, &node, 2), Some(20));
    assert_eq!(hamt_get(&mut store, &get_fn, &node, 99), None);
  }

  #[test]
  fn test_set_overwrite() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let get_fn = instance.get_func(&mut store, "hamt_get").unwrap();

    let node = hamt_empty(&mut store, &empty_fn);
    let v1 = hamt_set(&mut store, &set_fn, &node, 1, 10);
    let v2 = hamt_set(&mut store, &set_fn, &v1, 1, 99);

    // v2 has the new value.
    assert_eq!(hamt_get(&mut store, &get_fn, &v2, 1), Some(99));
    // v1 still has the old value (structural sharing).
    assert_eq!(hamt_get(&mut store, &get_fn, &v1, 1), Some(10));
  }

  #[test]
  fn test_delete() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let get_fn = instance.get_func(&mut store, "hamt_get").unwrap();
    let del_fn = instance.get_func(&mut store, "hamt_delete").unwrap();

    let node = hamt_empty(&mut store, &empty_fn);
    let node = hamt_set(&mut store, &set_fn, &node, 1, 10);
    let node = hamt_set(&mut store, &set_fn, &node, 2, 20);
    let node = hamt_set(&mut store, &set_fn, &node, 3, 30);

    let deleted = hamt_delete(&mut store, &del_fn, &node, 2);

    assert_eq!(hamt_get(&mut store, &get_fn, &deleted, 1), Some(10));
    assert_eq!(hamt_get(&mut store, &get_fn, &deleted, 2), None);
    assert_eq!(hamt_get(&mut store, &get_fn, &deleted, 3), Some(30));

    // Original still has key=2.
    assert_eq!(hamt_get(&mut store, &get_fn, &node, 2), Some(20));
  }

  #[test]
  fn test_delete_absent_key() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let get_fn = instance.get_func(&mut store, "hamt_get").unwrap();
    let del_fn = instance.get_func(&mut store, "hamt_delete").unwrap();

    let node = hamt_empty(&mut store, &empty_fn);
    let node = hamt_set(&mut store, &set_fn, &node, 1, 10);
    let deleted = hamt_delete(&mut store, &del_fn, &node, 99);

    assert_eq!(hamt_get(&mut store, &get_fn, &deleted, 1), Some(10));
  }

  #[test]
  fn test_pop() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let get_fn = instance.get_func(&mut store, "hamt_get").unwrap();
    let pop_fn = instance.get_func(&mut store, "hamt_pop").unwrap();

    let node = hamt_empty(&mut store, &empty_fn);
    let node = hamt_set(&mut store, &set_fn, &node, 1, 10);
    let node = hamt_set(&mut store, &set_fn, &node, 2, 20);
    let node = hamt_set(&mut store, &set_fn, &node, 3, 30);

    // Pop key=2 → (20, rest).
    let (val, rest) = hamt_pop(&mut store, &pop_fn, &node, 2);
    assert_eq!(val, Some(20));

    assert_eq!(hamt_get(&mut store, &get_fn, &rest, 1), Some(10));
    assert_eq!(hamt_get(&mut store, &get_fn, &rest, 2), None);
    assert_eq!(hamt_get(&mut store, &get_fn, &rest, 3), Some(30));
  }

  #[test]
  fn test_pop_absent() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let pop_fn = instance.get_func(&mut store, "hamt_pop").unwrap();

    let node = hamt_empty(&mut store, &empty_fn);
    let node = hamt_set(&mut store, &set_fn, &node, 1, 10);

    let (val, _rest) = hamt_pop(&mut store, &pop_fn, &node, 99);
    assert_eq!(val, None);
  }

  #[test]
  fn test_many_keys() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let get_fn = instance.get_func(&mut store, "hamt_get").unwrap();

    let mut node = hamt_empty(&mut store, &empty_fn);
    for i in 0..100u32 {
      node = hamt_set(&mut store, &set_fn, &node, i, i * 10);
    }

    for i in 0..100u32 {
      assert_eq!(
        hamt_get(&mut store, &get_fn, &node, i), Some(i * 10),
        "key {} should map to {}", i, i * 10
      );
    }
  }

  #[test]
  fn test_structural_sharing() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let get_fn = instance.get_func(&mut store, "hamt_get").unwrap();

    let node = hamt_empty(&mut store, &empty_fn);
    let v1 = hamt_set(&mut store, &set_fn, &node, 1, 10);
    let v1 = hamt_set(&mut store, &set_fn, &v1, 2, 20);

    // Fork: v2 adds key=3.
    let v2 = hamt_set(&mut store, &set_fn, &v1, 3, 30);

    // v1 should NOT have key=3.
    assert_eq!(hamt_get(&mut store, &get_fn, &v1, 3), None);

    // v2 should have all three.
    assert_eq!(hamt_get(&mut store, &get_fn, &v2, 1), Some(10));
    assert_eq!(hamt_get(&mut store, &get_fn, &v2, 2), Some(20));
    assert_eq!(hamt_get(&mut store, &get_fn, &v2, 3), Some(30));
  }

  #[test]
  fn test_chained_pop_destructure() {
    // Simulates: {a, b, ...rest} = {1: 10, 2: 20, 3: 30}
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let get_fn = instance.get_func(&mut store, "hamt_get").unwrap();
    let pop_fn = instance.get_func(&mut store, "hamt_pop").unwrap();

    let node = hamt_empty(&mut store, &empty_fn);
    let node = hamt_set(&mut store, &set_fn, &node, 1, 10);
    let node = hamt_set(&mut store, &set_fn, &node, 2, 20);
    let node = hamt_set(&mut store, &set_fn, &node, 3, 30);

    // Pop key=1 → (10, tmp)
    let (a, tmp) = hamt_pop(&mut store, &pop_fn, &node, 1);
    assert_eq!(a, Some(10));

    // Pop key=2 from tmp → (20, rest)
    let (b, rest) = hamt_pop(&mut store, &pop_fn, &tmp, 2);
    assert_eq!(b, Some(20));

    // rest should only have key=3.
    assert_eq!(hamt_get(&mut store, &get_fn, &rest, 1), None);
    assert_eq!(hamt_get(&mut store, &get_fn, &rest, 2), None);
    assert_eq!(hamt_get(&mut store, &get_fn, &rest, 3), Some(30));
  }

  #[test]
  fn test_merge_disjoint() {
    // {1: 10, 2: 20} merge {3: 30, 4: 40} → {1: 10, 2: 20, 3: 30, 4: 40}
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let get_fn = instance.get_func(&mut store, "hamt_get").unwrap();
    let merge_fn = instance.get_func(&mut store, "hamt_merge").unwrap();

    let node = hamt_empty(&mut store, &empty_fn);
    let a = hamt_set(&mut store, &set_fn, &node, 1, 10);
    let a = hamt_set(&mut store, &set_fn, &a, 2, 20);

    let b = hamt_set(&mut store, &set_fn, &node, 3, 30);
    let b = hamt_set(&mut store, &set_fn, &b, 4, 40);

    let merged = hamt_merge(&mut store, &merge_fn, &a, &b);

    assert_eq!(hamt_get(&mut store, &get_fn, &merged, 1), Some(10));
    assert_eq!(hamt_get(&mut store, &get_fn, &merged, 2), Some(20));
    assert_eq!(hamt_get(&mut store, &get_fn, &merged, 3), Some(30));
    assert_eq!(hamt_get(&mut store, &get_fn, &merged, 4), Some(40));
  }

  #[test]
  fn test_merge_overlap_src_wins() {
    // {1: 10, 2: 20} merge {2: 99, 3: 30} → {1: 10, 2: 99, 3: 30}
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let get_fn = instance.get_func(&mut store, "hamt_get").unwrap();
    let merge_fn = instance.get_func(&mut store, "hamt_merge").unwrap();

    let node = hamt_empty(&mut store, &empty_fn);
    let a = hamt_set(&mut store, &set_fn, &node, 1, 10);
    let a = hamt_set(&mut store, &set_fn, &a, 2, 20);

    let b = hamt_set(&mut store, &set_fn, &node, 2, 99);
    let b = hamt_set(&mut store, &set_fn, &b, 3, 30);

    let merged = hamt_merge(&mut store, &merge_fn, &a, &b);

    assert_eq!(hamt_get(&mut store, &get_fn, &merged, 1), Some(10));
    assert_eq!(hamt_get(&mut store, &get_fn, &merged, 2), Some(99)); // src wins
    assert_eq!(hamt_get(&mut store, &get_fn, &merged, 3), Some(30));
  }

  #[test]
  fn test_merge_into_empty() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let get_fn = instance.get_func(&mut store, "hamt_get").unwrap();
    let merge_fn = instance.get_func(&mut store, "hamt_merge").unwrap();

    let empty = hamt_empty(&mut store, &empty_fn);
    let b = hamt_set(&mut store, &set_fn, &empty, 1, 10);
    let b = hamt_set(&mut store, &set_fn, &b, 2, 20);

    let merged = hamt_merge(&mut store, &merge_fn, &empty, &b);

    assert_eq!(hamt_get(&mut store, &get_fn, &merged, 1), Some(10));
    assert_eq!(hamt_get(&mut store, &get_fn, &merged, 2), Some(20));
  }

  #[test]
  fn test_merge_preserves_originals() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let get_fn = instance.get_func(&mut store, "hamt_get").unwrap();
    let merge_fn = instance.get_func(&mut store, "hamt_merge").unwrap();

    let node = hamt_empty(&mut store, &empty_fn);
    let a = hamt_set(&mut store, &set_fn, &node, 1, 10);
    let b = hamt_set(&mut store, &set_fn, &node, 2, 20);

    let _merged = hamt_merge(&mut store, &merge_fn, &a, &b);

    // Originals unchanged.
    assert_eq!(hamt_get(&mut store, &get_fn, &a, 1), Some(10));
    assert_eq!(hamt_get(&mut store, &get_fn, &a, 2), None);
    assert_eq!(hamt_get(&mut store, &get_fn, &b, 1), None);
    assert_eq!(hamt_get(&mut store, &get_fn, &b, 2), Some(20));
  }

  #[test]
  fn test_size() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let del_fn = instance.get_func(&mut store, "hamt_delete").unwrap();
    let size_fn = instance.get_func(&mut store, "hamt_size").unwrap();

    let node = hamt_empty(&mut store, &empty_fn);
    assert_eq!(hamt_size(&mut store, &size_fn, &node), 0);

    let node = hamt_set(&mut store, &set_fn, &node, 1, 10);
    assert_eq!(hamt_size(&mut store, &size_fn, &node), 1);

    let node = hamt_set(&mut store, &set_fn, &node, 2, 20);
    assert_eq!(hamt_size(&mut store, &size_fn, &node), 2);

    let node = hamt_set(&mut store, &set_fn, &node, 3, 30);
    assert_eq!(hamt_size(&mut store, &size_fn, &node), 3);

    // Overwrite doesn't change size.
    let node2 = hamt_set(&mut store, &set_fn, &node, 2, 99);
    assert_eq!(hamt_size(&mut store, &size_fn, &node2), 3);

    // Delete reduces size.
    let node3 = hamt_delete(&mut store, &del_fn, &node, 2);
    assert_eq!(hamt_size(&mut store, &size_fn, &node3), 2);

    // Delete absent key doesn't change size.
    let node4 = hamt_delete(&mut store, &del_fn, &node, 99);
    assert_eq!(hamt_size(&mut store, &size_fn, &node4), 3);
  }

  #[test]
  fn test_size_many() {
    let (mut store, instance) = load_hamt();
    let empty_fn = instance.get_func(&mut store, "hamt_empty").unwrap();
    let set_fn = instance.get_func(&mut store, "hamt_set").unwrap();
    let size_fn = instance.get_func(&mut store, "hamt_size").unwrap();

    let mut node = hamt_empty(&mut store, &empty_fn);
    for i in 0..50u32 {
      node = hamt_set(&mut store, &set_fn, &node, i, i * 10);
    }
    assert_eq!(hamt_size(&mut store, &size_fn, &node), 50);
  }
}
