use std::collections::HashMap;

mod expr;

// level 1
pub enum EsonSegment<'a> {
    Null,
    Boolean(bool),
    Int(i64),
    Float(f64),
    String(&'a str),
    FmtString(Vec<FmtString<'a>>),
    List(Vec<EsonSegment<'a>>),
    Dict(HashMap<Key, EsonSegment<'a>>),
    FnCall(&'a str, Vec<EsonSegment<'a>>),
    Var(&'a str),
    Ref(EsonRef<'a>),
    Expr(TokenChunk<'a>),
}

enum FmtString<'a> {
    Literal(&'a str),
    Expr(TokenChunk<'a>),
}

enum EsonRef<'a> {
    Str(&'a str),
    Int(i16),
}

type TokenChunk<'a> = Vec<Token<'a>>;

enum Token<'a> {
    Group(TokenChunk<'a>),
    Primitive(EsonSegment<'a>),
    FnCall(&'a str, TokenChunk<'a>),
    FmtString(EsonFmtString<'a>),
    Var(&'a str),
    Ref(EsonRef<'a>),
    OpAdd,
    OpSub,
}

pub fn add(left: usize, right: usize) -> usize {
    left + right
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
        let result = add(2, 2);
        assert_eq!(result, 4);
    }
}
