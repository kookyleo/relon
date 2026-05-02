pub use context::Context;
pub use eson::{EsonValue, Render};

use crate::ast::ast;
use crate::context::Var;
use crate::error::EvaluatorError;
use crate::eval::Eval;

use tokenizer::Span;

mod ast;
mod context;
mod decorator;
mod error;
mod eson;
mod eval;
mod expr;
mod ops;
mod types;
mod udf;
mod util_iter;
mod util_nop;
mod util_tree;


pub fn dump_to_json(
    eson_span: Span,
    ext_vars: Vec<(String, EsonValue)>,
    pretty_dump: bool,
) -> Result<String, EvaluatorError> {
    let mut ctx = Context::default();
    ext_vars.iter().for_each(|(name, value)| {
        ctx.set_var(name.clone(), value.clone());
    });
    let tokens = tokenizer::parse_base(eson_span).unwrap().1;
    let mut ast = ast(tokens);
    let r = ast.eval(&mut ctx);
    return match r {
        Err(e) => Err(e),
        Ok(..) => {
            if pretty_dump {
                return Ok(ast.render_to_pretty_json());
            }
            Ok(ast.render_to_json())
        }
    };
}


#[cfg(test)]
mod tests {
    use tokenizer::Span;

    #[test]
    fn test_dump_to_json() {
        let eson = Span::from(r#"{"a": 1, "b": 2, "c": 3}"#);
        let ext_vars = vec![];
        let pretty_dump = true;
        let r = super::dump_to_json(eson, ext_vars, pretty_dump);
        println!("{:}", r.unwrap());
    }
}