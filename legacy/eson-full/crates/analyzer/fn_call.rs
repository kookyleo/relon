use parser::EsonSegment;

struct Udf {}

impl Udf {
    pub fn call(&self, name: &str, args: &[EsonSegment]) -> EsonSegment {
        match name {
            "add" => {
                let mut sum = 0;
                for arg in args {
                    match arg {
                        EsonSegment::Int(i) => {
                            sum += i;
                        }
                        EsonSegment::Float(f) => {
                            sum += *f as i64;
                        }
                        _ => {}
                    }
                }
                EsonSegment::Int(sum)
            }
            _ => EsonSegment::Null,
        }
    }
}

#[cfg(test)]
mod tests {
    use parser::eson_seg;

    use super::*;

    #[test]
    fn test_udf_call_add() {
        let udf = Udf {};
        let r = udf.call("add", &[
            EsonSegment::Int(1),
            EsonSegment::Int(2),
        ]);
        dbg!(r);
    }


    #[test]
    fn test_add() {
        let input = r##"{ a: add(1, 2) }"##;
        let (remaining, expr) = eson_seg(input).unwrap();
        dbg!(expr);
    }
}