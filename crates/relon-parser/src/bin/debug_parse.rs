use relon_parser::parse_document;

fn main() {
    let src2 = r#"{
        @schema Page<T>: {
            List<T> items: *
        }
    }"#;
    match parse_document(src2) {
        Ok(node) => println!("Success: {:#?}", node),
        Err(e) => {
            println!("Error: {:?}", e);
            if let relon_parser::ParseDocumentError::Parse { offset, .. } = e {
                println!("Fails at: {:?}", &src2[offset..]);
            }
        }
    }
}