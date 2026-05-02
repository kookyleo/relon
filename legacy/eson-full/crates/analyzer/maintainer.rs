use std::cell::RefCell;
use std::ops::{Deref, DerefMut};
use std::rc::{Rc, Weak};

use parser::RefIndex;

use crate::eval::Eval;
use crate::visitor::{Node, Stage};

pub trait Maintain {
    fn new_empty_root() -> Option<Rc<RefCell<Node>>>;

    fn new(id: RefIndex, father: Option<Weak<RefCell<Node>>>, val: Stage) -> Rc<RefCell<Node>>;
    // fn duplicate(self) -> Rc<RefCell<Node>>;
    fn duplicate_with_idx_and_father(
        self,
        idx: RefIndex,
        father: Option<Weak<RefCell<Node>>>,
    ) -> Rc<RefCell<Node>>;

    fn add_child(self, child: Rc<RefCell<Node>>);
}

impl Maintain for Rc<RefCell<Node>> {
    fn new_empty_root() -> Option<Rc<RefCell<Node>>> {
        Some(Rc::new(RefCell::new(Node::Dict(
            RefIndex::Root,
            None,
            vec![],
        ))))
    }

    fn new(id: RefIndex, father: Option<Weak<RefCell<Node>>>, val: Stage) -> Rc<RefCell<Node>> {
        Rc::new(RefCell::new(Node::InComplete(id, father, val)))
    }

    // fn duplicate(self) -> Rc<RefCell<Node>> {
    //     let node = self.borrow();
    //     match node.deref() {
    //         Node::InComplete(i, f, v) => Rc::new(RefCell::new(Node::InComplete(i.clone(), f.clone(), v.clone()))),
    //         Node::Null(i, f) => Rc::new(RefCell::new(Node::Null(i.clone(), f.clone()))),
    //         Node::Boolean(i, f, v) => Rc::new(RefCell::new(Node::Boolean(i.clone(), f.clone(),  v.clone()))),
    //         Node::Number(i, f, v) => Rc::new(RefCell::new(Node::Number(i.clone(), f.clone(),  v.clone()))),
    //         Node::String(i, f, v) => Rc::new(RefCell::new(Node::String(i.clone(), f.clone(),  v.clone()))),
    //         Node::Dict(i, f, v) => Rc::new(RefCell::new(Node::Dict(i.clone(), f.clone(),  v.clone()))),
    //         Node::List(i, f, v) => Rc::new(RefCell::new(Node::List(i.clone(), f.clone(),  v.clone()))),
    //     }
    // }

    fn duplicate_with_idx_and_father(
        self,
        idx: RefIndex,
        f: Option<Weak<RefCell<Node>>>,
    ) -> Rc<RefCell<Node>> {
        let node = self.borrow();
        match node.deref() {
            Node::InComplete(_, _, v) => Rc::new(RefCell::new(Node::InComplete(idx, f, v.clone()))),
            Node::Null(_, _) => Rc::new(RefCell::new(Node::Null(idx, f))),
            Node::Boolean(_, _, v) => Rc::new(RefCell::new(Node::Boolean(idx, f, v.clone()))),
            Node::Number(_, _, v) => Rc::new(RefCell::new(Node::Number(idx, f, v.clone()))),
            Node::String(_, _, v) => Rc::new(RefCell::new(Node::String(idx, f, v.clone()))),
            Node::Dict(_, _, v) => Rc::new(RefCell::new(Node::Dict(idx, f, v.clone()))),
            Node::List(_, _, v) => Rc::new(RefCell::new(Node::List(idx, f, v.clone()))),
        }
    }

    fn add_child(self, child: Rc<RefCell<Node>>) {
        match self.borrow_mut().deref_mut() {
            Node::Dict(_, _, ref mut children) => children.push(child),
            Node::List(_, _, ref mut children) => children.push(child),
            _ => panic!("Not allowed to add child to this node type"),
        }
    }
}

#[cfg(test)]
mod tests {
    // fn set_value<T: Clone + Default>(vec: &mut Vec<T>, index: usize, value: T) {
    //     if index < vec.len() {
    //         vec[index] = value;
    //     } else {
    //         // 如果索引超出当前长度，首先用默认值填充直到索引位置
    //         while vec.len() < index {
    //             vec.push(T::default());
    //         }
    //         vec.push(value);
    //     }
    // }

    // #[test]
    // fn test_vec() {
    //     let mut v: Vec<_> = vec![1, 2, 3];
    //     v.insert(2, 4);
    //     dbg!(v);
    //
    //     // let mut v = vec!["a", "b"];
    //     // set_value(&mut v, 1, "c");
    //     // println!("{:?}", v); // ["a", "c"]
    //     // set_value(&mut v, 3, "e");
    //     // println!("{:?}", v); // ["a", "c", "", "e"]
    // }

    #[test]
    fn test_node() {}
}
