use std::rc::Rc;

use tokenizer::{Token, TokenKey};

use crate::expr::ExprUnit;
use crate::util_tree::{Attachable, Indexed, Maintain, Node};

#[derive(Debug, Clone)]
pub(crate) enum Entity<T> {
    Attachable, // Attachable node, means it can have children. e.g. {}, []
    Lone(T),    // Single value
}

#[derive(Debug, Clone)]
pub(crate) struct EsonEntity(pub TokenKey, pub Entity<ExprUnit>);

impl Indexed for EsonEntity {
    fn index(&self) -> TokenKey {
        self.0.clone()
    }
}

impl Attachable for EsonEntity {
    fn attach_able(&self) -> bool {
        match self.1 {
            Entity::Attachable => true,
            _ => false,
        }
    }
}

pub(crate) trait AST {
    fn treeize(token: Token) -> Rc<Node<EsonEntity>>;
    fn update(&self, idx: TokenKey, dat: ExprUnit);
}

fn _idx_and_unit_to_node(i: TokenKey, body: ExprUnit) -> Rc<Node<EsonEntity>> {
    let _attachable_node = |children: Vec<(TokenKey, ExprUnit)>| {
        let mut node = Node::new(EsonEntity(i.clone(), Entity::Attachable));
        for (i, body) in children.into_iter() {
            let child = _idx_and_unit_to_node(i, body);
            child.attached_to(&node);
        }
        node
    };

    match body {
        ExprUnit::UnitFrameDict(d, _) => _attachable_node(d),
        ExprUnit::UnitFrameList(l, _) => _attachable_node(l),
        _ => Node::new(EsonEntity(i, Entity::Lone(body))),
    }
}

impl AST for Rc<Node<EsonEntity>> {
    fn treeize(token: Token) -> Rc<Node<EsonEntity>> {
        let unit = ExprUnit::from(token);
        match unit {
            ExprUnit::UnitFrameRoot(dat, _) => _idx_and_unit_to_node(TokenKey::Dummy, *dat),
            _ => unimplemented!("should not up from a unit that is not ExprUnit::UnitFrameRoot"),
        }
    }

    fn update(&self, idx: TokenKey, dat: ExprUnit) {
        let _update_and_append_children = |children: Vec<(TokenKey, ExprUnit)>| {
            self.update_payload(EsonEntity(idx.clone(), Entity::Attachable));
            for (i, body) in children {
                let child = _idx_and_unit_to_node(i, body);
                child.attached_to(&self);
            }
        };
        match dat {
            ExprUnit::UnitFrameRoot(v, _) => _update_and_append_children(vec![(TokenKey::Dummy, *v)]),
            ExprUnit::UnitFrameList(v, _) => _update_and_append_children(v),
            ExprUnit::UnitFrameDict(v, _) => _update_and_append_children(v),
            _ => self.update_payload(EsonEntity(idx, Entity::Lone(dat))),
        };
    }
}

pub(crate) fn ast(token: Token) -> Rc<Node<EsonEntity>> {
    <Rc<Node<EsonEntity>> as AST>::treeize(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokenizer::Span;

    #[test]
    fn test_() {
        let s = Span::from(
            r#"{
            "k": 1 + 2
        }"#,
        );
        let tokens = tokenizer::parse_base(s).unwrap().1;
        // dbg!(tokens);
        let node = ast(tokens);
        dbg!(node);
    }

    /*
    #[test]
    fn test_simple() {
        // let str = r#"{
        //     @upper
        //     "k": "Hello"
        // }"#;
        // let tokens = tokenizer::parse_base(str).unwrap().1;
        // // dbg!(tokens);
        // let node = treeize(tokens);
        // dbg!(node);

        // let str = r#"@foo {
        //     @upper
        //     "k": "Hello"
        // }"#;
        // let tokens = tokenizer::parse_base(str).unwrap().1;
        // // dbg!(tokens);
        // let node = treeize(tokens);
        // dbg!(node);

        // let str = r#"{
        //     "k": 1 + 2
        // }"#;
        // let tokens = tokenizer::parse_base(str).unwrap().1;
        // // dbg!(tokens);
        // let node = treeize(tokens);
        // dbg!(node);

        let str = r#"{
            @foo
            "k": 1 + 2
        }"#;
        let tokens = tokenizer::parse_base(str).unwrap().1;
        // dbg!(tokens);
        let node = ast(tokens);
        dbg!(node);
    }

    */
}
