// Property graph — typed dense storage indexed by node IDs.
//
// Each property graph is a Vec<T> keyed by a newtype ID (AstId, CpsId, etc.).
// PhantomData<Id> prevents accidental cross-indexing between different ID spaces.
// For sparse properties, use PropGraph<Id, Option<T>>.

use std::marker::PhantomData;

pub struct PropGraph<Id, T> {
    data: Vec<T>,
    _id: PhantomData<Id>,
}

impl<Id, T> PropGraph<Id, T> {
    pub fn new() -> Self {
        PropGraph { data: Vec::new(), _id: PhantomData }
    }

    pub fn with_size(n: usize, default: T) -> Self
    where T: Clone {
        PropGraph { data: vec![default; n], _id: PhantomData }
    }

    pub fn get(&self, id: Id) -> &T
    where Id: Into<usize> + Copy + std::fmt::Debug {
        let idx: usize = id.into();
        &self.data[idx]
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn push(&mut self, val: T) -> Id
    where Id: From<usize> {
        let id = Id::from(self.data.len());
        self.data.push(val);
        id
    }

    pub fn set(&mut self, id: Id, val: T)
    where Id: Into<usize> + Copy, T: Default {
        let idx: usize = id.into();
        if idx >= self.data.len() {
            self.data.resize_with(idx + 1, T::default);
        }
        self.data[idx] = val;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::AstId;

    #[test]
    fn push_and_get() {
        let mut g: PropGraph<AstId, &str> = PropGraph::new();
        g.push("hello");
        g.push("world");
        assert_eq!(g.get(AstId(0)), &"hello");
        assert_eq!(g.get(AstId(1)), &"world");
    }

    #[test]
    #[should_panic]
    fn get_out_of_bounds_panics() {
        let g: PropGraph<AstId, i32> = PropGraph::new();
        g.get(AstId(999));
    }

    #[test]
    fn with_size_and_set() {
        let mut g: PropGraph<AstId, &str> = PropGraph::with_size(3, "empty");
        g.set(AstId(2), "last");
        g.set(AstId(0), "first");
        assert_eq!(g.get(AstId(0)), &"first");
        assert_eq!(g.get(AstId(1)), &"empty");
        assert_eq!(g.get(AstId(2)), &"last");
    }

    #[test]
    fn set_grows_vec() {
        let mut g: PropGraph<AstId, Option<i32>> = PropGraph::new();
        g.set(AstId(3), Some(42));
        assert_eq!(g.get(AstId(0)), &None);
        assert_eq!(g.get(AstId(3)), &Some(42));
    }

    #[test]
    fn sparse_with_option() {
        let mut g: PropGraph<AstId, Option<&str>> = PropGraph::new();
        g.push(Some("hello"));
        g.push(None);
        g.push(Some("world"));
        assert_eq!(g.get(AstId(0)), &Some("hello"));
        assert_eq!(g.get(AstId(1)), &None);
        assert_eq!(g.get(AstId(2)), &Some("world"));
    }
}
