use std::cell::RefCell;
use std::rc::Rc;

use parser::{EsonRef, EsonVal, Key, RefIndex};

pub trait Travel {
    /// reset pos to root
    fn reset(&self);
    /// move pos to path
    fn move_to(&self, path: &[RefIndex]);
    /// move pos to child
    fn move_to_child(&self, key: RefIndex);
    /// get abs path from ref
    fn abs(&self, r: &mut EsonRef) -> Vec<RefIndex>;

    /// query value from pos
    fn value(&self) -> Result<&EsonVal, String>;
}

#[derive(Debug)]
pub struct EsonValTraveler<'a> {
    val: &'a EsonVal,
    pos: Rc<RefCell<Vec<RefIndex>>>,
}

impl<'a> From<&'a EsonVal> for EsonValTraveler<'a> {
    fn from(e: &EsonVal) -> EsonValTraveler {
        EsonValTraveler {
            val: e,
            pos: Rc::new(RefCell::new(vec![])),
        }
    }
}

impl Travel for EsonValTraveler<'_> {
    fn reset(&self) {
        self.pos.borrow_mut().clear();
    }

    fn move_to(&self, path: &[RefIndex]) {
        let mut path_ref = self.pos.borrow_mut();
        path_ref.clear();
        path_ref.extend_from_slice(path);
    }

    fn move_to_child(&self, key: RefIndex) {
        let mut path_ref = self.pos.borrow_mut();
        path_ref.push(key);
    }

    fn abs(&self, er: &mut EsonRef) -> Vec<RefIndex> {
        match er {
            EsonRef::Curr(ref mut r) => {
                let p = self.pos.borrow();
                let mut p = p.clone();
                p.append(r);
                p
            }
            EsonRef::Super(ref mut r) => {
                let p = self.pos.borrow();
                let mut p = p.clone();
                p.pop();
                p.append(r);
                p
            }
            EsonRef::Root(ref mut r) => {
                r.to_vec()
            }
        }
    }

    fn value(&self) -> Result<&EsonVal, String> {
        let root = self.val;
        // let mut current_value = &*root;
        let mut current_value = root;

        let p = self.pos.borrow();

        // for key in &*p {
        for key in p.iter() {
            match current_value {
                EsonVal::Dict(map) => {
                    let k = match key {
                        RefIndex::Str(s) => s,
                        RefIndex::Int(i) => {
                            return Err(format!("Invalid index '{}'", i));
                        }
                    };
                    let k = Key::from(k.clone());
                    if let Some(next) = map.get(&k) {
                        current_value = next
                    } else {
                        return Err(format!("No such key '{}' in the path", k));
                    }
                }
                EsonVal::List(ref arr) => {
                    if let RefIndex::Int(i) = key {
                        if let Some(next) = arr.get(*i) {
                            current_value = next;
                        } else {
                            return Err(format!("No such index '{}' in the path", i));
                        }
                    } else {
                        return Err("Not an array".to_owned());
                    }
                }
                _ => {
                    return Err("Not an dict or list".to_owned());
                }
            }
        }
        Ok(current_value)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use parser::{Annotation, EsonVal, Key};
    use tests::traveler::Travel;

    #[test]
    fn test_basic() {
        use super::*;

        let eson_dat = EsonVal::Dict(
            vec![
                (
                    Key::from("data1"),
                    EsonVal::String(String::from("value of 1")),
                ),
                (
                    Key::from("data2"),
                    EsonVal::Dict(
                        vec![
                            (
                                Key::from("data2_1"),
                                EsonVal::String(String::from("value of 2_1")),
                            ),
                            (
                                Key::from("data2_2"),
                                EsonVal::String(String::from("value of 2_2")),
                            ),
                        ]
                            .into_iter()
                            .collect::<HashMap<_, _>>(),
                    ),
                ),
                (
                    Key::from("data3"),
                    EsonVal::List(vec![
                        EsonVal::String(String::from("value of 3_1")),
                        EsonVal::String(String::from("value of 3_2")),
                    ]),
                ),
            ]
                .into_iter()
                .collect::<HashMap<_, _>>(),
        );

        let t = EsonValTraveler::from(&eson_dat);

        t.move_to(&vec![RefIndex::Str("data1".to_string())]);
        assert_eq!(
            t.abs(&mut EsonRef::Curr(vec![])),
            vec![RefIndex::Str("data1".to_string())]
        );
        assert_eq!(t.value(), Ok(&EsonVal::String(String::from("value of 1"))));

        t.move_to(&vec![RefIndex::Str("data2".to_string())]);
        assert_eq!(
            t.abs(&mut EsonRef::Curr(vec![RefIndex::Str(
                "data2_1".to_string()
            )])),
            vec![
                RefIndex::Str("data2".to_string()),
                RefIndex::Str("data2_1".to_string()),
            ]
        );
        t.move_to(
            t.abs(&mut EsonRef::Curr(vec![RefIndex::Str(
                "data2_1".to_string(),
            )]))
                .as_slice(),
        );
        assert_eq!(
            t.value(),
            Ok(&EsonVal::String(String::from("value of 2_1")))
        );

        t.move_to(&vec![RefIndex::Str("data3".to_string())]);
        assert_eq!(
            t.abs(&mut EsonRef::Curr(vec![RefIndex::Int(0)])),
            vec![RefIndex::Str("data3".to_string()), RefIndex::Int(0)]
        );
        t.move_to(t.abs(&mut EsonRef::Curr(vec![RefIndex::Int(0)])).as_slice());
        assert_eq!(
            t.value(),
            Ok(&EsonVal::String(String::from("value of 3_1")))
        );

        t.move_to(&vec![RefIndex::Str("data4".to_string())]);
        // dbg!(&t.value());
        // "No such key 'data4' in the path"
        assert!(t.value().is_err());

        // test move_to_child
        t.move_to(&vec![RefIndex::Str("data2".to_string())]);
        t.move_to_child(RefIndex::Str("data2_2".to_string()));
        assert_eq!(
            t.abs(&mut EsonRef::Curr(vec![])),
            vec![
                RefIndex::Str("data2".to_string()),
                RefIndex::Str("data2_2".to_string()),
            ]
        );
        assert_eq!(
            t.value(),
            Ok(&EsonVal::String(String::from("value of 2_2")))
        );

        // test reset
        t.reset();
        assert_eq!(t.abs(&mut EsonRef::Curr(vec![])), vec![]);
    }

    #[test]
    fn test_not_dict_or_list() {
        use super::*;

        let esonval = EsonVal::String(String::from("value1"));
        let t = EsonValTraveler::from(&esonval);
        t.move_to(&vec![RefIndex::Str("data1".to_string())]);
        assert_eq!(
            t.abs(&mut EsonRef::Curr(vec![])),
            vec![RefIndex::Str("data1".to_string())]
        );
        // "Not an dict or list"
        assert!(t.value().is_err());
    }

    #[test]
    fn test_super_and_root() {
        use super::*;

        let esonval = EsonVal::Dict(
            vec![(
                Key::from("data1"),
                EsonVal::Dict(
                    vec![
                        (
                            Key::from("data1_1"),
                            EsonVal::String(String::from("value of 1_1")),
                        ),
                        (
                            Key::from("data1_2"),
                            EsonVal::String(String::from("value of 1_2")),
                        ),
                    ]
                        .into_iter()
                        .collect::<HashMap<_, _>>(),
                ),
            )]
                .into_iter()
                .collect::<HashMap<_, _>>(),
        );
        let t = EsonValTraveler::from(&esonval);
        t.move_to(&vec![
            RefIndex::Str("data1".to_string()),
            RefIndex::Str("data1_1".to_string()),
        ]);
        // dbg!(t.value());
        assert_eq!(
            t.abs(&mut EsonRef::Curr(vec![])),
            vec![
                RefIndex::Str("data1".to_string()),
                RefIndex::Str("data1_1".to_string()),
            ]
        );
        assert_eq!(
            t.abs(&mut EsonRef::Super(vec![])),
            vec![RefIndex::Str("data1".to_string())]
        );
        assert_eq!(t.abs(&mut EsonRef::Root(vec![])), vec![]);

        t.move_to(&vec![RefIndex::Str("data1".to_string())]);
        assert_eq!(t.abs(&mut EsonRef::Super(vec![])), vec![]);
    }
}
