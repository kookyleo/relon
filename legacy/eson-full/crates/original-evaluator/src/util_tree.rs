use std::cell::RefCell;
use std::ops::{Deref, DerefMut};
use std::rc::{Rc, Weak};

use tokenizer::TokenKey;

#[derive(Debug)]
pub(crate) struct Node<T> {
    pub payload: RefCell<Rc<T>>,
    pub parent: RefCell<Weak<Node<T>>>,
    pub children: RefCell<Vec<Rc<Node<T>>>>,
}

pub(crate) trait Indexed {
    fn index(&self) -> TokenKey;
}

pub(crate) trait Attachable {
    fn attach_able(&self) -> bool;
}

pub(crate) trait Navigate {
    fn full_name(&self) -> String;
    fn nav_to_root(&self) -> Self;
    fn nav_to_father(&self) -> Option<Self>
    where
        Self: Sized;
    fn nav_to_child(&self, i: TokenKey) -> Option<Self>
    where
        Self: Sized;
    fn nav_to_uncle(&self, i: TokenKey) -> Option<Self>
    where
        Self: Sized;
    fn nav_from_uncle(&self, ii: Vec<TokenKey>) -> Option<Self>
    where
        Self: Sized;
    fn nav_to_sibling(&self, i: TokenKey) -> Option<Self>
    where
        Self: Sized;
    fn nav_from_sibling(&self, ii: Vec<TokenKey>) -> Option<Self>
    where
        Self: Sized;
    fn nav_to_descendant(&self, i: Vec<TokenKey>) -> Option<Self>
    where
        Self: Sized;
}

pub(crate) trait Maintain<T> {
    fn attached_to(&self, parent: &Self);
    fn payload(&self) -> Rc<T>;
    fn update_payload(&self, payload: T);

    // fn new_root() -> Self;
    // fn replace_with(self, node: TreeNode);
    // // fn set_value(self, value: Rc<RefCell<TreeNode>>);
    // fn append_child(self, child: Rc<RefCell<TreeNode>>);
    // fn append_children(self, children: Vec<Rc<RefCell<TreeNode>>>);
    // fn test_append(self, child: Rc<RefCell<TreeNode>>);
    // fn update_father(self, father: Option<Weak<RefCell<TreeNode>>>);
    // fn index(self) -> Index;
    // fn count(self) -> usize;
    // fn dump(self) -> String;
}

impl<T> Node<T> {
    pub(crate) fn new(payload: T) -> Rc<Self> {
        Rc::new(Node {
            payload: RefCell::new(Rc::new(payload)),
            parent: RefCell::new(Weak::new()),
            children: RefCell::new(vec![]),
        })
    }
}

impl<T> Maintain<T> for Rc<Node<T>>
where
    T: Attachable,
{
    fn attached_to(&self, parent: &Rc<Node<T>>) {
        if parent.payload.borrow().attach_able() {
            self.parent.replace(Rc::downgrade(parent));
            parent.children.borrow_mut().push(Rc::clone(&self));
        } else {
            unreachable!("parent node is not attachable");
        }
    }

    fn payload(&self) -> Rc<T> {
        self.payload.borrow().deref().clone()
    }

    fn update_payload(&self, payload: T) {
        *self.payload.borrow_mut() = Rc::new(payload);
    }
}

impl<T> Navigate for Rc<Node<T>>
where
    T: Indexed,
{
    // eg. root.foo.0.bar
    fn full_name(&self) -> String {
        let mut cur = self.clone();
        let mut ids = vec![];
        loop {
            ids.push(cur.payload.borrow().index().to_string());
            // ids.rotate_right(1); // move the last element to the first
            match cur.nav_to_father() {
                Some(parent) => cur = parent.clone(),
                None => break,
            };
        }
        ids.reverse();
        ids.join(".")
    }

    fn nav_to_root(&self) -> Self {
        let mut cur = self.clone();
        loop {
            match cur.nav_to_father() {
                Some(parent) => cur = parent.clone(),
                None => return cur,
            };
        }
    }

    fn nav_to_father(&self) -> Option<Self> {
        self.parent.borrow().upgrade()
    }

    fn nav_to_child(&self, i: TokenKey) -> Option<Self> {
        let children = self.children.borrow();
        for child in children.iter() {
            if child.payload.borrow().index() == i {
                return Some(child.clone());
            }
        }
        None
    }

    fn nav_to_uncle(&self, i: TokenKey) -> Option<Self> {
        match self.nav_to_father() {
            Some(parent) => parent.nav_to_sibling(i),
            None => None,
        }
    }

    fn nav_from_uncle(&self, ii: Vec<TokenKey>) -> Option<Self> {
        match self.nav_to_father() {
            Some(parent) => parent.nav_from_sibling(ii),
            None => None,
        }
    }

    fn nav_to_sibling(&self, i: TokenKey) -> Option<Self> {
        match self.nav_to_father() {
            Some(parent) => parent.nav_to_child(i),
            None => None,
        }
    }

    fn nav_from_sibling(&self, ii: Vec<TokenKey>) -> Option<Self> {
        match self.nav_to_father() {
            Some(parent) => parent.nav_to_descendant(ii),
            None => None,
        }
    }

    fn nav_to_descendant(&self, ii: Vec<TokenKey>) -> Option<Self> {
        let mut cur = self.clone();
        for i in ii {
            match cur.nav_to_child(i) {
                Some(child) => cur = child.clone(),
                None => return None,
            }
        }
        Some(cur)
    }
}

/*

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    enum Eson {
        Null(TokenKey),
        Str(TokenKey, String),
        Number(TokenKey, String),
        Boolean(TokenKey, bool),
        Dict(TokenKey),
        List(TokenKey),
    }

    impl Indexed for Eson {
        fn index(&self) -> TokenKey {
            match self {
                Eson::Null(i) => i.clone(),
                Eson::Str(i, ..) => i.clone(),
                Eson::Number(i, ..) => i.clone(),
                Eson::Boolean(i, ..) => i.clone(),
                Eson::Dict(i) => i.clone(),
                Eson::List(i) => i.clone(),
            }
        }
    }

    impl Attachable for Eson {
        fn attach_able(&self) -> bool {
            match self {
                Eson::Dict(..) => true,
                Eson::List(..) => true,
                _ => false,
            }
        }
    }

    #[test]
    fn test_maintain() {
        let root = Node::new(Eson::Dict(TokenKey::String("root".to_string())));
        let child = Node::new(Eson::Number(TokenKey::DummySn(0), String::from("child0")));
        child.attached_to(&root);
        assert_eq!(root.children.borrow().len(), 1);
        assert_eq!(
            root.children.borrow()[0]
                .clone()
                .payload
                .borrow()
                .deref()
                .clone(),
            Eson::Number(TokenKey::DummySn(0), String::from("child0")).into()
        );
        child.update_payload(Eson::Number(TokenKey::DummySn(0), String::from("child01")));
        assert_eq!(
            root.children.borrow()[0]
                .clone()
                .payload
                .borrow()
                .deref()
                .clone(),
            Eson::Number(TokenKey::DummySn(0), String::from("child01")).into()
        );
    }

    #[test]
    fn test_navigate() {
        let root = Node::new(Eson::Dict(TokenKey::String("root".to_string())));
        let child1 = Node::new(Eson::Dict(TokenKey::String("child1".to_string())));
        let child2 = Node::new(Eson::Str(
            TokenKey::String("child2".to_string()),
            String::from("child2"),
        ));
        let child1a = Node::new(Eson::List(TokenKey::String("child1a".to_string())));
        let child1b = Node::new(Eson::Str(
            TokenKey::String("child1b".to_string()),
            String::from("child1b"),
        ));
        let child1a1 = Node::new(Eson::Number(TokenKey::DummySn(0), String::from("0")));
        let child1a2 = Node::new(Eson::Number(TokenKey::DummySn(1), String::from("1")));

        child1.attached_to(&root);
        child2.attached_to(&root);
        child1a.attached_to(&child1);
        child1b.attached_to(&child1);
        child1a1.attached_to(&child1a);
        child1a2.attached_to(&child1a);

        assert_eq!(
            root.nav_to_child(TokenKey::String("child1".to_string()))
                .unwrap()
                .payload
                .borrow()
                .index(),
            TokenKey::String("child1".to_string())
        );
        assert_eq!(
            root.nav_to_child(TokenKey::String("child1".to_string()))
                .unwrap()
                .nav_to_child(TokenKey::String("child1a".to_string()))
                .unwrap()
                .payload
                .borrow()
                .index(),
            TokenKey::String("child1a".to_string())
        );
        assert_eq!(
            root.nav_to_child(TokenKey::String("child1".to_string()))
                .unwrap()
                .nav_to_child(TokenKey::String("child1a".to_string()))
                .unwrap()
                .nav_to_child(TokenKey::DummySn(0))
                .unwrap()
                .payload
                .borrow()
                .index(),
            TokenKey::DummySn(0)
        );

        assert_eq!(
            child1a2.nav_to_root().payload.borrow().index(),
            TokenKey::String("root".to_string())
        );
        assert_eq!(
            child1a2.nav_to_father().unwrap().payload.borrow().index(),
            TokenKey::String("child1a".to_string())
        );
        assert_eq!(
            child1a2
                .nav_to_uncle(TokenKey::String("child1b".to_string()))
                .unwrap()
                .payload
                .borrow()
                .index(),
            TokenKey::String("child1b".to_string())
        );
        assert_eq!(
            child1a2
                .nav_to_sibling(TokenKey::DummySn(0))
                .unwrap()
                .payload
                .borrow()
                .index(),
            TokenKey::DummySn(0)
        );
        assert_eq!(
            root.nav_to_descendant(vec![
                TokenKey::String("child1".to_string()),
                TokenKey::String("child1a".to_string()),
                TokenKey::DummySn(0)
            ])
            .unwrap()
            .payload
            .borrow()
            .index(),
            TokenKey::DummySn(0)
        );

        // nav_to_child1a2 first, then nav_to_root
        let nav_to_child1a2 = root
            .nav_to_descendant(vec![
                TokenKey::String("child1".to_string()),
                TokenKey::String("child1a".to_string()),
                TokenKey::DummySn(1),
            ])
            .unwrap();
        assert_eq!(nav_to_child1a2.payload.borrow().index(), TokenKey::DummySn(1));
        assert_eq!(
            nav_to_child1a2.nav_to_root().payload.borrow().index(),
            TokenKey::String("root".to_string())
        );

        // test nav_from_sibling
        let nav_to_child2 = root
            .nav_to_child(TokenKey::String("child2".to_string()))
            .unwrap();
        let nav_from_child2_to_child1a2 = nav_to_child2
            .nav_from_sibling(vec![
                TokenKey::String("child1".to_string()),
                TokenKey::String("child1a".to_string()),
                TokenKey::DummySn(1),
            ])
            .unwrap();
        assert_eq!(
            nav_from_child2_to_child1a2.payload.borrow().index(),
            TokenKey::DummySn(1)
        );

        // test nav_from_uncle
        let nav_to_child1a = root
            .nav_to_descendant(vec![
                TokenKey::String("child1".to_string()),
                TokenKey::String("child1a".to_string()),
            ])
            .unwrap();
        let nav_from_child1a_to_child2 = nav_to_child1a
            .nav_from_uncle(vec![TokenKey::String("child2".to_string())])
            .unwrap();
        assert_eq!(
            nav_from_child1a_to_child2.payload.borrow().index(),
            TokenKey::String("child2".to_string())
        );

        // test full_name
        assert_eq!(child1a2.full_name(), "root.child1.child1a.1");
    }
}

*/