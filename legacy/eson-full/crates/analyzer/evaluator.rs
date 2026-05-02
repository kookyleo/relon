use std::cell::RefCell;
use std::collections::HashMap;
use std::mem;
use std::ops::Deref;
use std::rc::Rc;

use parser::{EsonRef, EsonSegment, FmtString, Key, RefIndex};
use serde::Serialize;

use crate::compute::Compute;
use crate::context::Context;

#[derive(Debug, Serialize, Clone, PartialEq)]
pub enum JsonValue {
    Null,
    Boolean(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(HashMap<String, JsonValue>),
}

#[derive(Debug)]
struct Evaluator<'a> {
    value: Rc<RefCell<EsonSegment>>,
    ctx: &'a mut Context,
}

impl<'a> Evaluator<'a> {
    fn eval(ctx: &'a mut Context, value: &mut EsonSegment) {
        let value = mem::replace(value, EsonSegment::Null);
        let mut evaluator = Self {
            value: Rc::new(RefCell::new(value)),
            ctx,
        };

        let borrowed = evaluator.value.borrow().deref();
        // ctx.set_value_ref(borrowed);

        // evaluator.compute();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test() {
        use crate::context::Context;
        use parser::{EsonRef, EsonSegment, Key, RefIndex};
        use crate::evaluator::Evaluator;

        let mut ctx = Context::new();
        let mut val = EsonSegment::Dict(
            vec![
                (Key::from("a"), EsonSegment::Int(1)),
                (
                    Key::from("b"),
                    EsonRef::Root(vec![RefIndex::Str("a".to_string())]).into(),
                ),
            ]
                .into_iter()
                .collect::<std::collections::HashMap<_, _>>(),
        );
        let mut evaluator = Evaluator::eval(&mut ctx, &mut val);
        dbg!(evaluator);
    }
}