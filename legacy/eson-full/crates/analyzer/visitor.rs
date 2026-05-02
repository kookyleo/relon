use core::option::Option;
use std::cell::RefCell;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::rc::{Rc, Weak};
use std::task::Wake;

use parser::{
    EsonBoolean, EsonDict, EsonList, EsonNumber, EsonRef, EsonString, RefIndex, TokenChunk,
};

use crate::compute::Compute;
use crate::context::Context;
use crate::eval::Eval;
use crate::expr::{PrattParser, Subject};
use crate::maintainer::Maintain;
use crate::navigator::Navigate;

#[derive(Debug, Clone)]
pub(crate) enum Stage {
    Origin(TokenChunk),
    Semi(Subject),
}

#[derive(Debug)]
pub(crate) enum Node {
    InComplete(RefIndex, Option<Weak<RefCell<Node>>>, Stage),
    Null(RefIndex, Option<Weak<RefCell<Node>>>),
    Boolean(RefIndex, Option<Weak<RefCell<Node>>>, EsonBoolean),
    Number(RefIndex, Option<Weak<RefCell<Node>>>, EsonNumber),
    String(RefIndex, Option<Weak<RefCell<Node>>>, EsonString),
    Dict(
        RefIndex,
        Option<Weak<RefCell<Node>>>,
        Vec<Rc<RefCell<Node>>>,
    ),
    List(
        RefIndex,
        Option<Weak<RefCell<Node>>>,
        Vec<Rc<RefCell<Node>>>,
    ),
}

impl From<(RefIndex, Option<Weak<RefCell<Node>>>, TokenChunk)> for Node {
    fn from((idx, op_father, tc): (RefIndex, Option<Weak<RefCell<Node>>>, TokenChunk)) -> Self {
        Node::InComplete(idx, op_father, Stage::Origin(tc))
    }
}

pub(crate) trait Materialize {
    fn materialize(self, ctx: &mut Context);
    // fn prepare(self) -> (Option<Weak<RefCell<Node>>>, Option<Subject>);

    fn wind_up(self, ctx: &mut Context);
    fn prepare(self, ctx: &mut Context)
               -> Option<(RefIndex, Option<Weak<RefCell<Node>>>, Subject)>;
}

impl Materialize for Rc<RefCell<Node>> {
    fn materialize(self, ctx: &mut Context) {
        // check the current node, if it is incomplete, then parse it and replace it with the parsed node
        let curr = self.clone().prepare(ctx);

        if let Some((idx, father, subject)) = curr {
            let cur_node = self.clone();
            let mut cur_n_borrow_mut = cur_node.borrow_mut();
            match subject {
                Subject::Group(tc) => {
                    let n = Rc::new(RefCell::new(Node::from((idx, father, tc))));
                    n.clone().materialize(ctx);
                    // switch the original node with the fresh node
                    mem::swap(&mut *cur_n_borrow_mut, &mut *n.borrow_mut());
                }
                Subject::PrimNumber(num) => {
                    let n = Rc::new(RefCell::new(Node::Number(idx, father, num)));
                    // n.clone().materialize(ctx);
                    // n
                    let mut fresh = n.borrow_mut();
                    // switch the original node with the fresh node
                    mem::swap(&mut *cur_n_borrow_mut, &mut *fresh);
                }
                Subject::PrimStr(str) => {
                    let n = Rc::new(RefCell::new(Node::String(
                        idx,
                        father,
                        EsonString::from(str),
                    )));
                    // n.clone().materialize(ctx);
                    // n
                    let mut fresh = n.borrow_mut();
                    // switch the original node with the fresh node
                    mem::swap(&mut *cur_n_borrow_mut, &mut *fresh);
                }
                Subject::PrimBoolean(b) => {
                    let n = Rc::new(RefCell::new(Node::Boolean(idx, father, b)));
                    // n.clone().materialize(ctx);
                    // n
                    let mut fresh = n.borrow_mut();
                    // switch the original node with the fresh node
                    mem::swap(&mut *cur_n_borrow_mut, &mut *fresh);
                }
                Subject::PrimNull => {
                    let n = Rc::new(RefCell::new(Node::Null(idx, father)));
                    // n.clone().materialize(ctx);
                    // n
                    let mut fresh = n.borrow_mut();
                    // switch the original node with the fresh node
                    mem::swap(&mut *cur_n_borrow_mut, &mut *fresh);
                }
                // Subject::FnCall(_) => {}
                // Subject::FmtString(_) => {}
                // Subject::Var(_) => {}
                Subject::Ref(r) => {
                    // here made a fake node to represent self and the reference condition,
                    // its father is make sense, but itself is not
                    let virtual_node =
                        Rc::new(RefCell::new(Node::Null(idx.clone(), father.clone())));
                    let ref_target_node = match r.clone() {
                        EsonRef::Sibling(r) => virtual_node.sibling(r),
                        EsonRef::Uncle(r) => virtual_node.uncle(r),
                        EsonRef::Root(r) => virtual_node.root().and_then(|n| n.descendant(r)),
                    };

                    match ref_target_node {
                        // if ref_target_node exists, then duplicate it and replace the original node
                        Some(ref_target_node) => {
                            ref_target_node.clone().materialize(ctx);

                            dbg!("🌟", &ref_target_node, idx.clone(), father.clone());

                            let n = ref_target_node.duplicate_with_idx_and_father(idx, father);
                            mem::swap(&mut *cur_n_borrow_mut, &mut *n.borrow_mut());
                        }
                        // if ref_target_node does not exist, then make a deferred node and append its ref to the deferred queue
                        None => {
                            let deferred_node = Rc::new(RefCell::new(Node::InComplete(
                                idx,
                                father,
                                Stage::Semi(Subject::Ref(r)),
                            )));
                            mem::swap(&mut *cur_n_borrow_mut, &mut *deferred_node.borrow_mut());
                            // ctx.append_to_deferred_q(cur_node.clone());
                        }
                    }
                }
                Subject::Dict(d) => {
                    let EsonDict(mut d) = d;
                    let fresh_node = Rc::new(RefCell::new(Node::Dict(idx, father, vec![])));
                    for (k, v) in d.drain() {
                        let cur_idx = RefIndex::Str(k.into());
                        let child_node = Rc::new(RefCell::new(Node::from((
                            cur_idx,
                            Some(Rc::downgrade(&cur_node)),
                            v,
                        ))));
                        child_node.clone().materialize(ctx);
                        fresh_node.clone().add_child(child_node);
                    }
                    fresh_node.clone().materialize(ctx);
                    // switch the original node with the fresh node
                    mem::swap(&mut *cur_n_borrow_mut, &mut *fresh_node.borrow_mut());
                }
                Subject::List(l) => {
                    let EsonList(mut l) = l;
                    let node = Rc::new(RefCell::new(Node::List(idx, father, vec![])));
                    for (i, v) in l.drain(..).enumerate() {
                        let idx = RefIndex::Int(i.into());
                        let child_node = Rc::new(RefCell::new(Node::from((
                            RefIndex::Int(i),
                            Some(Rc::downgrade(&cur_node)),
                            v,
                        ))));
                        child_node.clone().materialize(ctx);
                        node.clone().add_child(child_node);
                    }
                    let n = node;
                    n.clone().materialize(ctx);
                    // n
                    let mut fresh = n.borrow_mut();
                    // switch the original node with the fresh node
                    mem::swap(&mut *cur_n_borrow_mut, &mut *fresh);
                }
                _ => {
                    panic!("🥦🥦🥦🥦");
                }
            };

            // let mut original = original.borrow_mut();
            // let mut fresh = fresh.borrow_mut();

            // dbg!("🌟", &original, &fresh);

            // switch the original node with the fresh node
            // mem::swap(&mut *original, &mut *fresh);
        }
    }
    fn wind_up(self, ctx: &mut Context) {
        while let Some(tbd) = ctx.pop_from_deferred_q() {
            tbd.materialize(ctx);
        }
    }

    fn prepare(
        self,
        ctx: &mut Context,
    ) -> Option<(RefIndex, Option<Weak<RefCell<Node>>>, Subject)> {
        let curr = {
            let cur_node = self.clone();
            let mut cur_node = cur_node.borrow_mut();
            if let Node::InComplete(idx, father_node, stage) = cur_node.deref_mut() {
                let father = father_node.take();
                match stage {
                    Stage::Origin(tc) => Some((
                        idx.clone(),
                        father.clone(),
                        PrattParser::parse(tc).eval(ctx),
                    )),
                    Stage::Semi(subject) => Some((
                        idx.clone(),
                        father.clone(),
                        mem::replace(subject, Subject::PrimNull),
                    )),
                }
            } else {
                // no other cases need to be handled
                None
            }
        };
        curr
    }
}

#[cfg(test)]
mod tests {
    use parser::TokenChunk;

    use super::*;

    #[test]
    fn test_node() {
        let eson = eson0();
        // dbg!(&eson);
        let ref mut ctx = Context::default();
        let root_node = Rc::new(RefCell::new(Node::from((RefIndex::Root, None, eson))));
        // println!("🚀Root_node Addr: {:p}", root_node);
        let n = root_node.clone();
        n.clone().materialize(ctx);
        n.clone().wind_up(ctx);

        dbg!(n);

        // let mut node = Node {
        //     index: RefIndex::Root,
        //     father: None,
        //     val: RefCell::new(Body::Dict(Node {
        //         index: RefIndex::Root,
        //         father: None,
        //         val: RefCell::new(None),
        //     })),
        // };
    }

    fn eson2() -> TokenChunk {
        let dat = r#"{
            a: 1,
            r: sibling.a,
            r2: sibling.r,
        }"#;

        let (_, tc) = parser::parse_root(dat).unwrap();
        tc
    }

    fn eson1() -> TokenChunk {
        let dat = r#"{
            a: 1,
            b: ["x", "y", "z"],
            c: { d: 2, e: 3 },
            r: sibling.a
        }"#;
        let (_, tc) = parser::parse_root(dat).unwrap();
        tc
    }

    fn eson0() -> TokenChunk {
        let dat = r#"{
            a: 1,
            r: sibling.a
        }"#;
        let (_, tc) = parser::parse_root(dat).unwrap();
        // &tc = TokenChunk([
        //     Dict(EsonDict({
        //         EsonKey {
        //             name: "/", decorator: None,
        //         }: TokenChunk([
        //             Dict(EsonDict({
        //                 EsonKey {
        //                     name: "a", decorator: None,
        //                 }: TokenChunk([
        //                     PrimNumber(EsonNumber(1.0))
        //                 ]),
        //                 EsonKey {
        //                     name: "r", decorator: None,
        //                 }: TokenChunk([
        //                     Ref(Sibling([Str("a")]))
        //                 ]),
        //             })),
        //         ]),
        //     })),
        // ])
        tc
    }
}
