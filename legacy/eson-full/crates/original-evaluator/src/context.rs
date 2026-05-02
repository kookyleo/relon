use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::ast::EsonEntity;
use crate::eson::EsonValue;
use crate::util_tree::{Navigate, Node};

#[derive(Debug)]
pub struct Context {
    _circular_ref_sign: HashSet<String>,
    _var: HashMap<String, EsonValue>,
}

impl Default for Context {
    fn default() -> Self {
        Context {
            _circular_ref_sign: HashSet::default(),
            _var: HashMap::default(),
        }
    }
}

pub(crate) trait Var {
    fn get_var(&self, name: String) -> Option<EsonValue>;
    fn set_var(&mut self, name: String, value: EsonValue);
    fn del_var(&mut self, name: String);
}

impl Var for Context {
    fn get_var(&self, name: String) -> Option<EsonValue> {
        self._var.get(name.as_str()).cloned()
    }

    fn set_var(&mut self, name: String, value: EsonValue) {
        self._var.insert(name.to_string(), value);
    }

    fn del_var(&mut self, name: String) {
        self._var.remove(name.as_str());
    }
}

pub(crate) trait CircularRefDetector {
    fn circular_ref_detection_sign(&mut self, node: &Rc<Node<EsonEntity>>);
    fn is_circular_ref_detected(&self, node: &Rc<Node<EsonEntity>>) -> bool;
    fn get_circle_ref_nodes(&self) -> Vec<String>;
    fn circular_ref_detection_sign_clear(&mut self);
}

impl CircularRefDetector for Context {
    fn circular_ref_detection_sign(&mut self, node: &Rc<Node<EsonEntity>>) {
        self._circular_ref_sign.insert(node.full_name());
    }

    fn is_circular_ref_detected(&self, node: &Rc<Node<EsonEntity>>) -> bool {
        self._circular_ref_sign.contains(&node.full_name())
    }

    fn get_circle_ref_nodes(&self) -> Vec<String> {
        self._circular_ref_sign.iter().cloned().collect()
    }

    fn circular_ref_detection_sign_clear(&mut self) {
        self._circular_ref_sign.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eson::EsonValue;

    #[test]
    fn test_context() {
        let mut ctx = Context::default();
        ctx.set_var(String::from("a"), EsonValue::EsonNumberInt(1));
        assert_eq!(
            ctx.get_var(String::from("a")),
            Some(EsonValue::EsonNumberInt(1))
        );
        ctx.del_var(String::from("a"));
        assert_eq!(ctx.get_var(String::from("a")), None);
    }

    #[test]
    fn test_circular_ref_detector() {}
}
