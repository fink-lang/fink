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

  // -- List tests -----------------------------------------------------

  fn load_list() -> (Store<()>, Instance) {
    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    config.wasm_multi_value(true);

    let engine = Engine::new(&config).unwrap();
    let wat = include_bytes!("list.wat");
    let module = Module::new(&engine, &wat[..]).unwrap();
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    (store, instance)
  }

  fn list_empty(store: &mut Store<()>, nil_fn: &Func) -> Val {
    let mut result = [Val::AnyRef(None)];
    nil_fn.call(store, &[], &mut result).unwrap();
    result[0].clone()
  }

  fn list_append(store: &mut Store<()>, cons_fn: &Func, head: u32, tail: &Val) -> Val {
    let h = i31_key(store, head);
    let mut result = [Val::AnyRef(None)];
    cons_fn.call(store, &[h, tail.clone()], &mut result).unwrap();
    result[0].clone()
  }

  fn list_head(store: &mut Store<()>, head_fn: &Func, list: &Val) -> Option<u32> {
    let mut result = [Val::AnyRef(None)];
    head_fn.call(&mut *store, &[list.clone()], &mut result).unwrap();
    get_i31(store, &result[0])
  }

  fn list_tail(store: &mut Store<()>, tail_fn: &Func, list: &Val) -> Val {
    let mut result = [Val::AnyRef(None)];
    tail_fn.call(store, &[list.clone()], &mut result).unwrap();
    result[0].clone()
  }

  fn list_pop(store: &mut Store<()>, pop_fn: &Func, list: &Val) -> (Option<u32>, Val) {
    let mut result = [Val::AnyRef(None), Val::AnyRef(None)];
    pop_fn.call(&mut *store, &[list.clone()], &mut result).unwrap();
    (get_i31(store, &result[0]), result[1].clone())
  }

  fn list_size(store: &mut Store<()>, len_fn: &Func, list: &Val) -> i32 {
    let mut result = [Val::I32(0)];
    len_fn.call(store, &[list.clone()], &mut result).unwrap();
    match &result[0] { Val::I32(n) => *n, _ => panic!("expected i32") }
  }

  fn list_concat(store: &mut Store<()>, fn_: &Func, a: &Val, b: &Val) -> Val {
    let mut result = [Val::AnyRef(None)];
    fn_.call(store, &[a.clone(), b.clone()], &mut result).unwrap();
    result[0].clone()
  }

  /// Build a list from a slice: [1, 2, 3] → cons(1, cons(2, cons(3, nil)))
  fn build_list(store: &mut Store<()>, nil_fn: &Func, cons_fn: &Func, items: &[u32]) -> Val {
    let mut list = list_empty(store, nil_fn);
    for &item in items.iter().rev() {
      list = list_append(store, cons_fn, item, &list);
    }
    list
  }

  /// Collect a list into a vec for easy assertion.
  fn collect_list(store: &mut Store<()>, head_fn: &Func, tail_fn: &Func, list: &Val) -> Vec<u32> {
    let mut result = vec![];
    let mut current = list.clone();
    loop {
      match &current {
        Val::AnyRef(None) => break,
        Val::AnyRef(Some(_)) => {
          result.push(list_head(store, head_fn, &current).unwrap());
          current = list_tail(store, tail_fn, &current);
        }
        _ => break,
      }
    }
    result
  }

  #[test]
  fn test_list_empty() {
    let (mut store, instance) = load_list();
    let empty_fn = instance.get_func(&mut store, "list_empty").unwrap();
    let size_fn = instance.get_func(&mut store, "list_size").unwrap();

    let nil = list_empty(&mut store, &empty_fn);
    assert_eq!(list_size(&mut store, &size_fn, &nil), 0);
  }

  #[test]
  fn test_list_append_and_head_tail() {
    let (mut store, instance) = load_list();
    let nil_fn = instance.get_func(&mut store, "list_empty").unwrap();
    let cons_fn = instance.get_func(&mut store, "list_append").unwrap();
    let head_fn = instance.get_func(&mut store, "list_head").unwrap();
    let tail_fn = instance.get_func(&mut store, "list_tail").unwrap();
    let size_fn = instance.get_func(&mut store, "list_size").unwrap();

    // [1, 2, 3]
    let list = build_list(&mut store, &nil_fn, &cons_fn, &[1, 2, 3]);

    assert_eq!(list_size(&mut store, &size_fn, &list), 3);
    assert_eq!(list_head(&mut store, &head_fn, &list), Some(1));

    let rest = list_tail(&mut store, &tail_fn, &list);
    assert_eq!(list_head(&mut store, &head_fn, &rest), Some(2));

    let rest2 = list_tail(&mut store, &tail_fn, &rest);
    assert_eq!(list_head(&mut store, &head_fn, &rest2), Some(3));

    let rest3 = list_tail(&mut store, &tail_fn, &rest2);
    assert_eq!(list_size(&mut store, &size_fn, &rest3), 0);
  }

  #[test]
  fn test_list_pop() {
    let (mut store, instance) = load_list();
    let nil_fn = instance.get_func(&mut store, "list_empty").unwrap();
    let cons_fn = instance.get_func(&mut store, "list_append").unwrap();
    let pop_fn = instance.get_func(&mut store, "list_pop").unwrap();
    let size_fn = instance.get_func(&mut store, "list_size").unwrap();

    let list = build_list(&mut store, &nil_fn, &cons_fn, &[10, 20, 30]);

    let (h, rest) = list_pop(&mut store, &pop_fn, &list);
    assert_eq!(h, Some(10));

    let (h2, rest2) = list_pop(&mut store, &pop_fn, &rest);
    assert_eq!(h2, Some(20));

    let (h3, rest3) = list_pop(&mut store, &pop_fn, &rest2);
    assert_eq!(h3, Some(30));

    assert_eq!(list_size(&mut store, &size_fn, &rest3), 0);
  }

  #[test]
  fn test_list_size() {
    let (mut store, instance) = load_list();
    let nil_fn = instance.get_func(&mut store, "list_empty").unwrap();
    let cons_fn = instance.get_func(&mut store, "list_append").unwrap();
    let len_fn = instance.get_func(&mut store, "list_size").unwrap();

    let nil = list_empty(&mut store, &nil_fn);
    assert_eq!(list_size(&mut store, &len_fn, &nil), 0);

    let list = build_list(&mut store, &nil_fn, &cons_fn, &[1, 2, 3, 4, 5]);
    assert_eq!(list_size(&mut store, &len_fn, &list), 5);
  }

  #[test]
  fn test_list_concat() {
    let (mut store, instance) = load_list();
    let nil_fn = instance.get_func(&mut store, "list_empty").unwrap();
    let cons_fn = instance.get_func(&mut store, "list_append").unwrap();
    let head_fn = instance.get_func(&mut store, "list_head").unwrap();
    let tail_fn = instance.get_func(&mut store, "list_tail").unwrap();
    let concat_fn = instance.get_func(&mut store, "list_concat").unwrap();

    let a = build_list(&mut store, &nil_fn, &cons_fn, &[1, 2]);
    let b = build_list(&mut store, &nil_fn, &cons_fn, &[3, 4, 5]);

    let merged = list_concat(&mut store, &concat_fn, &a, &b);
    assert_eq!(collect_list(&mut store, &head_fn, &tail_fn, &merged), vec![1, 2, 3, 4, 5]);
  }

  #[test]
  fn test_list_concat_empty() {
    let (mut store, instance) = load_list();
    let nil_fn = instance.get_func(&mut store, "list_empty").unwrap();
    let cons_fn = instance.get_func(&mut store, "list_append").unwrap();
    let head_fn = instance.get_func(&mut store, "list_head").unwrap();
    let tail_fn = instance.get_func(&mut store, "list_tail").unwrap();
    let concat_fn = instance.get_func(&mut store, "list_concat").unwrap();

    let nil = list_empty(&mut store, &nil_fn);
    let a = build_list(&mut store, &nil_fn, &cons_fn, &[1, 2]);

    // empty ++ a = a
    let r1 = list_concat(&mut store, &concat_fn, &nil, &a);
    assert_eq!(collect_list(&mut store, &head_fn, &tail_fn, &r1), vec![1, 2]);

    // a ++ empty = a
    let r2 = list_concat(&mut store, &concat_fn, &a, &nil);
    assert_eq!(collect_list(&mut store, &head_fn, &tail_fn, &r2), vec![1, 2]);
  }

  #[test]
  fn test_list_structural_sharing() {
    let (mut store, instance) = load_list();
    let nil_fn = instance.get_func(&mut store, "list_empty").unwrap();
    let cons_fn = instance.get_func(&mut store, "list_append").unwrap();
    let head_fn = instance.get_func(&mut store, "list_head").unwrap();
    let tail_fn = instance.get_func(&mut store, "list_tail").unwrap();

    // shared = [2, 3]
    let shared = build_list(&mut store, &nil_fn, &cons_fn, &[2, 3]);

    // v1 = [1, 2, 3] (prepend 1 to shared)
    let v1 = list_append(&mut store, &cons_fn, 1, &shared);

    // v2 = [9, 2, 3] (prepend 9 to shared)
    let v2 = list_append(&mut store, &cons_fn, 9, &shared);

    assert_eq!(collect_list(&mut store, &head_fn, &tail_fn, &v1), vec![1, 2, 3]);
    assert_eq!(collect_list(&mut store, &head_fn, &tail_fn, &v2), vec![9, 2, 3]);
  }

  #[test]
  fn test_list_get() {
    let (mut store, instance) = load_list();
    let nil_fn = instance.get_func(&mut store, "list_empty").unwrap();
    let cons_fn = instance.get_func(&mut store, "list_append").unwrap();
    let get_fn = instance.get_func(&mut store, "list_get").unwrap();

    let list = build_list(&mut store, &nil_fn, &cons_fn, &[10, 20, 30]);

    let mut result = [Val::AnyRef(None)];

    // Valid indices.
    get_fn.call(&mut store, &[list.clone(), Val::I32(0)], &mut result).unwrap();
    assert_eq!(get_i31(&store, &result[0]), Some(10));

    get_fn.call(&mut store, &[list.clone(), Val::I32(1)], &mut result).unwrap();
    assert_eq!(get_i31(&store, &result[0]), Some(20));

    get_fn.call(&mut store, &[list.clone(), Val::I32(2)], &mut result).unwrap();
    assert_eq!(get_i31(&store, &result[0]), Some(30));

    // Out of bounds.
    get_fn.call(&mut store, &[list.clone(), Val::I32(3)], &mut result).unwrap();
    assert!(matches!(&result[0], Val::AnyRef(None)));

    // Negative index.
    get_fn.call(&mut store, &[list.clone(), Val::I32(-1)], &mut result).unwrap();
    assert!(matches!(&result[0], Val::AnyRef(None)));

    // Empty list.
    let nil = list_empty(&mut store, &nil_fn);
    get_fn.call(&mut store, &[nil, Val::I32(0)], &mut result).unwrap();
    assert!(matches!(&result[0], Val::AnyRef(None)));
  }

  #[test]
  fn test_list_find() {
    let (mut store, instance) = load_list();
    let nil_fn = instance.get_func(&mut store, "list_empty").unwrap();
    let cons_fn = instance.get_func(&mut store, "list_append").unwrap();
    let find_fn = instance.get_func(&mut store, "list_find").unwrap();

    let list = build_list(&mut store, &nil_fn, &cons_fn, &[10, 20, 30, 40]);

    // Find existing elements.
    let v10 = i31_key(&mut store, 10);
    let v30 = i31_key(&mut store, 30);
    let v40 = i31_key(&mut store, 40);
    let v99 = i31_key(&mut store, 99);

    let mut result = [Val::I32(0)];
    find_fn.call(&mut store, &[list.clone(), v10], &mut result).unwrap();
    assert_eq!(result[0].unwrap_i32(), 0);

    find_fn.call(&mut store, &[list.clone(), v30], &mut result).unwrap();
    assert_eq!(result[0].unwrap_i32(), 2);

    find_fn.call(&mut store, &[list.clone(), v40], &mut result).unwrap();
    assert_eq!(result[0].unwrap_i32(), 3);

    // Not found.
    find_fn.call(&mut store, &[list.clone(), v99], &mut result).unwrap();
    assert_eq!(result[0].unwrap_i32(), -1);

    // Empty list.
    let nil = list_empty(&mut store, &nil_fn);
    let v1 = i31_key(&mut store, 1);
    find_fn.call(&mut store, &[nil, v1], &mut result).unwrap();
    assert_eq!(result[0].unwrap_i32(), -1);
  }
}
