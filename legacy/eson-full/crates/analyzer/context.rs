use std::collections::HashMap;

use parser::{Eson, EsonSegment, Key, RefIndex};

#[derive(Debug)]
pub struct Context {
    variables: HashMap<String, EsonSegment>,
    functions: HashMap<String, EsonSegment>,
    // cursor: Vec<RefIndex>,
    // value_ref: Option<&'a EsonVal>,
}

impl Context {
    pub fn new() -> Self {
        Self {
            variables: Default::default(),
            functions: Default::default(),
            // cursor: vec![],
            // value_ref: None,
        }
    }

    pub fn get_variable(&self, name: &str) -> Option<&EsonSegment> {
        self.variables.get(name)
    }

    pub fn set_variable(&mut self, name: &str, value: EsonSegment) {
        self.variables.insert(name.to_string(), value);
    }

    pub fn get_cursor(&self) -> &Vec<RefIndex> {
        todo!()
        // &self.cursor
    }
    //
    pub fn set_cursor(&mut self, pos: Vec<RefIndex>) {
        todo!()
        // self.cursor = pos;
    }
    //
    pub fn get_value_ref(&self) -> Option<&EsonSegment> {
        todo!()
        // self.value_ref
    }
    //
    // // @todo!
    pub fn set_value_ref<'a>(&'a mut self, pos_root: &'a EsonSegment) {
        todo!()
        // self.value_ref = Some(pos_root);
    }

    pub fn function_call(&mut self, name: &str, args: Vec<EsonSegment>) -> EsonSegment {
        EsonSegment::Null
    }

    pub fn load_functions(&mut self, functions: HashMap<String, EsonSegment>) {
        self.functions = functions;
    }
}

#[cfg(test)]
mod tests {
    use parser::Key;

    use super::*;

    #[test]
    fn test_context() {
        let ev: EsonSegment = EsonSegment::Dict(
            vec![
                (
                    Key::from("name"),
                    EsonSegment::String(String::from("John")),
                ),
                (Key::from("age"), EsonSegment::Int(17)),
            ]
                .into_iter()
                .collect::<HashMap<_, _>>(),
        );

        let mut ctx = Context::new();
        ctx.set_variable("city", EsonSegment::String(String::from("Los Angeles")));
        ctx.set_variable("time", EsonSegment::Int(2024));

        // ctx.set_value_ref(&ev);

        assert_eq!(
            ctx.get_variable("city"),
            Some(&EsonSegment::String(String::from("Los Angeles")))
        );
        assert_eq!(ctx.get_variable("time"), Some(&EsonSegment::Int(2024)));
        // assert_eq!(ctx.get_cursor(), &vec![]);
        // assert_eq!(ctx.get_value_ref().borrow(), &ev);


        // // just for test
        // variables: vec![
        //     (
        //         String::from("name"),
        //         EsonVal::String(String::from("TestName")),
        //     ),
        //     (String::from("age"), EsonVal::Int(17)),
        // ]
        //     .into_iter()
        //     .collect::<HashMap<_, _>>(),
        // //
        // functions: Default::default(),
        // cursor: vec![],
        // value_ref: RefCell::new(EsonVal::Null),


        // let dat = r#"{
        //     "name": "John",
        //     "age": 42,
        //     "city": "London"
        // }"#;
        // let (_, value) = parser::eson_val(dat).unwrap();
        // let mut ctx = Context::new();
        //
        // let k = vec![RefIndex::Str(String::from("name"))];
        // assert_eq!(ctx.get(k), &EsonVal::String(String::from("John")));
        //
        // let k = vec![RefIndex::Str(String::from("age"))];
        // assert_eq!(ctx.get(k), &EsonVal::Int(42));
        //
        // let k = vec![RefIndex::Str(String::from("city"))];
        // assert_eq!(ctx.get(k), &EsonVal::String(String::from("London")));
    }

    #[test]
    fn test_q() {
        // let dat = r#"{
        //     "name": "John",
        //     "age": {
        //         "value": 42,
        //         "unit": "year"
        //     },
        //     "city": [
        //         "London",
        //         "New York",
        //         "Paris"
        //     ]
        // }"#;
        // let (_, mut ev) = parser::eson_val(dat).unwrap();
        //
        // dbg!(query(&ev, vec![RefIndex::Str(String::from("name"))]));
        // dbg!(query(
        //     &ev,
        //     vec![
        //         RefIndex::Str(String::from("age")),
        //         RefIndex::Str(String::from("value"))
        //     ]
        // ));
        // dbg!(query(
        //     &ev,
        //     vec![RefIndex::Str(String::from("city")), RefIndex::Int(1)]
        // ));
        //
        // set(
        //     &mut ev,
        //     vec![RefIndex::Str(String::from("name"))],
        //     EsonVal::String(String::from("yahoo")),
        // );
        // assert_eq!(
        //     query(&ev, vec![RefIndex::Str(String::from("name"))]),
        //     &EsonVal::String(String::from("yahoo"))
        // );
        //
        // set(
        //     &mut ev,
        //     vec![
        //         RefIndex::Str(String::from("fav")),
        //         RefIndex::Str(String::from("sport")),
        //     ],
        //     EsonVal::String("football".to_string()),
        // );
        // assert_eq!(
        //     query(
        //         &ev,
        //         vec![
        //             RefIndex::Str(String::from("fav")),
        //             RefIndex::Str(String::from("sport"))
        //         ]
        //     ),
        //     &EsonVal::String("football".to_string())
        // );
        //
        // fn exists(ev: &EsonVal, q: K) -> bool {
        //     let mut p: &EsonVal = ev;
        //     for ri in q {
        //         match (ri, p) {
        //             (RefIndex::Int(i), EsonVal::List(arr)) => {
        //                 if i as usize >= arr.len() {
        //                     return false;
        //                 }
        //                 p = arr.get(i as usize).unwrap();
        //             }
        //             (RefIndex::Str(s), EsonVal::Dict(map)) => {
        //                 if !map.contains_key(&Key::from(s.clone())) {
        //                     return false;
        //                 }
        //                 p = map.get(&Key::new(s.to_owned().as_str(), None)).unwrap();
        //             }
        //             _ => {}
        //         }
        //     }
        //     true
        // }
        //
        // fn set(ev: &mut EsonVal, q: K, value: EsonVal) {
        //     let mut p: &mut EsonVal = ev;
        //     for ri in q {
        //         p = match (ri, p) {
        //             (RefIndex::Int(i), EsonVal::List(arr)) => arr.get_mut(i as usize).unwrap(),
        //             (RefIndex::Str(s), EsonVal::Dict(map)) => map.get_mut(&Key::from(s)).unwrap(),
        //             _ => unreachable!(),
        //         }
        //     }
        //     *p = value;
        // }
        //
        // fn query(ev: &EsonVal, q: K) -> &EsonVal {
        //     let mut p: &EsonVal = ev;
        //     for ri in q {
        //         match (ri, p) {
        //             (RefIndex::Int(i), EsonVal::List(arr)) => {
        //                 p = arr.get(i as usize).unwrap();
        //             }
        //             (RefIndex::Str(s), EsonVal::Dict(map)) => {
        //                 p = map.get(&Key::from(s)).unwrap();
        //             }
        //             _ => {}
        //         }
        //     }
        //     p
        // }
    }

    #[test]
    fn test() {
        // let vec = vec![String::from("foo"), String::from("bar")];
        //
        // // 使用 loop 来遍历 vec
        // let mut index = 0;
        // loop {
        //     if index >= vec.len() {
        //         break; // 遍历结束，退出循环
        //     }
        //
        //     let element = &vec[index];
        //
        //     // 在这里可以对当前元素进行处理
        //     println!("Element: {}", element);
        //
        //     index += 1;
        // }
    }
}
