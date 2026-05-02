// Just a nop placeholder
#[macro_export]
macro_rules! nop {
    ($($arg:tt)*) => {{}};
}

#[cfg(test)]
mod tests {
    #[test]
    fn test() {
        nop!();
    }
}
