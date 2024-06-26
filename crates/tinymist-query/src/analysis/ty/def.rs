use core::fmt;
use std::sync::Arc;

use ecow::{EcoString, EcoVec};
use parking_lot::RwLock;
use reflexo::vector::ir::DefId;
use typst::{
    foundations::{CastInfo, Element, Func, ParamInfo, Value},
    syntax::{ast, Span},
};

use crate::analysis::ty::param_mapping;

use super::{FlowBuiltinType, TypeCheckInfo};

struct RefDebug<'a>(&'a FlowType);

impl<'a> fmt::Debug for RefDebug<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            FlowType::Var(v) => write!(f, "@{}", v.1),
            _ => write!(f, "{:?}", self.0),
        }
    }
}

#[derive(Hash, Clone)]
#[allow(clippy::box_collection)]
pub(crate) enum FlowType {
    Clause,
    Undef,
    Content,
    Any,
    None,
    Infer,
    FlowNone,
    Auto,
    Boolean(Option<bool>),
    Builtin(FlowBuiltinType),
    Value(Box<(Value, Span)>),
    ValueDoc(Box<(Value, &'static str)>),
    Element(Element),

    Var(Box<(DefId, EcoString)>),
    Func(Box<FlowSignature>),
    Dict(FlowRecord),
    Array(Box<FlowType>),
    // Note: may contains spread types
    Tuple(EcoVec<FlowType>),
    With(Box<(FlowType, Vec<FlowArgs>)>),
    Args(Box<FlowArgs>),
    At(FlowAt),
    Unary(FlowUnaryType),
    Binary(FlowBinaryType),
    If(Box<FlowIfType>),
    Union(Box<Vec<FlowType>>),
    Let(Arc<FlowVarStore>),
}

impl fmt::Debug for FlowType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FlowType::Clause => f.write_str("Clause"),
            FlowType::Undef => f.write_str("Undef"),
            FlowType::Content => f.write_str("Content"),
            FlowType::Any => f.write_str("Any"),
            FlowType::None => f.write_str("None"),
            FlowType::Infer => f.write_str("Infer"),
            FlowType::FlowNone => f.write_str("FlowNone"),
            FlowType::Auto => f.write_str("Auto"),
            FlowType::Builtin(t) => write!(f, "{t:?}"),
            FlowType::Args(a) => write!(f, "&({a:?})"),
            FlowType::Func(s) => write!(f, "{s:?}"),
            FlowType::Dict(r) => write!(f, "{r:?}"),
            FlowType::Array(a) => write!(f, "Array<{a:?}>"),
            FlowType::Tuple(t) => {
                f.write_str("(")?;
                for t in t {
                    write!(f, "{t:?}, ")?;
                }
                f.write_str(")")
            }
            FlowType::With(w) => write!(f, "({:?}).with(..{:?})", w.0, w.1),
            FlowType::At(a) => write!(f, "{a:?}"),
            FlowType::Union(u) => {
                f.write_str("(")?;
                if let Some((first, u)) = u.split_first() {
                    write!(f, "{first:?}")?;
                    for u in u {
                        write!(f, " | {u:?}")?;
                    }
                }
                f.write_str(")")
            }
            FlowType::Let(v) => write!(f, "{v:?}"),
            FlowType::Var(v) => write!(f, "@{}", v.1),
            FlowType::Unary(u) => write!(f, "{u:?}"),
            FlowType::Binary(b) => write!(f, "{b:?}"),
            FlowType::If(i) => write!(f, "{i:?}"),
            FlowType::Value(v) => write!(f, "{v:?}", v = v.0),
            FlowType::ValueDoc(v) => write!(f, "{v:?}"),
            FlowType::Element(e) => write!(f, "{e:?}"),
            FlowType::Boolean(b) => {
                if let Some(b) = b {
                    write!(f, "{b}")
                } else {
                    f.write_str("Boolean")
                }
            }
        }
    }
}

impl FlowType {
    pub fn from_return_site(f: &Func, c: &'_ CastInfo) -> Option<Self> {
        use typst::foundations::func::Repr;
        match f.inner() {
            Repr::Element(e) => return Some(FlowType::Element(*e)),
            Repr::Closure(_) => {}
            Repr::With(w) => return FlowType::from_return_site(&w.0, c),
            Repr::Native(_) => {}
        };

        let ty = match c {
            CastInfo::Any => FlowType::Any,
            CastInfo::Value(v, doc) => FlowType::ValueDoc(Box::new((v.clone(), *doc))),
            CastInfo::Type(ty) => FlowType::Value(Box::new((Value::Type(*ty), Span::detached()))),
            CastInfo::Union(e) => FlowType::Union(Box::new(
                e.iter()
                    .flat_map(|e| Self::from_return_site(f, e))
                    .collect(),
            )),
        };

        Some(ty)
    }

    pub(crate) fn from_param_site(f: &Func, p: &ParamInfo, s: &CastInfo) -> Option<FlowType> {
        use typst::foundations::func::Repr;
        match f.inner() {
            Repr::Element(..) | Repr::Native(..) => {
                if let Some(ty) = param_mapping(f, p) {
                    return Some(ty);
                }
            }
            Repr::Closure(_) => {}
            Repr::With(w) => return FlowType::from_param_site(&w.0, p, s),
        };

        let ty = match &s {
            CastInfo::Any => FlowType::Any,
            CastInfo::Value(v, doc) => FlowType::ValueDoc(Box::new((v.clone(), *doc))),
            CastInfo::Type(ty) => FlowType::Value(Box::new((Value::Type(*ty), Span::detached()))),
            CastInfo::Union(e) => FlowType::Union(Box::new(
                e.iter()
                    .flat_map(|e| Self::from_param_site(f, p, e))
                    .collect(),
            )),
        };

        Some(ty)
    }

    pub(crate) fn is_dict(&self) -> bool {
        matches!(self, FlowType::Dict(..))
    }

    pub(crate) fn from_types(e: impl ExactSizeIterator<Item = FlowType>) -> Self {
        if e.len() == 0 {
            FlowType::Any
        } else if e.len() == 1 {
            let mut e = e;
            e.next().unwrap()
        } else {
            FlowType::Union(Box::new(e.collect()))
        }
    }

    pub(crate) fn signatures(
        &self,
        ty_chk: &TypeCheckInfo,
        principal: bool,
    ) -> Option<Vec<FlowSignature>> {
        let mut res = Vec::new();
        check_signatures(self, &mut res, ty_chk, principal);
        if res.is_empty() {
            None
        } else {
            // todo: bad performance
            for sig in &mut res {
                for pos in &mut sig.pos {
                    *pos = ty_chk.simplify(pos.clone(), principal);
                }
                for (_, ty) in &mut sig.named {
                    *ty = ty_chk.simplify(ty.clone(), principal);
                }
            }

            Some(res)
        }
    }
}

#[derive(Debug, Clone, Hash)]
pub(crate) enum FlowUnaryType {
    Pos(Box<FlowType>),
    Neg(Box<FlowType>),
    Not(Box<FlowType>),
    Context(Box<FlowType>),
}

impl FlowUnaryType {
    pub fn lhs(&self) -> &FlowType {
        match self {
            FlowUnaryType::Pos(e) => e,
            FlowUnaryType::Neg(e) => e,
            FlowUnaryType::Not(e) => e,
            FlowUnaryType::Context(e) => e,
        }
    }
}

#[derive(Debug, Clone, Hash)]
pub(crate) struct FlowBinaryType {
    pub op: ast::BinOp,
    pub operands: Box<(FlowType, FlowType)>,
}

impl FlowBinaryType {
    pub fn repr(&self) -> (&FlowType, &FlowType) {
        (&self.operands.0, &self.operands.1)
    }
}

#[derive(Debug, Clone, Hash)]
pub(crate) struct FlowIfType {
    pub cond: FlowType,
    pub then: FlowType,
    pub else_: FlowType,
}

impl FlowIfType {}

#[derive(Clone, Hash)]
pub(crate) struct FlowVarStore {
    pub lbs: Vec<FlowType>,
    pub ubs: Vec<FlowType>,
}

impl fmt::Debug for FlowVarStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // write!(f, "{}", self.name)
        // also where
        if !self.lbs.is_empty() {
            write!(f, " ⪰ {:?}", self.lbs[0])?;
            for lb in &self.lbs[1..] {
                write!(f, " | {lb:?}")?;
            }
        }
        if !self.ubs.is_empty() {
            write!(f, " ⪯ {:?}", self.ubs[0])?;
            for ub in &self.ubs[1..] {
                write!(f, " & {ub:?}")?;
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) enum FlowVarKind {
    Weak(Arc<RwLock<FlowVarStore>>),
}

#[derive(Clone)]
pub(crate) struct FlowVar {
    pub name: EcoString,
    pub id: DefId,
    pub kind: FlowVarKind,
}

impl std::hash::Hash for FlowVar {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        0.hash(state);
        self.id.hash(state);
    }
}

impl fmt::Debug for FlowVar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "@{}", self.name)?;
        match &self.kind {
            // FlowVarKind::Strong(t) => write!(f, " = {:?}", t),
            FlowVarKind::Weak(w) => write!(f, "{w:?}"),
        }
    }
}

impl FlowVar {
    pub fn name(&self) -> EcoString {
        self.name.clone()
    }

    pub fn id(&self) -> DefId {
        self.id
    }

    pub fn get_ref(&self) -> FlowType {
        FlowType::Var(Box::new((self.id, self.name.clone())))
    }

    pub fn ever_be(&self, exp: FlowType) {
        match &self.kind {
            // FlowVarKind::Strong(_t) => {}
            FlowVarKind::Weak(w) => {
                let mut w = w.write();
                w.lbs.push(exp.clone());
            }
        }
    }

    pub fn as_strong(&mut self, exp: FlowType) {
        // self.kind = FlowVarKind::Strong(value);
        match &self.kind {
            // FlowVarKind::Strong(_t) => {}
            FlowVarKind::Weak(w) => {
                let mut w = w.write();
                w.lbs.push(exp.clone());
            }
        }
    }
}

#[derive(Hash, Clone)]
pub(crate) struct FlowAt(pub Box<(FlowType, EcoString)>);

impl fmt::Debug for FlowAt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}.{}", RefDebug(&self.0 .0), self.0 .1)
    }
}

#[derive(Clone, Hash)]
pub(crate) struct FlowArgs {
    pub args: Vec<FlowType>,
    pub named: Vec<(EcoString, FlowType)>,
}
impl FlowArgs {
    pub fn start_match(&self) -> &[FlowType] {
        &self.args
    }
}

impl fmt::Debug for FlowArgs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use std::fmt::Write;

        f.write_str("&(")?;
        if let Some((first, args)) = self.args.split_first() {
            write!(f, "{first:?}")?;
            for arg in args {
                write!(f, "{arg:?}, ")?;
            }
        }
        f.write_char(')')
    }
}

#[derive(Clone, Hash)]
pub(crate) struct FlowSignature {
    pub pos: Vec<FlowType>,
    pub named: Vec<(EcoString, FlowType)>,
    pub rest: Option<FlowType>,
    pub ret: FlowType,
}

impl fmt::Debug for FlowSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("(")?;
        if let Some((first, pos)) = self.pos.split_first() {
            write!(f, "{first:?}")?;
            for p in pos {
                write!(f, ", {p:?}")?;
            }
        }
        for (name, ty) in &self.named {
            write!(f, ", {name}: {ty:?}")?;
        }
        if let Some(rest) = &self.rest {
            write!(f, ", ...: {rest:?}")?;
        }
        f.write_str(") -> ")?;
        write!(f, "{:?}", self.ret)
    }
}

#[derive(Clone, Hash)]
pub(crate) struct FlowRecord {
    pub fields: EcoVec<(EcoString, FlowType, Span)>,
}
impl FlowRecord {
    pub(crate) fn intersect_keys_enumerate<'a>(
        &'a self,
        rhs: &'a FlowRecord,
    ) -> impl Iterator<Item = (usize, usize)> + 'a {
        let mut lhs = self;
        let mut rhs = rhs;

        // size optimization
        let mut swapped = false;
        if lhs.fields.len() < rhs.fields.len() {
            swapped = true;
            std::mem::swap(&mut lhs, &mut rhs);
        }

        lhs.fields
            .iter()
            .enumerate()
            .filter_map(move |(i, (name, _, _))| {
                rhs.fields
                    .iter()
                    .position(|(name2, _, _)| name == name2)
                    .map(|j| (i, j))
            })
            .map(move |(i, j)| if swapped { (j, i) } else { (i, j) })
    }

    pub(crate) fn intersect_keys<'a>(
        &'a self,
        rhs: &'a FlowRecord,
    ) -> impl Iterator<Item = (&(EcoString, FlowType, Span), &(EcoString, FlowType, Span))> + 'a
    {
        self.intersect_keys_enumerate(rhs)
            .filter_map(move |(i, j)| {
                self.fields
                    .get(i)
                    .and_then(|lhs| rhs.fields.get(j).map(|rhs| (lhs, rhs)))
            })
    }
}

impl fmt::Debug for FlowRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("{")?;
        if let Some((first, fields)) = self.fields.split_first() {
            write!(f, "{name:?}: {ty:?}", name = first.0, ty = first.1)?;
            for (name, ty, _) in fields {
                write!(f, ", {name:?}: {ty:?}")?;
            }
        }
        f.write_str("}")
    }
}

fn instantiate_signature(
    f: &FlowType,
    args: Vec<FlowArgs>,
    sigs: &mut Vec<FlowSignature>,
    ty_chk: &TypeCheckInfo,
    principal: bool,
) {
    let sigs_checkpoint = sigs.len();
    check_signatures(f, sigs, ty_chk, principal);
    if sigs.len() == sigs_checkpoint {
        return;
    }
    for sig in &mut sigs[sigs_checkpoint..] {
        // consume the positional arguments
        sig.pos = if sig.pos.len() > args.len() {
            sig.pos.split_off(args.len())
        } else {
            Vec::new()
        };
    }
}

fn check_signatures(
    ty: &FlowType,
    res: &mut Vec<FlowSignature>,
    ty_chk: &TypeCheckInfo,
    principal: bool,
) {
    match ty {
        FlowType::Func(s) => res.push(*s.clone()),
        FlowType::With(w) => {
            instantiate_signature(&w.0, w.1.clone(), res, ty_chk, principal);
        }
        FlowType::Union(u) => {
            for ty in u.iter() {
                check_signatures(ty, res, ty_chk, principal);
            }
        }
        FlowType::Var(u) => {
            let var = ty_chk.vars.get(&u.0);
            if let Some(var) = var {
                let FlowVarKind::Weak(w) = &var.kind;
                let w = w.read();
                for lb in &w.ubs {
                    check_signatures(lb, res, ty_chk, principal);
                }
                if !principal {
                    for ub in &w.lbs {
                        check_signatures(ub, res, ty_chk, principal);
                    }
                }
            }
        }
        _ => {}
    }
}
