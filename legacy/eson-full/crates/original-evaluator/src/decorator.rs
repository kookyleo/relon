// @foo("baz") k: "val"
// =>
// k: foo("baz")("val")

use std::collections::HashMap;

use tokenizer::TokenDec;

// @foo("baz") @bar("qux") k: "val"
// =>
// k: foo("baz")(bar("qux")("val"))
use crate::expr::ExprUnit;

trait Decorator {
    fn call(&self, input: ExprUnit) -> ExprUnit;
}

struct UpperCaseDecorator;
struct LowerCaseDecorator;

struct FooDecorator;

impl Decorator for UpperCaseDecorator {
    fn call(&self, input: ExprUnit) -> ExprUnit {
        if let ExprUnit::UnitPrimString(s, tr) = input {
            return ExprUnit::UnitPrimString(s.to_uppercase(), tr);
        }
        unimplemented!()
    }
}

impl Decorator for LowerCaseDecorator {
    fn call(&self, input: ExprUnit) -> ExprUnit {
        if let ExprUnit::UnitPrimString(s, tr) = input {
            return ExprUnit::UnitPrimString(s.to_lowercase(), tr);
        }
        unimplemented!()
    }
}

impl Decorator for FooDecorator {
    fn call(&self, input: ExprUnit) -> ExprUnit {
        input // nop
    }
}

fn decorators_map() -> HashMap<&'static str, Box<dyn Decorator>> {
    let mut decorators: HashMap<&str, Box<dyn Decorator>> = HashMap::new();
    decorators.insert("upper", Box::new(UpperCaseDecorator));
    decorators.insert("lower", Box::new(LowerCaseDecorator));
    decorators.insert("foo", Box::new(FooDecorator));
    decorators
}

pub(crate) fn decorators_call(decorators: Vec<TokenDec>, original: ExprUnit) -> ExprUnit {
    let decorators_map = decorators_map();
    let mut result = original;
    for decorator in decorators.iter().rev() {
        let name = decorator.0.name();
        let decorator = decorators_map.get(name).unwrap();
        result = decorator.call(result);
    }
    result
}

#[cfg(test)]
mod tests {}
