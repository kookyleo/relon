fn main() {
    let src = r#"{
        @schema Page<T>: {
            List<T> items: *
        }
    }"#;
    let node = relon_parser::parse_document(src).unwrap();
    println!("{:#?}", node.expr);
}
