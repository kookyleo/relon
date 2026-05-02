use tokenizer::Token;

#[derive(Debug)]
pub struct Iter<'a, T> {
    inner: &'a mut Vec<T>,
    cursor: usize,
}

impl<'a> From<&'a mut Vec<Token>> for Iter<'a, Token> {
    fn from(inner: &'a mut Vec<Token>) -> Self {
        Iter::new(inner)
    }
}

impl<T> Iter<'_, T> {
    fn new(inner: &mut Vec<T>) -> Iter<T> {
        Iter { inner, cursor: 0 }
    }

    pub fn take_next(&mut self) -> Option<T> {
        if self.cursor >= self.inner.len() {
            return None;
        }
        let item = self.inner.remove(self.cursor);
        Some(item)
    }

    #[allow(dead_code)]
    pub fn next(&mut self) -> Option<&T> {
        let token = self.inner.get(self.cursor);
        self.cursor += 1;
        token
    }

    pub fn peek(&self) -> Option<&T> {
        if self.cursor >= self.inner.len() {
            return None;
        }
        Some(&self.inner[self.cursor])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iter() {
        let mut binding = vec![1, 2, 3];
        let mut iter = Iter::new(&mut binding);
        assert_eq!(iter.peek(), Some(&1));
        assert_eq!(iter.peek(), Some(&1));
        assert_eq!(iter.next(), Some(&1));
        assert_eq!(iter.peek(), Some(&2));
        assert_eq!(iter.peek(), Some(&2));
        assert_eq!(iter.next(), Some(&2));
        assert_eq!(iter.peek(), Some(&3));
        assert_eq!(iter.peek(), Some(&3));
        assert_eq!(iter.next(), Some(&3));
        assert_eq!(iter.peek(), None);
        assert_eq!(iter.peek(), None);
        assert_eq!(iter.next(), None);
        assert_eq!(iter.peek(), None);
        assert_eq!(iter.peek(), None);
        assert_eq!(iter.next(), None);
    }
}
