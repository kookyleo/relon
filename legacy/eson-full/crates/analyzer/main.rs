mod compute;
mod context;
mod eval;
mod expr;
mod literal;
mod ops;
pub mod util_iter;
mod tree;

fn main() {
    let dat = r#"{ data:  "direct data" }"#;
    let (_, eson) = parser::parse_root(dat).unwrap();
    println!("{:?}", eson);
}

#[cfg(test)]
mod tests {
    use parser::{EsonDict, EsonKey, EsonNumber, Token, TokenChunk};

    use crate::compute::Compute;
    use crate::context::Context;
    use crate::context::test_ctx;

    #[test]
    fn test_ref() {
        let mut ctx = test_ctx();
        let dat = r#"{ data: { a: 1, b: super.a } }"#;
        let (_, mut eson) = parser::parse_root(dat).unwrap();

        eson.compute(&mut ctx);

        assert_eq!(
            eson,
            TokenChunk::single(Token::from(EsonDict::from(vec![(
                EsonKey::from("/"),
                TokenChunk::single(Token::from(EsonDict::from(vec![(
                    EsonKey::from("data"),
                    TokenChunk::single(Token::Dict(EsonDict::from(vec![
                        (
                            EsonKey::from("a"),
                            TokenChunk::single(EsonNumber::from(1).into())
                        ),
                        (
                            EsonKey::from("b"),
                            TokenChunk::single(EsonNumber::from(1).into())
                        ),
                    ])))
                )])))
            )])))
        );
    }

    #[test]
    fn test_var() {
        let mut ctx = test_ctx();
        let dat = r#"{ data: va }"#;
        let (_, mut eson) = parser::parse_root(dat).unwrap();
        eson.compute(&mut ctx);

        assert_eq!(
            eson,
            TokenChunk::single(Token::from(EsonDict::from(vec![(
                EsonKey::from("/"),
                TokenChunk::single(Token::from(EsonDict::from(vec![(
                    EsonKey::from("data"),
                    TokenChunk::single(EsonNumber::from(1).into())
                )])))
            )])))
        );
    }

    #[test]
    fn test_sub_compute() {
        let mut ctx = Context::default();
        let dat = r#"{ data: 2 * 3 }"#;
        let (_, mut eson) = parser::parse_root(dat).unwrap();
        eson.compute(&mut ctx);
        assert_eq!(
            eson,
            TokenChunk::single(Token::from(EsonDict::from(vec![(
                EsonKey::from("/"),
                TokenChunk::single(Token::from(EsonDict::from(vec![(
                    EsonKey::from("data"),
                    TokenChunk::single(EsonNumber::from(6).into())
                )])))
            )])))
        );

        let dat = r#"{ data: {
            i: 2 * 3
        } }"#;
        let (_, mut eson) = parser::parse_root(dat).unwrap();
        eson.compute(&mut ctx);
        assert_eq!(
            eson,
            TokenChunk::single(Token::from(EsonDict::from(vec![(
                EsonKey::from("/"),
                TokenChunk::single(Token::from(EsonDict::from(vec![(
                    EsonKey::from("data"),
                    TokenChunk::single(Token::from(EsonDict::from(vec![(
                        EsonKey::from("i"),
                        TokenChunk::single(EsonNumber::from(6).into())
                    )])))
                )])))
            )])))
        );
    }

    #[test]
    fn test_op_add_sub_mul_div() {
        let mut ctx = Context::default();

        let dat = r#"{ data: 1 + 2 }"#;
        let (_, mut eson) = parser::parse_root(dat).unwrap();
        eson.compute(&mut ctx);
        assert_eq!(
            eson,
            TokenChunk::single(Token::from(EsonDict::from(vec![(
                EsonKey::from("/"),
                TokenChunk::single(Token::from(EsonDict::from(vec![(
                    EsonKey::from("data"),
                    TokenChunk::single(EsonNumber::from(3).into())
                )])))
            )])))
        );

        let dat = r#"{ data: 1 - 2 }"#;
        let (_, mut eson) = parser::parse_root(dat).unwrap();
        eson.compute(&mut ctx);
        assert_eq!(
            eson,
            TokenChunk::single(Token::from(EsonDict::from(vec![(
                EsonKey::from("/"),
                TokenChunk::single(Token::from(EsonDict::from(vec![(
                    EsonKey::from("data"),
                    TokenChunk::single(EsonNumber::from(-1).into())
                )])))
            )])))
        );

        let dat = r#"{ data: 1 * 2 }"#;
        let (_, mut eson) = parser::parse_root(dat).unwrap();
        eson.compute(&mut ctx);
        assert_eq!(
            eson,
            TokenChunk::single(Token::from(EsonDict::from(vec![(
                EsonKey::from("/"),
                TokenChunk::single(Token::from(EsonDict::from(vec![(
                    EsonKey::from("data"),
                    TokenChunk::single(EsonNumber::from(2).into())
                )])))
            )])))
        );

        let dat = r#"{ data: 1 / 2 }"#;
        let (_, mut eson) = parser::parse_root(dat).unwrap();
        eson.compute(&mut ctx);
        assert_eq!(
            eson,
            TokenChunk::single(Token::from(EsonDict::from(vec![(
                EsonKey::from("/"),
                TokenChunk::single(Token::from(EsonDict::from(vec![(
                    EsonKey::from("data"),
                    TokenChunk::single(EsonNumber::from(0.5).into())
                )])))
            )])))
        );
    }

    #[test]
    fn test_fmt_string() {
        let dat = r#"{
            name: "John",
            slogan: f"{ super.name } is a good boy",
        }"#;
        let (_, ref mut eson) = parser::parse_root(dat).unwrap();
        // dbg!(&eson);
        eson.compute(&mut Context::default());
        // dbg!(&eson);
        // todo!
    }

    #[test]
    fn test_simple() {
        let dat = r#"{ data: {
            a: 1,
            b: 2,
            c: 3,
        } }"#;
        let (_, mut eson) = parser::parse_root(dat).unwrap();

        let mut ctx = Context::default();
        eson.compute(&mut ctx);
        assert_eq!(
            eson,
            TokenChunk::single(Token::from(EsonDict::from(vec![(
                EsonKey::from("/"),
                TokenChunk::single(Token::from(EsonDict::from(vec![(
                    EsonKey::from("data"),
                    TokenChunk::single(Token::from(EsonDict::from(vec![
                        (
                            EsonKey::from("a"),
                            TokenChunk::single(EsonNumber::from(1).into())
                        ),
                        (
                            EsonKey::from("b"),
                            TokenChunk::single(EsonNumber::from(2).into())
                        ),
                        (
                            EsonKey::from("c"),
                            TokenChunk::single(EsonNumber::from(3).into())
                        ),
                    ])))
                )])))
            )])))
        );
    }
}
