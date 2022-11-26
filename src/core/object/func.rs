use super::{
    super::{
        error::ArgError,
        gc::{Block, Context, Root},
    },
    display_slice, nil, CloneIn, IntoObject, LispString, LispVec,
};
use super::{GcObj, WithLifetime};
use crate::core::gc::{GcManaged, GcMark, Rt};
use std::fmt::{self, Debug, Display};

use anyhow::{bail, ensure, Result};
use fn_macros::Trace;
use streaming_iterator::StreamingIterator;
/// A function implemented in lisp. Note that all functions are byte compiled,
/// so this contains the byte-code representation of the function.
#[derive(PartialEq, Trace)]
pub(crate) struct ByteFn {
    gc: GcMark,
    #[no_trace]
    pub(crate) args: FnArgs,
    #[no_trace]
    pub(crate) depth: usize,
    op_codes: &'static LispString,
    constants: &'static LispVec,
}

define_unbox!(ByteFn, Func, &'ob ByteFn);

impl ByteFn {
    pub(crate) unsafe fn new(
        op_codes: &LispString,
        consts: &LispVec,
        args: FnArgs,
        depth: usize,
    ) -> Self {
        Self {
            gc: GcMark::default(),
            constants: unsafe { consts.with_lifetime() },
            op_codes: unsafe { op_codes.with_lifetime() },
            args,
            depth,
        }
    }

    pub(crate) fn codes<'a>(&'a self) -> &'a LispString {
        unsafe { std::mem::transmute::<&'static LispString, &'a LispString>(self.op_codes) }
    }

    pub(crate) fn constants<'a>(&'a self) -> &'a LispVec {
        unsafe { std::mem::transmute::<&'static LispVec, &'a LispVec>(self.constants) }
    }

    pub(crate) fn index(&self, index: usize) -> Option<GcObj> {
        match index {
            0 => Some((self.args.into_arg_spec() as i64).into()),
            1 => Some(self.codes().into()),
            2 => Some(self.constants().into()),
            3 => Some(self.depth.into()),
            _ => None,
        }
    }
}

impl<'new> CloneIn<'new, &'new Self> for ByteFn {
    fn clone_in<const C: bool>(&self, bk: &'new Block<C>) -> super::Gc<&'new Self> {
        let byte_fn = unsafe {
            Self::new(
                self.op_codes.clone_in(bk).get(),
                self.constants.clone_in(bk).get(),
                self.args,
                self.depth,
            )
        };
        byte_fn.into_obj(bk)
    }
}

impl Rt<&'static ByteFn> {
    pub(crate) fn code(&self) -> &Rt<&'static LispString> {
        unsafe { &*std::ptr::addr_of!(self.bind_unchecked().op_codes).cast() }
    }

    pub(crate) fn consts(&self) -> &Rt<&'static LispVec> {
        unsafe { &*std::ptr::addr_of!(self.bind_unchecked().constants).cast() }
    }
}

impl GcManaged for ByteFn {
    fn get_mark(&self) -> &GcMark {
        &self.gc
    }
}

impl Display for ByteFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let args = &self.args;
        let code = display_slice(self.op_codes);
        let consts = display_slice(self.constants);
        write!(f, "#[{args:?} {code} [{consts}]]")
    }
}

impl Debug for ByteFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ByteFn")
            .field("args", &self.args)
            .field("body", &self)
            .finish()
    }
}

pub(crate) struct ByteFnStreamIter<'rt, 'rs> {
    vector: &'rt Root<'rs, 'rt, &'static ByteFn>,
    elem: Option<Rt<GcObj<'static>>>,
    idx: usize,
}

impl<'rt, 'rs> Root<'rs, 'rt, &'static ByteFn> {
    #[allow(clippy::iter_not_returning_iterator)]
    pub(crate) fn iter(&'rt mut self) -> ByteFnStreamIter<'rt, 'rs> {
        ByteFnStreamIter {
            vector: self,
            elem: None,
            idx: 0,
        }
    }
}

impl<'rt, 'id> StreamingIterator for ByteFnStreamIter<'rt, 'id> {
    type Item = Rt<GcObj<'static>>;

    fn advance(&mut self) {
        unsafe {
            let obj = self.vector.bind_unchecked().index(self.idx);
            self.elem = obj.map(|x| Rt::new_unchecked(x.with_lifetime()));
            self.idx += 1;
        }
    }

    fn get(&self) -> Option<&Self::Item> {
        self.elem.as_ref()
    }
}

/// Argument requirments to a function.
#[derive(Copy, Clone, Debug, PartialEq)]
pub(crate) struct FnArgs {
    /// a &rest argument.
    pub(crate) rest: bool,
    /// minimum required arguments.
    pub(crate) required: u16,
    /// &optional arguments.
    pub(crate) optional: u16,
    /// If this function is advised.
    pub(crate) advice: bool,
}

impl FnArgs {
    pub(crate) fn from_arg_spec(spec: u64) -> Result<Self> {
        // The spec is an integer of the form NNNNNNNRMMMMMMM where the 7bit
        // MMMMMMM specifies the minimum number of arguments, the 7-bit NNNNNNN
        // specifies the maximum number of arguments (ignoring &rest) and the R
        // bit specifies whether there is a &rest argument to catch the
        // left-over arguments.
        ensure!(
            spec <= 0x7FFF,
            "Invalid bytecode argument spec: bits out of range"
        );
        let spec = spec as u16;
        let required = spec & 0x7F;
        let max = (spec >> 8) & 0x7F;
        let Some(optional) = max.checked_sub(required) else {
            bail!("Invalid bytecode argument spec: max of {max} was smaller then min {required}")
        };
        let rest = spec & 0x80 != 0;
        Ok(FnArgs {
            required,
            optional,
            rest,
            advice: false,
        })
    }

    pub(crate) fn into_arg_spec(self) -> u64 {
        let mut spec = self.required;
        let max = self.required + self.optional;
        spec |= max << 8;
        spec |= u16::from(self.rest) << 7;
        u64::from(spec)
    }

    /// Number of arguments needed to fill out the remaining slots on the stack.
    /// If a function has 3 required args and 2 optional, and it is called with
    /// 4 arguments, then 1 will be returned. Indicating that 1 additional `nil`
    /// argument should be added to the stack.
    pub(crate) fn num_of_fill_args(self, args: u16, name: &str) -> Result<u16> {
        if args < self.required {
            bail!(ArgError::new(self.required, args, name));
        }
        let total = self.required + self.optional;
        if !self.rest && (args > total) {
            bail!(ArgError::new(total, args, name));
        }
        Ok(total.saturating_sub(args))
    }
}

pub(crate) type BuiltInFn = for<'ob> fn(
    &[Rt<GcObj<'static>>],
    &mut Root<crate::core::env::Env>,
    &'ob mut Context,
) -> Result<GcObj<'ob>>;

pub(crate) struct SubrFn {
    pub(crate) subr: BuiltInFn,
    pub(crate) args: FnArgs,
    pub(crate) name: &'static str,
}
define_unbox!(SubrFn, Func, &'ob SubrFn);

impl SubrFn {
    pub(crate) fn call<'ob>(
        &self,
        args: &mut Root<Vec<GcObj<'static>>>,
        env: &mut Root<crate::core::env::Env>,
        cx: &'ob mut Context,
    ) -> Result<GcObj<'ob>> {
        {
            let args = args.as_mut(cx);
            let arg_cnt = args.len() as u16;
            let fill_args = self.args.num_of_fill_args(arg_cnt, self.name)?;
            for _ in 0..fill_args {
                args.push(nil());
            }
        }
        (self.subr)(args, env, cx)
    }
}

impl<'new> WithLifetime<'new> for &SubrFn {
    type Out = &'new SubrFn;

    unsafe fn with_lifetime(self) -> Self::Out {
        &*(self as *const SubrFn)
    }
}

impl Display for SubrFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(Builtin: {} -> {:?})", &self.name, self.args)
    }
}

impl Debug for SubrFn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl PartialEq for SubrFn {
    #[allow(clippy::fn_to_numeric_cast_any)]
    fn eq(&self, other: &Self) -> bool {
        let lhs = self.subr as *const BuiltInFn;
        let rhs = other.subr as *const BuiltInFn;
        lhs == rhs
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn check_arg_spec(spec: u64) {
        assert_eq!(spec, FnArgs::from_arg_spec(spec).unwrap().into_arg_spec());
    }

    #[test]
    fn test_arg_spec() {
        check_arg_spec(0);
        check_arg_spec(257);
        check_arg_spec(513);
        check_arg_spec(128);
        check_arg_spec(771);

        assert!(FnArgs::from_arg_spec(12345).is_err());
        assert!(FnArgs::from_arg_spec(1).is_err());
        assert!(FnArgs::from_arg_spec(0xFFFF).is_err());
    }
}
