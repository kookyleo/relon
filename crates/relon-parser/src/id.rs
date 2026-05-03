use crate::{create_range, Span, TokenId};
use winnow::prelude::*;
use winnow::stream::Location;
use winnow::token::take_while;

/// Parse a valid identifier.
pub fn id<'a>(input: &mut Span<'a>) -> ModalResult<TokenId> {
    let start_offset = input.location();

    // Identifiers start with a letter or underscore, followed by letters, digits, or underscores.
    let head = winnow::token::one_of(('_', 'a'..='z', 'A'..='Z')).parse_next(input)?;
    let tail: &str = take_while(0.., ('_', 'a'..='z', 'A'..='Z', '0'..='9')).parse_next(input)?;

    let mut name = String::from(head);
    name.push_str(tail);

    let end_offset = input.location();
    Ok(TokenId(name, create_range(input, start_offset, end_offset)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_id() {
        let mut s = Span::new("name");
        assert_eq!(id(&mut s).unwrap().0, "name");

        let mut s = Span::new("_id123");
        assert_eq!(id(&mut s).unwrap().0, "_id123");

        let mut s = Span::new("123id");
        assert!(id(&mut s).is_err());
    }
}
