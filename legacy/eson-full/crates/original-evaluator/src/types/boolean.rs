use std::fmt::Display;

use tokenizer::Token;

#[derive(Debug)]
pub struct EsonBoolean(pub bool);

impl From<bool> for EsonBoolean {
    fn from(b: bool) -> EsonBoolean {
        EsonBoolean(b)
    }
}

impl Display for EsonBoolean {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for EsonBoolean {
    fn default() -> EsonBoolean {
        EsonBoolean(false)
    }
}

impl From<Token> for EsonBoolean {
    fn from(t: Token) -> EsonBoolean {
        match t {
            Token::TokenPrimBoolean(b, _) => EsonBoolean(b),
            _ => unimplemented!("Unimplemented conversion for token: {:?}", t),
        }
    }
}
