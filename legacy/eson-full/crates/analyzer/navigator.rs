use std::cell::RefCell;
use std::ops::Deref;
use std::rc::Rc;

use parser::RefIndex;

use crate::maintainer::Maintain;
use crate::visitor::Node;

pub trait Navigate {
    fn root(self) -> Option<Rc<RefCell<Node>>>;

    fn uncle(self, r: Vec<RefIndex>) -> Option<Rc<RefCell<Node>>>;

    fn father(self) -> Option<Rc<RefCell<Node>>>;

    fn sibling(self, r: Vec<RefIndex>) -> Option<Rc<RefCell<Node>>>;

    fn child(self, r: RefIndex) -> Option<Rc<RefCell<Node>>>;

    fn descendant(self, r: Vec<RefIndex>) -> Option<Rc<RefCell<Node>>>;
}

impl Navigate for Rc<RefCell<Node>> {
    fn root(self) -> Option<Self> {
        let mut cur = self;
        while let Some(f) = cur.clone().father() {
            cur = f;
        }
        Some(cur)
    }
    fn uncle(self, r: Vec<RefIndex>) -> Option<Self> {
        if let Some(f) = self.father() {
            f.sibling(r)
        } else {
            None
        }
    }

    fn father(self) -> Option<Self> {
        match self.borrow().deref() {
            Node::InComplete(_, f, _) => f,
            Node::Null(_, f) => f,
            Node::Boolean(_, f, _) => f,
            Node::Number(_, f, _) => f,
            Node::String(_, f, _) => f,
            Node::Dict(_, f, _) => f,
            Node::List(_, f, _) => f,
        }.clone()
            .map(|f| {
                f.upgrade().expect("impossible")
            })
    }
    fn sibling(self, r: Vec<RefIndex>) -> Option<Self> {
        let n = self;
        if let Some(f) = n.father() {
            f.descendant(r)
        } else {
            None
        }
    }

    fn child(self, r: RefIndex) -> Option<Self> {
        match (self.borrow().deref(), r) {
            (Node::Dict(_, _, d), RefIndex::Str(r)) => d
                .iter()
                .find(|n| {
                    match n.borrow().deref() {
                        Node::InComplete(RefIndex::Str(rk), _, _) => rk == &r,
                        Node::Null(RefIndex::Str(rk), _) => rk == &r,
                        Node::Boolean(RefIndex::Str(rk), _, _) => rk == &r,
                        Node::Number(RefIndex::Str(rk), _, _) => rk == &r,
                        Node::String(RefIndex::Str(rk), _, _) => rk == &r,
                        Node::Dict(RefIndex::Str(rk), _, _) => rk == &r,
                        Node::List(RefIndex::Str(rk), _, _) => rk == &r,
                        _ => unimplemented!()
                    }
                })
                .cloned(),
            (Node::List(_, _, l), RefIndex::Int(r)) => l.get(r).cloned(),
            _ => None,
        }
    }
    fn descendant(self, r: Vec<RefIndex>) -> Option<Self> {
        let mut cur = self.clone();
        for i in r {
            if let Some(c) = cur.child(i) {
                cur = c;
            } else {
                return None;
            }
        }
        Some(cur)
    }
}

#[cfg(test)]
mod tests {
    use std::ops::DerefMut;

    use parser::EsonRef;

    use super::*;

    #[test]
    fn test() {
        // three layers dict
        let root = Rc::new(RefCell::new(Node::Dict(RefIndex::Root, None, vec![])));

        let layer1 = Rc::new(RefCell::new(Node::Dict(
            RefIndex::Str("a".into()),
            Some(Rc::downgrade(&root)),
            vec![],
        )));

        let layer11 = Rc::new(RefCell::new(Node::String(
            RefIndex::Str("a1".into()),
            Some(Rc::downgrade(&layer1)),
            "a1".into(),
        )));

        let layer12 = Rc::new(RefCell::new(Node::String(
            RefIndex::Str("a2".into()),
            Some(Rc::downgrade(&layer1)),
            "a2".into(),
        )));

        let layer2 = Rc::new(RefCell::new(Node::String(
            RefIndex::Str("b".into()),
            Some(Rc::downgrade(&root)),
            "b".into(),
        )));

        match layer1.borrow_mut().deref_mut() {
            Node::Dict(_, _, ref mut l1) => {
                l1.push(layer11.clone());
                l1.push(layer12.clone());
            }
            _ => {}
        };

        if let Node::Dict(_, _, ref mut r) = root.borrow_mut().deref_mut() {
            r.push(layer1.clone());
            r.push(layer2.clone());
        };

        // navigate test
        assert!(root.clone().father().is_none());
        {
            assert!(root.clone().root().is_some());
            let root = root.clone().root().unwrap();
            let root = root.borrow();
            let Node::Dict(i, f, _) = root.deref() else {
                panic!("impossible");
            };
            assert_eq!(*i, RefIndex::Root);
            assert_eq!(f.is_none(), true);
        }
        assert!(root.clone().uncle(vec![]).is_none());
        assert!(root.clone().sibling(vec![]).is_none());
        {
            assert!(root.clone().child(RefIndex::Str("a".into())).is_some());
            let root = root.clone().child(RefIndex::Str("a".into())).unwrap();
            let root = root.borrow();
            let Node::Dict(i, f, _) = root.deref() else {
                panic!("impossible")
            };
            assert_eq!(*i, RefIndex::Str("a".into()));
            assert_eq!(f.is_some(), true);
        }
        assert!(root.clone().child(RefIndex::Int(0)).is_none());
        assert!(root.clone().descendant(vec![]).is_some()); // root self
        {
            assert!(root
                .clone()
                .descendant(vec![RefIndex::Str("a".into())])
                .is_some());
            let root = root
                .clone()
                .descendant(vec![RefIndex::Str("a".into())])
                .unwrap();
            let root = root.borrow();
            let Node::Dict(i, f, _) = root.deref() else {
                panic!("impossible")
            };
            assert_eq!(*i, RefIndex::Str("a".into()));
            assert_eq!(f.is_some(), true);
        }
        let layer11_cp = layer11.clone();
        {
            assert!(layer11_cp
                .clone()
                .sibling(vec![RefIndex::Str("a2".into())])
                .is_some());
            let layer11_cp = layer11_cp
                .clone()
                .sibling(vec![RefIndex::Str("a2".into())])
                .unwrap();
            let layer11_cp = layer11_cp.borrow();
            let Node::String(i, f, _) = layer11_cp.deref() else {
                panic!("impossible")
            };
            assert_eq!(*i, RefIndex::Str("a2".into()));
            assert_eq!(f.is_some(), true);
        }
        {
            assert!(layer11_cp
                .clone()
                .uncle(vec![RefIndex::Str(String::from("b"))])
                .is_some());
            let layer11_cp = layer11_cp
                .clone()
                .uncle(vec![RefIndex::Str(String::from("b"))])
                .unwrap();
            let layer11_cp = layer11_cp.borrow();
            let Node::String(i, f, _) = layer11_cp.deref() else {
                panic!("impossible")
            };
            assert_eq!(*i, RefIndex::Str("b".into()));
            assert_eq!(f.is_none(), false);
        }
    }

    #[test]
    fn test_borrowed() {
        let r = EsonRef::Sibling(vec![RefIndex::Str("a".into())]);
        let virtual_node =
            Rc::new(RefCell::new(Node::Null(RefIndex::Root, None)));
        let ref_target_node = match r.clone() {
            EsonRef::Sibling(r) => virtual_node.sibling(r),
            EsonRef::Uncle(r) => virtual_node.uncle(r),
            EsonRef::Root(r) => virtual_node.root().and_then(|n| n.descendant(r)),
        };

        dbg!(ref_target_node);
    }
}
