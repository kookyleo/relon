use std::borrow::Borrow;

use parser::{
    Eson, EsonRef, EsonSegment, Expr, FmtString, Key, PrattParser, RefIndex, Token, TokenChunk,
};

use crate::context::Context;
use crate::eval::Eval;

pub trait Compute {
    fn compute(self, ctx: &mut Context);
    fn val(self) -> EsonSegment;
}
//
// impl Compute for &mut Eson {
//     fn compute(self, ctx: &mut Context) {
//         match self {
//             Eson::Dict(decorator, dict) => {
//                 // todo! decorator
//                 dict.iter_mut().for_each(|kv| {
//                     kv.compute(ctx);
//                 });
//             }
//             Eson::List(decorator, lst) => {
//                 // todo! decorator
//                 lst.iter_mut().for_each(|ev| {
//                     ev.compute(ctx);
//                 });
//             }
//         }
//     }
//
//     fn val(self) -> EsonVal {
//         // self.clone().into()
//         mem::replace(self, Eson::default()).into()
//     }
// }
//
// impl Compute for &mut EsonVal {
//     fn compute(self, mut ctx: RefMut<Context>) {
//         match self {
//             EsonVal::List(list) => list.iter_mut().for_each(|ev| ev.compute(ctx)),
//             EsonVal::Dict(dict) => dict.iter_mut().for_each(|kv| {
//                 kv.compute(ctx);
//             }),
//             EsonVal::FnCall(name, args) => {
//                 let args = args.iter_mut().map(|tc| tc.eval(ctx)).collect();
//                 *self = ctx.function_call(name, args);
//             }
//             EsonVal::FmtString(fs) => {
//                 *self = EsonVal::String(fmt_string_to_string(ctx, fs));
//             }
//             EsonVal::Var(name) => {
//                 *self = var_to_val(ctx, name);
//             }
//             EsonVal::Ref(r) => {
//                 *self = get_value_by_ref(ctx, r).clone();
//             }
//             _ => {}
//         }
//     }
//
//     fn val(self) -> EsonVal {
//         self.clone()
//     }
// }
//
// impl Compute for (&Key, &mut EsonVal) {
//     fn compute(self, ctx: RefMut<Context>) {
//         // todo! Key
//         self.1.compute(ctx);
//     }
//
//     fn val(self) -> EsonVal {
//         todo!()
//     }
// }
//
// // fn compute(ctx: &mut Context, ev: &mut EsonVal) {
// //     ctx.set_value_ref(ev);
// //     ev.compute(ctx);
// // }
//
// fn var_to_val(ctx: &mut Context, name: &str) -> EsonVal {
//     if let Some(v) = ctx.get_variable(name) {
//         v.clone()
//     } else {
//         EsonVal::Null
//     }
// }
//
// fn fmt_string_to_string(ctx: &mut Context, fs: &mut Vec<FmtString>) -> String {
//     fs.iter_mut()
//         .map(|v| match v {
//             FmtString::Lit(s) => s.clone(),
//             FmtString::Expr(expr) => expr.eval(ctx).to_string(),
//         })
//         .collect::<Vec<_>>()
//         .join("")
// }
//
// pub(crate) fn get_value_by_ref<'a>(ctx: &mut Context, p1: &mut EsonRef) -> &'a EsonVal {
//     let p0 = ctx.get_value_ref().unwrap();
//
//     dbg!(&p0);
//
//     let p1: Vec<RefIndex> = match p1 {
//         EsonRef::Curr(r) => {
//             let mut p = ctx.get_cursor().clone();
//             p.append(r);
//             p
//         }
//         EsonRef::Super(r) => {
//             let mut p = ctx.get_cursor().clone();
//             p.pop();
//             p.append(r);
//             p
//         }
//         EsonRef::Root(r) => r.clone(),
//     };
//
//     let binding = p0.borrow();
//     let mut cur = binding.deref();
//     for key in p1.iter() {
//         match cur {
//             EsonVal::Dict(map) => {
//                 let k = match key {
//                     RefIndex::Str(s) => s,
//                     RefIndex::Int(i) => {
//                         return &EsonVal::Null;
//                     }
//                 };
//                 let k = Key::from(k.clone());
//                 if let Some(next) = map.get(&k) {
//                     cur = next;
//                 } else {
//                     return &EsonVal::Null;
//                 }
//             }
//             EsonVal::List(ref arr) => {
//                 if let RefIndex::Int(i) = key {
//                     if let Some(next) = arr.get(*i) {
//                         cur = next;
//                     } else {
//                         return &EsonVal::Null;
//                     }
//                 } else {
//                     return &EsonVal::Null;
//                 }
//             }
//             _ => {
//                 return &EsonVal::Null;
//             }
//         }
//     }
//     &EsonVal::Null
// }

// #[cfg(test)]
// mod tests {
//     use crate::compute::{compute, Compute};
//     use crate::context::Context;
//     use crate::eval::Eval;
//     use parser::{EsonRef, EsonVal, Key, PrattParser, RefIndex, Token, TokenChunk};
//
//     #[test]
//     fn test_ref() {
//         let mut ctx = Context::new();
//
//         let mut eson = EsonVal::Dict(
//             vec![
//                 (Key::from("a"), EsonVal::Int(1)),
//                 (
//                     Key::from("b"),
//                     EsonRef::Root(vec![RefIndex::Str("a".to_string())]).into(),
//                 ),
//             ]
//             .into_iter()
//             .collect::<std::collections::HashMap<_, _>>(),
//         );
//
//         let v = compute(&mut ctx, &mut eson);
//         dbg!(v);
//     }
// }
