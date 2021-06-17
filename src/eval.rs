use crate::arena::Arena;
use crate::data::Environment;
use crate::object::InnerObject;
use crate::object::{BuiltInFn, FnArgs, FunctionValue, GcObject, LispFn, Object, Symbol, Value};
use crate::opcode::OpCode;
use fn_macros::lisp_fn;
use std::convert::TryInto;

use anyhow::{bail, Result};

#[derive(Debug, PartialEq)]
pub enum Error {
    ArgCount(u16, u16),
    VoidFunction(Symbol),
    VoidVariable(Symbol),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::ArgCount(exp, act) => write!(f, "Expected {} arg(s), found {}", exp, act),
            Error::VoidFunction(func) => write!(f, "Void function: {}", func.get_name()),
            Error::VoidVariable(var) => write!(f, "Void variable: {}", var.get_name()),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Clone)]
struct CallFrame<'a> {
    ip: Ip,
    func: &'a LispFn,
    start: usize,
}

#[derive(Clone)]
struct Ip {
    range: std::ops::Range<*const u8>,
    ip: *const u8,
}

impl Ip {
    #[allow(clippy::missing_const_for_fn)]
    fn new(vec: &[u8]) -> Self {
        Ip {
            range: vec.as_ptr_range(),
            ip: vec.as_ptr(),
        }
    }

    fn next(&mut self) -> u8 {
        unsafe {
            debug_assert!(self.range.contains(&self.ip));
            let x = *self.ip;
            self.ip = self.ip.add(1);
            x
        }
    }

    fn jump(&mut self, offset: i16) {
        unsafe {
            self.ip = self.ip.offset(offset as isize);
            debug_assert!(self.range.contains(&self.ip));
        }
    }

    fn take_arg(&mut self) -> usize {
        self.next() as usize
    }

    fn take_double_arg(&mut self) -> usize {
        let upper = self.next();
        let lower = self.next();
        (upper as usize) << 8 | lower as usize
    }
}

impl<'a> CallFrame<'a> {
    fn new(func: &'a LispFn, frame_start: usize) -> CallFrame {
        CallFrame {
            ip: Ip::new(&func.op_codes),
            func,
            start: frame_start,
        }
    }

    fn get_const(&self, i: usize) -> InnerObject {
        unsafe { self.func.constants.get(i).unwrap().inner() }
    }
}

pub fn from_slice<'borrow, 'obj>(
    slice: &'borrow [InnerObject],
    _arena: &'obj Arena,
) -> &'borrow [Object<'obj>] {
    let ptr = slice.as_ptr() as *const Object<'obj>;
    let len = slice.len();
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

trait LispStack {
    fn from_end(&self, i: usize) -> usize;
    fn push_ref(&mut self, i: usize);
    fn set_ref(&mut self, i: usize);
    fn ref_at(&self, i: usize) -> InnerObject;
    fn take_slice(&self, i: usize) -> &[InnerObject];
}

impl LispStack for Vec<InnerObject> {
    fn from_end(&self, i: usize) -> usize {
        debug_assert!(i < self.len());
        self.len() - (i + 1)
    }

    fn push_ref(&mut self, i: usize) {
        self.push(self.ref_at(i));
    }

    fn set_ref(&mut self, i: usize) {
        self.swap_remove(self.from_end(i));
    }

    fn ref_at(&self, i: usize) -> InnerObject {
        *self.get(self.from_end(i)).unwrap()
    }

    fn take_slice(&self, i: usize) -> &[InnerObject] {
        &self[self.from_end(i - 1)..]
    }
}

pub struct Routine<'a> {
    stack: Vec<InnerObject>,
    call_frames: Vec<CallFrame<'a>>,
    frame: CallFrame<'a>,
}

impl<'a, 'ob> Routine<'a> {
    fn process_args(&mut self, count: u16, args: FnArgs, _sym: Symbol) -> Result<()> {
        if count < args.required {
            bail!(Error::ArgCount(args.required, count));
        }
        let total_args = args.required + args.optional;
        if !args.rest && (count > total_args) {
            bail!(Error::ArgCount(total_args, count));
        }
        if total_args > count {
            for _ in 0..(total_args - count) {
                self.stack.push(InnerObject::nil());
            }
        }
        Ok(())
    }

    fn varref(&mut self, idx: usize, env: &Environment<'a>) -> Result<()> {
        let symbol = self.frame.get_const(idx);
        if let Value::Symbol(sym) = symbol.val() {
            let value = match env.vars.get(&sym) {
                Some(x) => x,
                None => bail!(Error::VoidVariable(sym)),
            };
            self.stack.push(unsafe { value.inner() });
            Ok(())
        } else {
            panic!("Varref was not a symbol: {:?}", symbol);
        }
    }

    fn varset(&mut self, idx: usize, env: &mut Environment) -> Result<()> {
        let obj: Object = self.frame.get_const(idx).into();
        let symbol: Symbol = obj.try_into()?;
        let value = self.stack.pop().unwrap();
        crate::data::set(symbol, value.into(), env);
        Ok(())
    }

    fn call(&mut self, arg_cnt: u16, env: &mut Environment<'ob>, arena: &'ob Arena) -> Result<()> {
        let fn_idx = arg_cnt as usize;
        let sym = match self.stack.ref_at(fn_idx).val() {
            Value::Symbol(x) => x,
            x => panic!("Expected symbol for call found {:?}", x),
        };
        match sym.get_func().ok_or(Error::VoidFunction(sym))?.val() {
            FunctionValue::LispFn(func) => {
                self.process_args(arg_cnt, func.args, sym)?;
                self.call_frames.push(self.frame.clone());
                self.frame = CallFrame::new(
                    // TODO: This is unsound. We don't know that this will live
                    // long enough
                    unsafe { std::mem::transmute(func) },
                    self.stack.from_end(fn_idx),
                );
            }
            FunctionValue::SubrFn(func) => {
                self.process_args(arg_cnt, func.args, sym)?;
                self.call_subr(func.subr, arg_cnt as usize, env, arena)?;
            }
        };
        Ok(())
    }

    fn call_subr(
        &mut self,
        func: BuiltInFn,
        args: usize,
        env: &mut Environment<'ob>,
        arena: &'ob Arena,
    ) -> Result<()> {
        let i = self.stack.from_end(args);
        let slice = self.stack.take_slice(args);
        let new_slice = from_slice(slice, arena);
        let result = func(new_slice, env, arena)?;
        self.stack[i] = unsafe { result.inner() };
        self.stack.truncate(i + 1);
        Ok(())
    }

    pub fn execute(
        func: &LispFn,
        env: &mut Environment<'ob>,
        arena: &'ob Arena,
    ) -> Result<GcObject> {
        use OpCode as op;
        let mut rout = Routine {
            stack: vec![],
            call_frames: vec![],
            frame: CallFrame::new(func, 0),
        };
        loop {
            // println!("{:?}", rout.stack);
            let op = unsafe { op::from_unchecked(rout.frame.ip.next()) };
            // println!("op : {:?}", op);
            match op {
                op::StackRef0 => rout.stack.push_ref(0),
                op::StackRef1 => rout.stack.push_ref(1),
                op::StackRef2 => rout.stack.push_ref(2),
                op::StackRef3 => rout.stack.push_ref(3),
                op::StackRef4 => rout.stack.push_ref(4),
                op::StackRef5 => rout.stack.push_ref(5),
                op::StackRefN => {
                    let idx = rout.frame.ip.take_arg();
                    rout.stack.push_ref(idx);
                }
                op::StackRefN2 => {
                    let idx = rout.frame.ip.take_double_arg();
                    rout.stack.push_ref(idx);
                }
                op::StackSet0 => rout.stack.set_ref(0),
                op::StackSet1 => rout.stack.set_ref(1),
                op::StackSet2 => rout.stack.set_ref(2),
                op::StackSet3 => rout.stack.set_ref(3),
                op::StackSet4 => rout.stack.set_ref(4),
                op::StackSet5 => rout.stack.set_ref(5),
                op::StackSetN => {
                    let idx = rout.frame.ip.take_arg();
                    rout.stack.set_ref(idx);
                }
                op::StackSetN2 => {
                    let idx = rout.frame.ip.take_double_arg();
                    rout.stack.set_ref(idx);
                }
                op::Constant0 => rout.stack.push(rout.frame.get_const(0)),
                op::Constant1 => rout.stack.push(rout.frame.get_const(1)),
                op::Constant2 => rout.stack.push(rout.frame.get_const(2)),
                op::Constant3 => rout.stack.push(rout.frame.get_const(3)),
                op::Constant4 => rout.stack.push(rout.frame.get_const(4)),
                op::Constant5 => rout.stack.push(rout.frame.get_const(5)),
                op::ConstantN => {
                    let idx = rout.frame.ip.take_arg();
                    rout.stack.push(rout.frame.get_const(idx))
                }
                op::ConstantN2 => {
                    let idx = rout.frame.ip.take_double_arg();
                    rout.stack.push(rout.frame.get_const(idx))
                }
                op::VarRef0 => rout.varref(0, env)?,
                op::VarRef1 => rout.varref(1, env)?,
                op::VarRef2 => rout.varref(2, env)?,
                op::VarRef3 => rout.varref(3, env)?,
                op::VarRef4 => rout.varref(4, env)?,
                op::VarRef5 => rout.varref(5, env)?,
                op::VarRefN => {
                    let idx = rout.frame.ip.take_arg();
                    rout.varref(idx, env)?
                }
                op::VarRefN2 => {
                    let idx = rout.frame.ip.take_double_arg();
                    rout.varref(idx, env)?
                }
                op::VarSet0 => rout.varset(0, env)?,
                op::VarSet1 => rout.varset(1, env)?,
                op::VarSet2 => rout.varset(2, env)?,
                op::VarSet3 => rout.varset(3, env)?,
                op::VarSet4 => rout.varset(4, env)?,
                op::VarSet5 => rout.varset(5, env)?,
                op::VarSetN => {
                    let idx = rout.frame.ip.take_arg();
                    rout.varset(idx, env)?
                }
                op::VarSetN2 => {
                    let idx = rout.frame.ip.take_double_arg();
                    rout.varset(idx, env)?
                }
                op::Call0 => rout.call(0, env, arena)?,
                op::Call1 => rout.call(1, env, arena)?,
                op::Call2 => rout.call(2, env, arena)?,
                op::Call3 => rout.call(3, env, arena)?,
                op::Discard => {
                    rout.stack.pop();
                }
                op::Duplicate => {
                    let value = *rout.stack.last().unwrap();
                    rout.stack.push(value);
                }
                op::Jump => {
                    let offset = rout.frame.ip.take_double_arg();
                    rout.frame.ip.jump(offset as i16);
                }
                op::JumpNil => {
                    let cond = rout.stack.pop().unwrap();
                    let offset = rout.frame.ip.take_double_arg();
                    if matches!(cond.val(), Value::Nil) {
                        rout.frame.ip.jump(offset as i16);
                    }
                }
                op::JumpNilElsePop => {
                    let cond = rout.stack.last().unwrap();
                    let offset = rout.frame.ip.take_double_arg();
                    if matches!(cond.val(), Value::Nil) {
                        rout.frame.ip.jump(offset as i16);
                    } else {
                        rout.stack.pop();
                    }
                }
                op::Ret => {
                    if rout.call_frames.is_empty() {
                        return Ok(rout.stack.pop().unwrap().into());
                    } else {
                        let var = rout.stack.pop().unwrap();
                        rout.stack[rout.frame.start] = var;
                        rout.stack.truncate(rout.frame.start + 1);
                        rout.frame = rout.call_frames.pop().unwrap();
                    }
                }
                x => panic!("unknown opcode {:?}", x),
            }
        }
    }
}

#[lisp_fn]
pub fn eval<'ob>(
    form: Object<'ob>,
    env: &mut Environment<'ob>,
    arena: &'ob Arena,
) -> anyhow::Result<Object<'ob>> {
    let func = crate::compile::Exp::compile(form)?.into();
    Routine::execute(&func, env, arena)
}

defsubr!(eval);

#[cfg(test)]
mod test {
    use super::*;
    use crate::arena::Arena;
    use crate::compile::Exp;
    use crate::object::IntoObject;
    use crate::reader::Reader;

    fn test_eval(sexp: &str, expect: Object) {
        let arena = &Arena::new();
        let obj = Reader::read(sexp, arena).unwrap().0;
        let func = Exp::compile(obj).unwrap().into();
        let env = &mut Environment::default();
        let val = Routine::execute(&func, env, arena).unwrap();
        assert_eq!(val, expect);
    }

    #[test]
    fn compute() {
        let arena = &Arena::new();
        test_eval("(- 7 (- 13 (* 3 (+ 7 (+ 13 1 2)))))", 63.into_obj(arena));
        test_eval("7", (7).into_obj(arena));
        test_eval("(+ 1 2.5)", (3.5).into_obj(arena));
    }

    #[test]
    fn let_form() {
        let arena = &Arena::new();
        test_eval("(let ((foo 5) (bar 8)) (+ foo bar))", 13.into_obj(arena));
        test_eval("(let ((foo 5) (bar 8)) (+ 1 bar))", 9.into_obj(arena));
    }

    #[test]
    fn jump() {
        let arena = &Arena::new();
        test_eval("(+ 7 (if nil 11 3))", 10.into_obj(arena));
        test_eval("(+ 7 (if t 11 3) 4)", 22.into_obj(arena));
        test_eval(
            "(let ((foo 7) (bar t)) (+ 7 (if bar foo 3)))",
            14.into_obj(arena),
        );
        test_eval(
            "(let ((foo 7) (bar nil)) (+ 7 (if bar foo 3)))",
            10.into_obj(arena),
        );
        test_eval(
            "(let ((foo (+ 3 4)) (bar t)) (+ 7 (if bar foo 3)))",
            14.into_obj(arena),
        );
        test_eval("(if nil 11)", false.into_obj(arena));
        test_eval("(if t 11)", 11.into_obj(arena));
    }

    #[test]
    fn loops() {
        let arena = &Arena::new();
        test_eval("(while nil)", false.into_obj(arena));
        test_eval("(while nil (set 'foo 7))", false.into_obj(arena));
        test_eval(
            "(let ((foo t)) (while foo (setq foo nil)))",
            false.into_obj(arena),
        );
        test_eval(
            "(let ((foo 10) (bar 0)) (while (> foo 3) (setq bar (1+ bar)) (setq foo (1- foo))) bar)",
            7.into_obj(arena),
        );
    }

    #[test]
    fn variables() {
        let arena = &Arena::new();
        test_eval("(progn (set 'foo 5) foo)", 5.into_obj(arena));
        test_eval("(let ((foo 1)) (setq foo 2) foo)", 2.into_obj(arena));
        test_eval("(progn (setq foo 2) foo)", 2.into_obj(arena));
    }

    #[test]
    fn call() {
        let arena = &Arena::new();
        test_eval(
            "(progn
(defalias 'bottom (lambda (x y z) (+ x z) (* x (+ y z))))
(defalias 'middle (lambda (x y z) (+ (bottom x z y) (bottom x z y))))
(middle 7 3 13))",
            (224).into_obj(arena),
        );
    }

    fn test_eval_error(sexp: &str, error: Error) {
        let arena = &Arena::new();
        let obj = Reader::read(sexp, arena).unwrap().0;
        let func = Exp::compile(obj).unwrap().into();
        let env = &mut Environment::default();
        let val = Routine::execute(&func, env, arena);
        assert_eq!(val.err().unwrap().downcast::<Error>().unwrap(), (error));
    }

    #[test]
    fn errors() {
        test_eval_error(
            "(bad-function-name)",
            Error::VoidFunction(crate::intern::intern("bad-function-name")),
        );
        test_eval_error("(1+ 1 2)", Error::ArgCount(1, 2));
        test_eval_error("(/)", Error::ArgCount(1, 0));
    }
}
