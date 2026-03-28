// Thread-local test context — set by the test macro, read by test helpers.
// Used to pass test name/file info for debug file dumps (e.g. DUMP_WAT).

use std::cell::RefCell;

thread_local! {
  static NAME: RefCell<String> = const { RefCell::new(String::new()) };
  static FILE: RefCell<String> = const { RefCell::new(String::new()) };
}

pub fn set(name: &str, file: &str) {
  NAME.with(|n| *n.borrow_mut() = name.to_string());
  FILE.with(|f| *f.borrow_mut() = file.to_string());
}

pub fn name() -> String {
  NAME.with(|n| n.borrow().clone())
}

pub fn file() -> String {
  FILE.with(|f| f.borrow().clone())
}
