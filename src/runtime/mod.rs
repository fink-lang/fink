// Runtime support modules for fink's WASM output.
//
// These are standalone WAT modules that implement core data structures
// (HAMT for records/dicts, cons lists, etc.) using WASM GC types.
// The compiler's codegen emits calls into these runtime functions.

#[cfg(test)]
mod tests {
  use wasmtime::*;

  /// Prepare a WAT source that uses `@fink/runtime/types` imports for
  /// standalone testing: strip the import line and inject the canonical
  /// type definitions that the linker would normally provide.
  fn prepare_wat(wat: &str, type_defs: &str) -> String {
    let wat = wat.replace(
      "(import \"@fink/runtime/types\" \"*\" (func (param anyref)))",
      "",
    );
    wat.replace(
      "(module\n",
      &format!("(module\n{}\n", type_defs),
    )
  }

  /// Load a WAT module with injected type defs.
  fn load_module(wat_src: &str, type_defs: &str) -> (Store<()>, Instance) {
    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    config.wasm_multi_value(true);

    let engine = Engine::new(&config).unwrap();
    let wat = prepare_wat(wat_src, type_defs);
    let module = Module::new(&engine, &wat).unwrap();
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    (store, instance)
  }

  const HAMT_TYPE_DEFS: &str = concat!(
    "  (rec\n",
    "    (type $Rec (sub (struct)))\n",
    "    (type $Dict (sub (struct)))\n",
    "  )\n",
  );

  const LIST_TYPE_DEFS: &str = "  (type $List (sub (struct)))\n";

  const SET_TYPE_DEFS: &str = "  (type $Set (sub (struct)))\n";

  /// Load the HAMT module and return (store, instance).
  fn load_hamt() -> (Store<()>, Instance) {
    load_module(include_str!("hamt.wat"), HAMT_TYPE_DEFS)
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
    load_module(include_str!("list.wat"), LIST_TYPE_DEFS)
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

  // -- Set tests ------------------------------------------------------

  fn load_set() -> (Store<()>, Instance) {
    load_module(include_str!("set.wat"), SET_TYPE_DEFS)
  }

  fn set_empty(store: &mut Store<()>, fn_: &Func) -> Val {
    let mut result = [Val::AnyRef(None)];
    fn_.call(store, &[], &mut result).unwrap();
    result[0].clone()
  }

  fn set_set(store: &mut Store<()>, fn_: &Func, node: &Val, key: u32) -> Val {
    let k = i31_key(store, key);
    let mut result = [Val::AnyRef(None)];
    fn_.call(store, &[node.clone(), k], &mut result).unwrap();
    result[0].clone()
  }

  fn set_has(store: &mut Store<()>, fn_: &Func, node: &Val, key: u32) -> bool {
    let k = i31_key(store, key);
    let mut result = [Val::I32(0)];
    fn_.call(store, &[node.clone(), k], &mut result).unwrap();
    result[0].unwrap_i32() != 0
  }

  fn set_remove(store: &mut Store<()>, fn_: &Func, node: &Val, key: u32) -> Val {
    let k = i31_key(store, key);
    let mut result = [Val::AnyRef(None)];
    fn_.call(store, &[node.clone(), k], &mut result).unwrap();
    result[0].clone()
  }

  fn set_size(store: &mut Store<()>, fn_: &Func, node: &Val) -> i32 {
    let mut result = [Val::I32(0)];
    fn_.call(store, &[node.clone()], &mut result).unwrap();
    result[0].unwrap_i32()
  }

  fn set_op(store: &mut Store<()>, fn_: &Func, a: &Val, b: &Val) -> Val {
    let mut result = [Val::AnyRef(None)];
    fn_.call(store, &[a.clone(), b.clone()], &mut result).unwrap();
    result[0].clone()
  }

  #[test]
  fn test_set_empty_and_add() {
    let (mut store, instance) = load_set();
    let empty_fn = instance.get_func(&mut store, "set_empty").unwrap();
    let add_fn = instance.get_func(&mut store, "set_set").unwrap();
    let has_fn = instance.get_func(&mut store, "set_has").unwrap();
    let size_fn = instance.get_func(&mut store, "set_size").unwrap();

    let s = set_empty(&mut store, &empty_fn);
    assert_eq!(set_size(&mut store, &size_fn, &s), 0);
    assert!(!set_has(&mut store, &has_fn, &s, 1));

    let s = set_set(&mut store, &add_fn, &s, 1);
    let s = set_set(&mut store, &add_fn, &s, 2);
    let s = set_set(&mut store, &add_fn, &s, 3);

    assert_eq!(set_size(&mut store, &size_fn, &s), 3);
    assert!(set_has(&mut store, &has_fn, &s, 1));
    assert!(set_has(&mut store, &has_fn, &s, 2));
    assert!(set_has(&mut store, &has_fn, &s, 3));
    assert!(!set_has(&mut store, &has_fn, &s, 99));
  }

  #[test]
  fn test_set_set_duplicate() {
    let (mut store, instance) = load_set();
    let empty_fn = instance.get_func(&mut store, "set_empty").unwrap();
    let add_fn = instance.get_func(&mut store, "set_set").unwrap();
    let size_fn = instance.get_func(&mut store, "set_size").unwrap();

    let s = set_empty(&mut store, &empty_fn);
    let s = set_set(&mut store, &add_fn, &s, 1);
    let s = set_set(&mut store, &add_fn, &s, 1);
    let s = set_set(&mut store, &add_fn, &s, 1);

    assert_eq!(set_size(&mut store, &size_fn, &s), 1);
  }

  #[test]
  fn test_set_remove() {
    let (mut store, instance) = load_set();
    let empty_fn = instance.get_func(&mut store, "set_empty").unwrap();
    let add_fn = instance.get_func(&mut store, "set_set").unwrap();
    let has_fn = instance.get_func(&mut store, "set_has").unwrap();
    let remove_fn = instance.get_func(&mut store, "set_remove").unwrap();
    let size_fn = instance.get_func(&mut store, "set_size").unwrap();

    let s = set_empty(&mut store, &empty_fn);
    let s = set_set(&mut store, &add_fn, &s, 1);
    let s = set_set(&mut store, &add_fn, &s, 2);
    let s = set_set(&mut store, &add_fn, &s, 3);

    let s2 = set_remove(&mut store, &remove_fn, &s, 2);
    assert_eq!(set_size(&mut store, &size_fn, &s2), 2);
    assert!(set_has(&mut store, &has_fn, &s2, 1));
    assert!(!set_has(&mut store, &has_fn, &s2, 2));
    assert!(set_has(&mut store, &has_fn, &s2, 3));

    // Original unchanged.
    assert_eq!(set_size(&mut store, &size_fn, &s), 3);
  }

  #[test]
  fn test_set_union() {
    let (mut store, instance) = load_set();
    let empty_fn = instance.get_func(&mut store, "set_empty").unwrap();
    let add_fn = instance.get_func(&mut store, "set_set").unwrap();
    let has_fn = instance.get_func(&mut store, "set_has").unwrap();
    let union_fn = instance.get_func(&mut store, "set_union").unwrap();
    let size_fn = instance.get_func(&mut store, "set_size").unwrap();

    let s = set_empty(&mut store, &empty_fn);
    let a = set_set(&mut store, &add_fn, &s, 1);
    let a = set_set(&mut store, &add_fn, &a, 2);

    let b = set_set(&mut store, &add_fn, &s, 2);
    let b = set_set(&mut store, &add_fn, &b, 3);

    let u = set_op(&mut store, &union_fn, &a, &b);
    assert_eq!(set_size(&mut store, &size_fn, &u), 3);
    assert!(set_has(&mut store, &has_fn, &u, 1));
    assert!(set_has(&mut store, &has_fn, &u, 2));
    assert!(set_has(&mut store, &has_fn, &u, 3));
  }

  #[test]
  fn test_set_intersect() {
    let (mut store, instance) = load_set();
    let empty_fn = instance.get_func(&mut store, "set_empty").unwrap();
    let add_fn = instance.get_func(&mut store, "set_set").unwrap();
    let has_fn = instance.get_func(&mut store, "set_has").unwrap();
    let intersect_fn = instance.get_func(&mut store, "set_intersect").unwrap();
    let size_fn = instance.get_func(&mut store, "set_size").unwrap();

    let s = set_empty(&mut store, &empty_fn);
    let a = set_set(&mut store, &add_fn, &s, 1);
    let a = set_set(&mut store, &add_fn, &a, 2);
    let a = set_set(&mut store, &add_fn, &a, 3);

    let b = set_set(&mut store, &add_fn, &s, 2);
    let b = set_set(&mut store, &add_fn, &b, 3);
    let b = set_set(&mut store, &add_fn, &b, 4);

    let i = set_op(&mut store, &intersect_fn, &a, &b);
    assert_eq!(set_size(&mut store, &size_fn, &i), 2);
    assert!(!set_has(&mut store, &has_fn, &i, 1));
    assert!(set_has(&mut store, &has_fn, &i, 2));
    assert!(set_has(&mut store, &has_fn, &i, 3));
    assert!(!set_has(&mut store, &has_fn, &i, 4));
  }

  #[test]
  fn test_set_difference() {
    let (mut store, instance) = load_set();
    let empty_fn = instance.get_func(&mut store, "set_empty").unwrap();
    let add_fn = instance.get_func(&mut store, "set_set").unwrap();
    let has_fn = instance.get_func(&mut store, "set_has").unwrap();
    let diff_fn = instance.get_func(&mut store, "set_difference").unwrap();
    let size_fn = instance.get_func(&mut store, "set_size").unwrap();

    let s = set_empty(&mut store, &empty_fn);
    let a = set_set(&mut store, &add_fn, &s, 1);
    let a = set_set(&mut store, &add_fn, &a, 2);
    let a = set_set(&mut store, &add_fn, &a, 3);

    let b = set_set(&mut store, &add_fn, &s, 2);
    let b = set_set(&mut store, &add_fn, &b, 4);

    // a - b = {1, 3}
    let d = set_op(&mut store, &diff_fn, &a, &b);
    assert_eq!(set_size(&mut store, &size_fn, &d), 2);
    assert!(set_has(&mut store, &has_fn, &d, 1));
    assert!(!set_has(&mut store, &has_fn, &d, 2));
    assert!(set_has(&mut store, &has_fn, &d, 3));
  }

  #[test]
  fn test_set_many() {
    let (mut store, instance) = load_set();
    let empty_fn = instance.get_func(&mut store, "set_empty").unwrap();
    let add_fn = instance.get_func(&mut store, "set_set").unwrap();
    let has_fn = instance.get_func(&mut store, "set_has").unwrap();
    let size_fn = instance.get_func(&mut store, "set_size").unwrap();

    let mut s = set_empty(&mut store, &empty_fn);
    for i in 0..100u32 {
      s = set_set(&mut store, &add_fn, &s, i);
    }
    assert_eq!(set_size(&mut store, &size_fn, &s), 100);
    for i in 0..100u32 {
      assert!(set_has(&mut store, &has_fn, &s, i), "should have {}", i);
    }
    assert!(!set_has(&mut store, &has_fn, &s, 100));
  }

  fn set_i32(store: &mut Store<()>, fn_: &Func, a: &Val, b: &Val) -> i32 {
    let mut result = [Val::I32(0)];
    fn_.call(store, &[a.clone(), b.clone()], &mut result).unwrap();
    result[0].unwrap_i32()
  }

  #[test]
  fn test_set_sym_diff() {
    let (mut store, instance) = load_set();
    let empty_fn = instance.get_func(&mut store, "set_empty").unwrap();
    let add_fn = instance.get_func(&mut store, "set_set").unwrap();
    let has_fn = instance.get_func(&mut store, "set_has").unwrap();
    let sym_fn = instance.get_func(&mut store, "set_sym_diff").unwrap();
    let size_fn = instance.get_func(&mut store, "set_size").unwrap();

    let s = set_empty(&mut store, &empty_fn);
    let a = set_set(&mut store, &add_fn, &s, 1);
    let a = set_set(&mut store, &add_fn, &a, 2);
    let a = set_set(&mut store, &add_fn, &a, 3);

    let b = set_set(&mut store, &add_fn, &s, 2);
    let b = set_set(&mut store, &add_fn, &b, 3);
    let b = set_set(&mut store, &add_fn, &b, 4);

    // a ^ b = {1, 4}
    let sd = set_op(&mut store, &sym_fn, &a, &b);
    assert_eq!(set_size(&mut store, &size_fn, &sd), 2);
    assert!(set_has(&mut store, &has_fn, &sd, 1));
    assert!(!set_has(&mut store, &has_fn, &sd, 2));
    assert!(!set_has(&mut store, &has_fn, &sd, 3));
    assert!(set_has(&mut store, &has_fn, &sd, 4));
  }

  #[test]
  fn test_set_subset() {
    let (mut store, instance) = load_set();
    let empty_fn = instance.get_func(&mut store, "set_empty").unwrap();
    let add_fn = instance.get_func(&mut store, "set_set").unwrap();
    let subset_fn = instance.get_func(&mut store, "set_subset").unwrap();

    let s = set_empty(&mut store, &empty_fn);
    let a = set_set(&mut store, &add_fn, &s, 1);
    let a = set_set(&mut store, &add_fn, &a, 2);

    let b = set_set(&mut store, &add_fn, &s, 1);
    let b = set_set(&mut store, &add_fn, &b, 2);
    let b = set_set(&mut store, &add_fn, &b, 3);

    // {1, 2} <= {1, 2, 3} → true
    assert_eq!(set_i32(&mut store, &subset_fn, &a, &b), 1);
    // {1, 2, 3} <= {1, 2} → false
    assert_eq!(set_i32(&mut store, &subset_fn, &b, &a), 0);
    // {1, 2} <= {1, 2} → true
    assert_eq!(set_i32(&mut store, &subset_fn, &a, &a), 1);
    // {} <= {1, 2} → true
    assert_eq!(set_i32(&mut store, &subset_fn, &s, &a), 1);
  }

  #[test]
  fn test_set_disjoint() {
    let (mut store, instance) = load_set();
    let empty_fn = instance.get_func(&mut store, "set_empty").unwrap();
    let add_fn = instance.get_func(&mut store, "set_set").unwrap();
    let disjoint_fn = instance.get_func(&mut store, "set_disjoint").unwrap();

    let s = set_empty(&mut store, &empty_fn);
    let a = set_set(&mut store, &add_fn, &s, 1);
    let a = set_set(&mut store, &add_fn, &a, 2);

    let b = set_set(&mut store, &add_fn, &s, 3);
    let b = set_set(&mut store, &add_fn, &b, 4);

    let c = set_set(&mut store, &add_fn, &s, 2);
    let c = set_set(&mut store, &add_fn, &c, 3);

    // {1, 2} >< {3, 4} → true
    assert_eq!(set_i32(&mut store, &disjoint_fn, &a, &b), 1);
    // {1, 2} >< {2, 3} → false
    assert_eq!(set_i32(&mut store, &disjoint_fn, &a, &c), 0);
    // {} >< {1, 2} → true
    assert_eq!(set_i32(&mut store, &disjoint_fn, &s, &a), 1);
  }

  #[test]
  fn test_set_eq() {
    let (mut store, instance) = load_set();
    let empty_fn = instance.get_func(&mut store, "set_empty").unwrap();
    let add_fn = instance.get_func(&mut store, "set_set").unwrap();
    let eq_fn = instance.get_func(&mut store, "set_eq").unwrap();

    let s = set_empty(&mut store, &empty_fn);
    let a = set_set(&mut store, &add_fn, &s, 1);
    let a = set_set(&mut store, &add_fn, &a, 2);

    let b = set_set(&mut store, &add_fn, &s, 2);
    let b = set_set(&mut store, &add_fn, &b, 1);

    let c = set_set(&mut store, &add_fn, &s, 1);
    let c = set_set(&mut store, &add_fn, &c, 3);

    // {1, 2} == {2, 1} → true (order independent)
    assert_eq!(set_i32(&mut store, &eq_fn, &a, &b), 1);
    // {1, 2} == {1, 3} → false
    assert_eq!(set_i32(&mut store, &eq_fn, &a, &c), 0);
    // {} == {} → true
    assert_eq!(set_i32(&mut store, &eq_fn, &s, &s), 1);
    // {1, 2} == {} → false
    assert_eq!(set_i32(&mut store, &eq_fn, &a, &s), 0);
  }

  // ---- String tests -------------------------------------------------------

  const STRING_TYPE_DEFS: &str = concat!(
    "  (rec\n",
    "    (type $Str (sub (struct)))\n",
    "    (type $StrTempl (sub $Str (struct)))\n",
    "    (type $StrVal (sub $Str (struct)))\n",
    "    (type $StrRaw (sub $StrVal (struct)))\n",
    "    (type $StrBytes (sub $StrVal (struct)))\n",
    "  )\n",
  );

  /// Load string.wat with types inlined and test data in linear memory.
  /// `data_bytes` is placed at offset 0 in the data section.
  /// `extra_wat` is injected before the closing ) for test-specific functions.
  fn load_string_with(data_bytes: &[u8], extra_wat: &str) -> (Store<()>, Instance) {
    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    config.wasm_multi_value(true);

    let engine = Engine::new(&config).unwrap();

    let wat = prepare_wat(include_str!("string.wat"), STRING_TYPE_DEFS);
    // Add memory, data section, and extra WAT before the closing )
    let data_hex: String = data_bytes.iter().map(|b| format!("\\{b:02x}")).collect();
    let tail = format!(
      "\n  (memory (export \"memory\") 1)\n  (data (i32.const 0) \"{}\")\n{}\n)",
      data_hex, extra_wat,
    );
    let wat = wat.trim_end().strip_suffix(')').unwrap().to_string() + &tail;

    let module = Module::new(&engine, &wat).unwrap();
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    (store, instance)
  }

  /// Helper: call a no-arg WAT test function that returns i32.
  fn call_test(store: &mut Store<()>, instance: &Instance, name: &str) -> i32 {
    let func = instance.get_func(&mut *store, name).unwrap();
    let mut result = [Val::I32(0)];
    func.call(store, &[], &mut result).unwrap();
    match &result[0] { Val::I32(n) => *n, _ => panic!("expected i32") }
  }

  #[test]
  fn test_str_eq_same_raw() {
    // Two raw strings from same data section region should be equal
    let data = b"hello";
    let (mut store, instance) = load_string_with(data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_raw (i32.const 0) (i32.const 5))
          (call $str_raw (i32.const 0) (i32.const 5))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_eq_different_raw() {
    // "hello" vs "helloX" (different length)
    let data = b"helloX";
    let (mut store, instance) = load_string_with(data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_raw (i32.const 0) (i32.const 5))
          (call $str_raw (i32.const 0) (i32.const 6))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 0);
  }

  #[test]
  fn test_str_escape_passthrough() {
    // "hello" has no escapes — escaped result should equal the raw
    let data = b"hello";
    let (mut store, instance) = load_string_with(data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 5)))
          (call $str_raw (i32.const 0) (i32.const 5))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_escape_newline() {
    // data: "a\nb" (4 bytes) + "\x0a" at offset 4 (the expected result byte)
    // Escaping "a\nb" should produce [0x61, 0x0A, 0x62] = "a<newline>b"
    // We store expected as "a" + actual newline + "b" at offset 4
    let mut data = Vec::new();
    data.extend_from_slice(b"a\\nb");       // offset 0, len 4: raw with \n
    data.extend_from_slice(b"a\nb");        // offset 4, len 3: expected after escape
    let (mut store, instance) = load_string_with(&data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 4)))
          (call $str_render_escape (call $str_raw (i32.const 4) (i32.const 3)))))
    "#);
    // "a\nb" escaped = [0x61, 0x0A, 0x62]
    // "a<newline>b" escaped = [0x61, 0x0A, 0x62] (no backslash, bytes pass through)
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_escape_hex() {
    // "\\x41" should escape to "A" (0x41)
    // Expected: "A" at offset 4
    let mut data = Vec::new();
    data.extend_from_slice(b"\\x41");   // offset 0, len 4
    data.extend_from_slice(b"A");       // offset 4, len 1
    let (mut store, instance) = load_string_with(&data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 4)))
          (call $str_render_escape (call $str_raw (i32.const 4) (i32.const 1)))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_escape_unicode_1byte() {
    // "\\u{0041}" should escape to "A" (U+0041, 1 byte UTF-8)
    let mut data = Vec::new();
    data.extend_from_slice(b"\\u{0041}");  // offset 0, len 8
    data.extend_from_slice(b"A");          // offset 8, len 1
    let (mut store, instance) = load_string_with(&data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 8)))
          (call $str_render_escape (call $str_raw (i32.const 8) (i32.const 1)))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_escape_unicode_2byte() {
    // "\\u{00E9}" should escape to "é" (U+00E9, 2 bytes UTF-8: 0xC3 0xA9)
    let mut data = Vec::new();
    data.extend_from_slice(b"\\u{00E9}");      // offset 0, len 8
    data.extend_from_slice("é".as_bytes());    // offset 8, len 2
    let (mut store, instance) = load_string_with(&data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 8)))
          (call $str_render_escape (call $str_raw (i32.const 8) (i32.const 2)))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_escape_unicode_3byte() {
    // "\\u{2603}" should escape to "☃" (U+2603, 3 bytes UTF-8)
    let mut data = Vec::new();
    data.extend_from_slice(b"\\u{2603}");      // offset 0, len 8
    data.extend_from_slice("☃".as_bytes());   // offset 8, len 3
    let (mut store, instance) = load_string_with(&data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 8)))
          (call $str_render_escape (call $str_raw (i32.const 8) (i32.const 3)))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_escape_unicode_4byte() {
    // "\\u{1F600}" should escape to "😀" (U+1F600, 4 bytes UTF-8)
    let mut data = Vec::new();
    data.extend_from_slice(b"\\u{1F600}");     // offset 0, len 9
    data.extend_from_slice("😀".as_bytes());  // offset 9, len 4
    let (mut store, instance) = load_string_with(&data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 9)))
          (call $str_render_escape (call $str_raw (i32.const 9) (i32.const 4)))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_escape_unicode_underscores() {
    // "\\u{10_FF_FF}" should escape to U+10FFFF (4 bytes UTF-8)
    let mut data = Vec::new();
    data.extend_from_slice(b"\\u{10_FF_FF}");                // offset 0, len 12
    let expected = char::from_u32(0x10FFFF).unwrap();
    let mut buf = [0u8; 4];
    let expected_bytes = expected.encode_utf8(&mut buf);
    let exp_len = expected_bytes.len();
    data.extend_from_slice(expected_bytes.as_bytes());        // offset 12
    let (mut store, instance) = load_string_with(&data, &format!(r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 12)))
          (call $str_render_escape (call $str_raw (i32.const 12) (i32.const {exp_len})))))
    "#));
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_escape_all_simple() {
    // \n\t\r\f\v\b → 0x0A 0x09 0x0D 0x0C 0x0B 0x08
    let mut data = Vec::new();
    data.extend_from_slice(b"\\n\\t\\r\\f\\v\\b");  // offset 0, len 12
    data.extend_from_slice(&[0x0A, 0x09, 0x0D, 0x0C, 0x0B, 0x08]); // offset 12, len 6
    let (mut store, instance) = load_string_with(&data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 12)))
          (call $str_render_escape (call $str_raw (i32.const 12) (i32.const 6)))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_escape_special_chars() {
    // \\ \' \$ → 0x5C 0x27 0x24
    let mut data = Vec::new();
    data.extend_from_slice(b"\\\\\\'\\\x24");  // offset 0, len 6: raw input
    data.extend_from_slice(&[0x5C, 0x27, 0x24]); // offset 6, len 3: expected bytes
    let (mut store, instance) = load_string_with(&data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 6)))
          (call $str_raw (i32.const 6) (i32.const 3))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_escape_empty() {
    let data = b"";
    let (mut store, instance) = load_string_with(data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 0)))
          (call $str_raw (i32.const 0) (i32.const 0))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_escape_trailing_backslash() {
    // "abc\" → "abc\" (trailing backslash preserved literally)
    let mut data = Vec::new();
    data.extend_from_slice(b"abc\\");          // offset 0, len 4
    data.extend_from_slice(b"abc\\");          // offset 4, len 4 (expected: same)
    let (mut store, instance) = load_string_with(&data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 4)))
          (call $str_render_escape (call $str_raw (i32.const 4) (i32.const 4)))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_unescape_roundtrip() {
    // escape "\\n" → [0x0A], unescape → "\\n" = [0x5C, 0x6E]
    // The unescaped result should equal a raw "\\n"
    let data = b"\\n";
    let (mut store, instance) = load_string_with(data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_unescape
            (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 2))))
          (call $str_raw (i32.const 0) (i32.const 2))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_eq_ref_identity() {
    // Same ref should be equal (fast path)
    let data = b"hello";
    let (mut store, instance) = load_string_with(data, r#"
      (func (export "test") (result i32)
        (local $r (ref $StrVal))
        (local.set $r (call $str_raw (i32.const 0) (i32.const 5)))
        (call $str_eq (local.get $r) (local.get $r)))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_eq_data_vs_escaped() {
    // raw "hello" (no escapes) vs escaped "hello" should be equal
    let data = b"hello";
    let (mut store, instance) = load_string_with(data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_raw (i32.const 0) (i32.const 5))
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 5)))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_eq_escaped_vs_escaped() {
    // Two separately escaped copies should be equal
    let data = b"a\\nb";
    let (mut store, instance) = load_string_with(data, r#"
      (func (export "test") (result i32)
        (call $str_eq
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 4)))
          (call $str_render_escape (call $str_raw (i32.const 0) (i32.const 4)))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test"), 1);
  }

  #[test]
  fn test_str_tmpl_count_and_get() {
    // Build a template with 2 segments, verify count and get
    let data = b"helloworld";
    let (mut store, instance) = load_string_with(data, r#"
      (func (export "test_count") (result i32)
        (call $str_tmpl_count
          (call $str_templ
            (array.new_fixed $StrSegments 2
              (call $str_raw (i32.const 0) (i32.const 5))
              (call $str_raw (i32.const 5) (i32.const 5))))))

      (func (export "test_get") (result i32)
        (local $t (ref $StrTempl))
        (local.set $t
          (call $str_templ
            (array.new_fixed $StrSegments 2
              (call $str_raw (i32.const 0) (i32.const 5))
              (call $str_raw (i32.const 5) (i32.const 5)))))
        ;; First segment should equal raw "hello"
        (call $str_eq
          (ref.cast (ref $StrVal) (call $str_tmpl_get (local.get $t) (i32.const 0)))
          (call $str_raw (i32.const 0) (i32.const 5))))
    "#);
    assert_eq!(call_test(&mut store, &instance, "test_count"), 2);
    assert_eq!(call_test(&mut store, &instance, "test_get"), 1);
  }

  // -- Range tests ----------------------------------------------------

  const RANGE_TYPE_DEFS: &str = concat!(
    "  (rec\n",
    "    (type $Num (struct (field $val f64)))\n",
    "    (type $Range (sub (struct)))\n",
    "  )\n",
  );

  /// Helper: construct a $Num from f64 via an exported WAT helper.
  fn make_num(store: &mut Store<()>, instance: &Instance, val: f64) -> Val {
    let func = instance.get_func(&mut *store, "make_num").unwrap();
    let mut result = [Val::AnyRef(None)];
    func.call(store, &[Val::F64(val.to_bits())], &mut result).unwrap();
    result[0].clone()
  }

  fn range_excl(store: &mut Store<()>, instance: &Instance, start: f64, end: f64) -> Val {
    let func = instance.get_func(&mut *store, "range_excl").unwrap();
    let s = make_num(store, instance, start);
    let e = make_num(store, instance, end);
    let mut result = [Val::AnyRef(None)];
    func.call(store, &[s, e], &mut result).unwrap();
    result[0].clone()
  }

  fn range_incl(store: &mut Store<()>, instance: &Instance, start: f64, end: f64) -> Val {
    let func = instance.get_func(&mut *store, "range_incl").unwrap();
    let s = make_num(store, instance, start);
    let e = make_num(store, instance, end);
    let mut result = [Val::AnyRef(None)];
    func.call(store, &[s, e], &mut result).unwrap();
    result[0].clone()
  }

  fn range_in(store: &mut Store<()>, instance: &Instance, val: f64, range: &Val) -> bool {
    let func = instance.get_func(&mut *store, "range_in").unwrap();
    let v = make_num(store, instance, val);
    let mut result = [Val::I32(0)];
    func.call(store, &[v, range.clone()], &mut result).unwrap();
    result[0].unwrap_i32() != 0
  }

  /// Load range.wat with a make_num helper injected for test construction.
  fn load_range_with_helpers() -> (Store<()>, Instance) {
    let mut config = Config::new();
    config.wasm_gc(true);
    config.wasm_function_references(true);
    config.wasm_multi_value(true);

    let engine = Engine::new(&config).unwrap();
    let wat = prepare_wat(include_str!("range.wat"), RANGE_TYPE_DEFS);

    // Inject a make_num helper before the closing )
    let helper = r#"
  (func (export "make_num") (param $v f64) (result (ref $Num))
    (struct.new $Num (local.get $v)))
"#;
    let wat = wat.trim_end().strip_suffix(')').unwrap().to_string() + helper + "\n)";

    let module = Module::new(&engine, &wat).unwrap();
    let mut store = Store::new(&engine, ());
    let instance = Instance::new(&mut store, &module, &[]).unwrap();
    (store, instance)
  }

  #[test]
  fn test_range_excl_in_bounds() {
    let (mut store, instance) = load_range_with_helpers();
    let r = range_excl(&mut store, &instance, 0.0, 10.0);
    // 0..10: 0 in, 5 in, 9 in, 10 out
    assert!(range_in(&mut store, &instance, 0.0, &r));
    assert!(range_in(&mut store, &instance, 5.0, &r));
    assert!(range_in(&mut store, &instance, 9.0, &r));
    assert!(!range_in(&mut store, &instance, 10.0, &r));
  }

  #[test]
  fn test_range_excl_out_of_bounds() {
    let (mut store, instance) = load_range_with_helpers();
    let r = range_excl(&mut store, &instance, 0.0, 10.0);
    assert!(!range_in(&mut store, &instance, -1.0, &r));
    assert!(!range_in(&mut store, &instance, 10.0, &r));
    assert!(!range_in(&mut store, &instance, 11.0, &r));
  }

  #[test]
  fn test_range_incl_in_bounds() {
    let (mut store, instance) = load_range_with_helpers();
    let r = range_incl(&mut store, &instance, 0.0, 10.0);
    // 0...10: 0 in, 5 in, 10 in
    assert!(range_in(&mut store, &instance, 0.0, &r));
    assert!(range_in(&mut store, &instance, 5.0, &r));
    assert!(range_in(&mut store, &instance, 10.0, &r));
  }

  #[test]
  fn test_range_incl_out_of_bounds() {
    let (mut store, instance) = load_range_with_helpers();
    let r = range_incl(&mut store, &instance, 0.0, 10.0);
    assert!(!range_in(&mut store, &instance, -1.0, &r));
    assert!(!range_in(&mut store, &instance, 11.0, &r));
  }

  #[test]
  fn test_range_excl_empty() {
    // 5..5 should contain nothing
    let (mut store, instance) = load_range_with_helpers();
    let r = range_excl(&mut store, &instance, 5.0, 5.0);
    assert!(!range_in(&mut store, &instance, 5.0, &r));
    assert!(!range_in(&mut store, &instance, 4.0, &r));
  }

  #[test]
  fn test_range_incl_single() {
    // 5...5 should contain only 5
    let (mut store, instance) = load_range_with_helpers();
    let r = range_incl(&mut store, &instance, 5.0, 5.0);
    assert!(range_in(&mut store, &instance, 5.0, &r));
    assert!(!range_in(&mut store, &instance, 4.0, &r));
    assert!(!range_in(&mut store, &instance, 6.0, &r));
  }

  #[test]
  fn test_range_negative_bounds() {
    let (mut store, instance) = load_range_with_helpers();
    let r = range_excl(&mut store, &instance, -10.0, -5.0);
    assert!(range_in(&mut store, &instance, -10.0, &r));
    assert!(range_in(&mut store, &instance, -7.0, &r));
    assert!(!range_in(&mut store, &instance, -5.0, &r));
    assert!(!range_in(&mut store, &instance, 0.0, &r));
  }
}
