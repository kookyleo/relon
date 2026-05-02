use std::fmt::Display;

use tokenizer::Token;

#[derive(Debug, PartialEq)]
enum Fragment<'a> {
    Literal(&'a str),
    EscapedChar(char),
    EscapedWS,
}

#[derive(Debug, PartialEq, Clone)]
pub struct EsonString(pub String);

impl Display for EsonString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Default for EsonString {
    fn default() -> Self {
        EsonString(String::default())
    }
}

impl EsonString {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<&str> for EsonString {
    fn from(s: &str) -> EsonString {
        EsonString(String::from(s))
    }
}

impl From<String> for EsonString {
    fn from(s: String) -> EsonString {
        EsonString(s)
    }
}

impl From<EsonString> for String {
    fn from(s: EsonString) -> String {
        s.0
    }
}

impl From<Token> for EsonString {
    fn from(t: Token) -> EsonString {
        match t {
            Token::TokenPrimString(s, _) => EsonString(s),
            _ => unimplemented!("Unimplemented conversion for token: {:?}", t),
        }
    }
}
