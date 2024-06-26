//! Top-level evaluation of a source file.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

use ecow::{EcoString, EcoVec};
use once_cell::sync::Lazy;
use parking_lot::{Mutex, RwLock};
use reflexo::{hash::hash128, vector::ir::DefId};
use typst::{
    foundations::{Func, Value},
    syntax::{
        ast::{self, AstNode},
        LinkedNode, Source, Span, SyntaxKind,
    },
};

use crate::{analysis::analyze_dyn_signature, AnalysisContext};

use super::{resolve_global_value, DefUseInfo, IdentRef};

mod def;
pub(crate) use def::*;
mod builtin;
pub(crate) use builtin::*;
mod literal_flow;
pub(crate) use literal_flow::*;

/// Type checking at the source unit level.
pub(crate) fn type_check(ctx: &mut AnalysisContext, source: Source) -> Option<Arc<TypeCheckInfo>> {
    let mut info = TypeCheckInfo::default();

    // Retrieve def-use information for the source.
    let def_use_info = ctx.def_use(source.clone())?;

    let mut type_checker = TypeChecker {
        ctx,
        source: source.clone(),
        def_use_info,
        info: &mut info,
        mode: InterpretMode::Markup,
    };
    let lnk = LinkedNode::new(source.root());

    let type_check_start = std::time::Instant::now();
    type_checker.check(lnk);
    let elapsed = type_check_start.elapsed();
    log::info!("Type checking on {:?} took {elapsed:?}", source.id());

    // todo: cross-file unit type checking
    let _ = type_checker.source;

    Some(Arc::new(info))
}

#[derive(Default)]
pub(crate) struct TypeCheckInfo {
    pub vars: HashMap<DefId, FlowVar>,
    pub mapping: HashMap<Span, FlowType>,

    cano_cache: Mutex<TypeCanoStore>,
}

impl TypeCheckInfo {
    pub fn simplify(&self, ty: FlowType, principal: bool) -> FlowType {
        let mut c = self.cano_cache.lock();
        let c = &mut *c;

        c.cano_local_cache.clear();
        c.positives.clear();
        c.negatives.clear();

        let mut worker = TypeSimplifier {
            principal,
            vars: &self.vars,
            cano_cache: &mut c.cano_cache,
            cano_local_cache: &mut c.cano_local_cache,

            positives: &mut c.positives,
            negatives: &mut c.negatives,
        };

        worker.simplify(ty, principal)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterpretMode {
    Markup,
    Code,
    Math,
}

struct TypeChecker<'a, 'w> {
    ctx: &'a mut AnalysisContext<'w>,
    source: Source,
    def_use_info: Arc<DefUseInfo>,

    info: &'a mut TypeCheckInfo,
    mode: InterpretMode,
}

impl<'a, 'w> TypeChecker<'a, 'w> {
    fn check(&mut self, root: LinkedNode) -> FlowType {
        let should_record = matches!(root.kind(), SyntaxKind::FuncCall).then(|| root.span());
        let w = self.check_inner(root).unwrap_or(FlowType::Undef);

        if let Some(s) = should_record {
            self.info.mapping.insert(s, w.clone());
        }

        w
    }

    fn check_inner(&mut self, root: LinkedNode) -> Option<FlowType> {
        Some(match root.kind() {
            SyntaxKind::Markup => return self.check_in_mode(root, InterpretMode::Markup),
            SyntaxKind::Math => return self.check_in_mode(root, InterpretMode::Math),
            SyntaxKind::Code => return self.check_in_mode(root, InterpretMode::Code),
            SyntaxKind::CodeBlock => return self.check_in_mode(root, InterpretMode::Code),
            SyntaxKind::ContentBlock => return self.check_in_mode(root, InterpretMode::Markup),

            // todo: space effect
            SyntaxKind::Space => FlowType::None,
            SyntaxKind::Parbreak => FlowType::None,

            SyntaxKind::Text => FlowType::Content,
            SyntaxKind::Linebreak => FlowType::Content,
            SyntaxKind::Escape => FlowType::Content,
            SyntaxKind::Shorthand => FlowType::Content,
            SyntaxKind::SmartQuote => FlowType::Content,
            SyntaxKind::Raw => FlowType::Content,
            SyntaxKind::RawLang => FlowType::Content,
            SyntaxKind::RawDelim => FlowType::Content,
            SyntaxKind::RawTrimmed => FlowType::Content,
            SyntaxKind::Link => FlowType::Content,
            SyntaxKind::Label => FlowType::Content,
            SyntaxKind::Ref => FlowType::Content,
            SyntaxKind::RefMarker => FlowType::Content,
            SyntaxKind::HeadingMarker => FlowType::Content,
            SyntaxKind::EnumMarker => FlowType::Content,
            SyntaxKind::ListMarker => FlowType::Content,
            SyntaxKind::TermMarker => FlowType::Content,
            SyntaxKind::MathAlignPoint => FlowType::Content,
            SyntaxKind::MathPrimes => FlowType::Content,

            SyntaxKind::Strong => return self.check_children(root),
            SyntaxKind::Emph => return self.check_children(root),
            SyntaxKind::Heading => return self.check_children(root),
            SyntaxKind::ListItem => return self.check_children(root),
            SyntaxKind::EnumItem => return self.check_children(root),
            SyntaxKind::TermItem => return self.check_children(root),
            SyntaxKind::Equation => return self.check_children(root),
            SyntaxKind::MathDelimited => return self.check_children(root),
            SyntaxKind::MathAttach => return self.check_children(root),
            SyntaxKind::MathFrac => return self.check_children(root),
            SyntaxKind::MathRoot => return self.check_children(root),

            SyntaxKind::LoopBreak => FlowType::None,
            SyntaxKind::LoopContinue => FlowType::None,
            SyntaxKind::FuncReturn => FlowType::None,
            SyntaxKind::LineComment => FlowType::None,
            SyntaxKind::BlockComment => FlowType::None,
            SyntaxKind::Error => FlowType::None,
            SyntaxKind::Eof => FlowType::None,

            SyntaxKind::None => FlowType::None,
            SyntaxKind::Auto => FlowType::Auto,
            SyntaxKind::Break => FlowType::FlowNone,
            SyntaxKind::Continue => FlowType::FlowNone,
            SyntaxKind::Return => FlowType::FlowNone,
            SyntaxKind::Ident => return self.check_ident(root, InterpretMode::Code),
            SyntaxKind::MathIdent => return self.check_ident(root, InterpretMode::Math),
            SyntaxKind::Bool
            | SyntaxKind::Int
            | SyntaxKind::Float
            | SyntaxKind::Numeric
            | SyntaxKind::Str => {
                return self
                    .ctx
                    .mini_eval(root.cast()?)
                    .map(|v| (FlowType::Value(Box::new((v, root.span())))))
            }
            SyntaxKind::Parenthesized => return self.check_children(root),
            SyntaxKind::Array => return self.check_array(root),
            SyntaxKind::Dict => return self.check_dict(root),
            SyntaxKind::Unary => return self.check_unary(root),
            SyntaxKind::Binary => return self.check_binary(root),
            SyntaxKind::FieldAccess => return self.check_field_access(root),
            SyntaxKind::FuncCall => return self.check_func_call(root),
            SyntaxKind::Args => return self.check_args(root),
            SyntaxKind::Closure => return self.check_closure(root),
            SyntaxKind::LetBinding => return self.check_let(root),
            SyntaxKind::SetRule => return self.check_set(root),
            SyntaxKind::ShowRule => return self.check_show(root),
            SyntaxKind::Contextual => return self.check_contextual(root),
            SyntaxKind::Conditional => return self.check_conditional(root),
            SyntaxKind::WhileLoop => return self.check_while_loop(root),
            SyntaxKind::ForLoop => return self.check_for_loop(root),
            SyntaxKind::ModuleImport => return self.check_module_import(root),
            SyntaxKind::ModuleInclude => return self.check_module_include(root),
            SyntaxKind::Destructuring => return self.check_destructuring(root),
            SyntaxKind::DestructAssignment => return self.check_destruct_assign(root),

            // Rest all are clauses
            SyntaxKind::Named => FlowType::Clause,
            SyntaxKind::Keyed => FlowType::Clause,
            SyntaxKind::Spread => FlowType::Clause,
            SyntaxKind::Params => FlowType::Clause,
            SyntaxKind::ImportItems => FlowType::Clause,
            SyntaxKind::RenamedImportItem => FlowType::Clause,
            SyntaxKind::Hash => FlowType::Clause,
            SyntaxKind::LeftBrace => FlowType::Clause,
            SyntaxKind::RightBrace => FlowType::Clause,
            SyntaxKind::LeftBracket => FlowType::Clause,
            SyntaxKind::RightBracket => FlowType::Clause,
            SyntaxKind::LeftParen => FlowType::Clause,
            SyntaxKind::RightParen => FlowType::Clause,
            SyntaxKind::Comma => FlowType::Clause,
            SyntaxKind::Semicolon => FlowType::Clause,
            SyntaxKind::Colon => FlowType::Clause,
            SyntaxKind::Star => FlowType::Clause,
            SyntaxKind::Underscore => FlowType::Clause,
            SyntaxKind::Dollar => FlowType::Clause,
            SyntaxKind::Plus => FlowType::Clause,
            SyntaxKind::Minus => FlowType::Clause,
            SyntaxKind::Slash => FlowType::Clause,
            SyntaxKind::Hat => FlowType::Clause,
            SyntaxKind::Prime => FlowType::Clause,
            SyntaxKind::Dot => FlowType::Clause,
            SyntaxKind::Eq => FlowType::Clause,
            SyntaxKind::EqEq => FlowType::Clause,
            SyntaxKind::ExclEq => FlowType::Clause,
            SyntaxKind::Lt => FlowType::Clause,
            SyntaxKind::LtEq => FlowType::Clause,
            SyntaxKind::Gt => FlowType::Clause,
            SyntaxKind::GtEq => FlowType::Clause,
            SyntaxKind::PlusEq => FlowType::Clause,
            SyntaxKind::HyphEq => FlowType::Clause,
            SyntaxKind::StarEq => FlowType::Clause,
            SyntaxKind::SlashEq => FlowType::Clause,
            SyntaxKind::Dots => FlowType::Clause,
            SyntaxKind::Arrow => FlowType::Clause,
            SyntaxKind::Root => FlowType::Clause,
            SyntaxKind::Not => FlowType::Clause,
            SyntaxKind::And => FlowType::Clause,
            SyntaxKind::Or => FlowType::Clause,
            SyntaxKind::Let => FlowType::Clause,
            SyntaxKind::Set => FlowType::Clause,
            SyntaxKind::Show => FlowType::Clause,
            SyntaxKind::Context => FlowType::Clause,
            SyntaxKind::If => FlowType::Clause,
            SyntaxKind::Else => FlowType::Clause,
            SyntaxKind::For => FlowType::Clause,
            SyntaxKind::In => FlowType::Clause,
            SyntaxKind::While => FlowType::Clause,
            SyntaxKind::Import => FlowType::Clause,
            SyntaxKind::Include => FlowType::Clause,
            SyntaxKind::As => FlowType::Clause,
        })
    }

    fn check_in_mode(&mut self, root: LinkedNode, into_mode: InterpretMode) -> Option<FlowType> {
        let mode = self.mode;
        self.mode = into_mode;
        let res = self.check_children(root);
        self.mode = mode;
        res
    }

    fn check_children(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let mut joiner = Joiner::default();

        for child in root.children() {
            joiner.join(self.check(child));
        }
        Some(joiner.finalize())
    }

    fn check_ident(&mut self, root: LinkedNode<'_>, mode: InterpretMode) -> Option<FlowType> {
        let ident: ast::Ident = root.cast()?;
        let ident_ref = IdentRef {
            name: ident.get().to_string(),
            range: root.range(),
        };

        let Some(def_id) = self.def_use_info.get_ref(&ident_ref) else {
            let s = root.span();
            let v = resolve_global_value(self.ctx, root, mode == InterpretMode::Math)?;
            return Some(FlowType::Value(Box::new((v, s))));
        };
        let var = self.info.vars.get(&def_id)?.clone();

        Some(var.get_ref())
    }

    fn check_array(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let _arr: ast::Array = root.cast()?;

        let mut elements = EcoVec::new();

        for elem in root.children() {
            let ty = self.check(elem);
            if matches!(ty, FlowType::Clause) {
                continue;
            }
            elements.push(ty);
        }

        Some(FlowType::Tuple(elements))
    }

    fn check_dict(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let dict: ast::Dict = root.cast()?;

        let mut fields = EcoVec::new();

        for field in dict.items() {
            match field {
                ast::DictItem::Named(n) => {
                    let name = n.name().get().clone();
                    let value = self.check_expr_in(n.expr().span(), root.clone());
                    fields.push((name, value, n.span()));
                }
                ast::DictItem::Keyed(k) => {
                    let key = self.ctx.const_eval(k.key());
                    if let Some(Value::Str(key)) = key {
                        let value = self.check_expr_in(k.expr().span(), root.clone());
                        fields.push((key.into(), value, k.span()));
                    }
                }
                // todo: var dict union
                ast::DictItem::Spread(_s) => {}
            }
        }

        Some(FlowType::Dict(FlowRecord { fields }))
    }

    fn check_unary(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let unary: ast::Unary = root.cast()?;

        if let Some(constant) = self.ctx.mini_eval(ast::Expr::Unary(unary)) {
            return Some(FlowType::Value(Box::new((constant, root.span()))));
        }

        let op = unary.op();

        let expr = Box::new(self.check_expr_in(unary.expr().span(), root));
        let ty = match op {
            ast::UnOp::Pos => FlowUnaryType::Pos(expr),
            ast::UnOp::Neg => FlowUnaryType::Neg(expr),
            ast::UnOp::Not => FlowUnaryType::Not(expr),
        };

        Some(FlowType::Unary(ty))
    }

    fn check_binary(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let binary: ast::Binary = root.cast()?;

        if let Some(constant) = self.ctx.mini_eval(ast::Expr::Binary(binary)) {
            return Some(FlowType::Value(Box::new((constant, root.span()))));
        }

        let op = binary.op();
        let lhs_span = binary.lhs().span();
        let lhs = self.check_expr_in(lhs_span, root.clone());
        let rhs_span = binary.rhs().span();
        let rhs = self.check_expr_in(rhs_span, root);

        match op {
            ast::BinOp::Add | ast::BinOp::Sub | ast::BinOp::Mul | ast::BinOp::Div => {}
            ast::BinOp::Eq | ast::BinOp::Neq | ast::BinOp::Leq | ast::BinOp::Geq => {
                self.check_comparable(&lhs, &rhs);
                self.possible_ever_be(&lhs, &rhs);
                self.possible_ever_be(&rhs, &lhs);
            }
            ast::BinOp::Lt | ast::BinOp::Gt => {
                self.check_comparable(&lhs, &rhs);
            }
            ast::BinOp::And | ast::BinOp::Or => {
                self.constrain(&lhs, &FlowType::Boolean(None));
                self.constrain(&rhs, &FlowType::Boolean(None));
            }
            ast::BinOp::NotIn | ast::BinOp::In => {
                self.check_containing(&rhs, &lhs, op == ast::BinOp::In);
            }
            ast::BinOp::Assign => {
                self.check_assignable(&lhs, &rhs);
                self.possible_ever_be(&lhs, &rhs);
            }
            ast::BinOp::AddAssign
            | ast::BinOp::SubAssign
            | ast::BinOp::MulAssign
            | ast::BinOp::DivAssign => {
                self.check_assignable(&lhs, &rhs);
            }
        }

        let res = FlowType::Binary(FlowBinaryType {
            op,
            operands: Box::new((lhs, rhs)),
        });

        Some(res)
    }

    fn check_field_access(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let field_access: ast::FieldAccess = root.cast()?;

        let obj = self.check_expr_in(field_access.target().span(), root.clone());
        let field = field_access.field().get().clone();

        Some(FlowType::At(FlowAt(Box::new((obj, field)))))
    }

    fn check_func_call(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let func_call: ast::FuncCall = root.cast()?;

        let args = self.check_expr_in(func_call.args().span(), root.clone());
        let callee = self.check_expr_in(func_call.callee().span(), root.clone());
        let mut candidates = Vec::with_capacity(1);

        log::debug!("func_call: {callee:?} with {args:?}");

        if let FlowType::Args(args) = args {
            self.check_apply(callee, &args, &func_call.args(), &mut candidates)?;
        }

        if candidates.len() == 1 {
            return Some(candidates[0].clone());
        }

        if candidates.is_empty() {
            return Some(FlowType::Any);
        }

        Some(FlowType::Union(Box::new(candidates)))
    }

    fn check_args(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let args: ast::Args = root.cast()?;

        let mut args_res = Vec::new();
        let mut named = vec![];

        for arg in args.items() {
            match arg {
                ast::Arg::Pos(e) => {
                    args_res.push(self.check_expr_in(e.span(), root.clone()));
                }
                ast::Arg::Named(n) => {
                    let name = n.name().get().clone();
                    let value = self.check_expr_in(n.expr().span(), root.clone());
                    named.push((name, value));
                }
                // todo
                ast::Arg::Spread(_w) => {}
            }
        }

        Some(FlowType::Args(Box::new(FlowArgs {
            args: args_res,
            named,
        })))
    }

    fn check_closure(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let closure: ast::Closure = root.cast()?;

        // let _params = self.check_expr_in(closure.params().span(), root.clone());

        let mut pos = vec![];
        let mut named = BTreeMap::new();
        let mut rest = None;

        for param in closure.params().children() {
            match param {
                ast::Param::Pos(pattern) => {
                    pos.push(self.check_pattern(pattern, FlowType::Any, root.clone()));
                }
                ast::Param::Named(e) => {
                    let exp = self.check_expr_in(e.expr().span(), root.clone());
                    let v = self.get_var(e.name().span(), to_ident_ref(&root, e.name())?)?;
                    v.ever_be(exp);
                    named.insert(e.name().get().clone(), v.get_ref());
                }
                ast::Param::Spread(a) => {
                    if let Some(e) = a.sink_ident() {
                        let exp = FlowType::Builtin(FlowBuiltinType::Args);
                        let v = self.get_var(e.span(), to_ident_ref(&root, e)?)?;
                        v.ever_be(exp);
                        rest = Some(v.get_ref());
                    }
                    // todo: ..(args)
                }
            }
        }

        let body = self.check_expr_in(closure.body().span(), root);

        Some(FlowType::Func(Box::new(FlowSignature {
            pos,
            named: named.into_iter().collect(),
            rest,
            ret: body,
        })))
    }

    fn check_let(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let let_binding: ast::LetBinding = root.cast()?;

        match let_binding.kind() {
            ast::LetBindingKind::Closure(c) => {
                // let _name = let_binding.name().get().to_string();
                let value = let_binding
                    .init()
                    .map(|init| self.check_expr_in(init.span(), root.clone()))
                    .unwrap_or_else(|| FlowType::Infer);

                let v = self.get_var(c.span(), to_ident_ref(&root, c)?)?;
                v.as_strong(value);
                // todo lbs is the lexical signature.
            }
            ast::LetBindingKind::Normal(pattern) => {
                // let _name = let_binding.name().get().to_string();
                let value = let_binding
                    .init()
                    .map(|init| self.check_expr_in(init.span(), root.clone()))
                    .unwrap_or_else(|| FlowType::Infer);

                self.check_pattern(pattern, value, root.clone());
            }
        }

        Some(FlowType::Any)
    }

    // todo: merge with func call, and regard difference (may be here)
    fn check_set(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let set_rule: ast::SetRule = root.cast()?;

        let callee = self.check_expr_in(set_rule.target().span(), root.clone());
        let args = self.check_expr_in(set_rule.args().span(), root.clone());
        let _cond = set_rule
            .condition()
            .map(|cond| self.check_expr_in(cond.span(), root.clone()));
        let mut candidates = Vec::with_capacity(1);

        log::debug!("set rule: {callee:?} with {args:?}");

        if let FlowType::Args(args) = args {
            self.check_apply(callee, &args, &set_rule.args(), &mut candidates)?;
        }

        if candidates.len() == 1 {
            return Some(candidates[0].clone());
        }

        if candidates.is_empty() {
            return Some(FlowType::Any);
        }

        Some(FlowType::Union(Box::new(candidates)))
    }

    fn check_show(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let show_rule: ast::ShowRule = root.cast()?;

        let _selector = show_rule
            .selector()
            .map(|sel| self.check_expr_in(sel.span(), root.clone()));
        let t = show_rule.transform();
        // todo: infer it type by selector
        let _transform = self.check_expr_in(t.span(), root.clone());

        Some(FlowType::Any)
    }

    // currently we do nothing on contextual
    fn check_contextual(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let contextual: ast::Contextual = root.cast()?;

        let body = self.check_expr_in(contextual.body().span(), root);

        Some(FlowType::Unary(FlowUnaryType::Context(Box::new(body))))
    }

    fn check_conditional(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let conditional: ast::Conditional = root.cast()?;

        let cond = self.check_expr_in(conditional.condition().span(), root.clone());
        let then = self.check_expr_in(conditional.if_body().span(), root.clone());
        let else_ = conditional
            .else_body()
            .map(|else_body| self.check_expr_in(else_body.span(), root.clone()))
            .unwrap_or(FlowType::None);

        Some(FlowType::If(Box::new(FlowIfType { cond, then, else_ })))
    }

    fn check_while_loop(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let while_loop: ast::WhileLoop = root.cast()?;

        let _cond = self.check_expr_in(while_loop.condition().span(), root.clone());
        let _body = self.check_expr_in(while_loop.body().span(), root);

        Some(FlowType::Any)
    }

    fn check_for_loop(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let for_loop: ast::ForLoop = root.cast()?;

        let _iter = self.check_expr_in(for_loop.iterable().span(), root.clone());
        let _pattern = self.check_expr_in(for_loop.pattern().span(), root.clone());
        let _body = self.check_expr_in(for_loop.body().span(), root);

        Some(FlowType::Any)
    }

    fn check_module_import(&mut self, root: LinkedNode<'_>) -> Option<FlowType> {
        let _module_import: ast::ModuleImport = root.cast()?;

        // check all import items

        Some(FlowType::None)
    }

    fn check_module_include(&mut self, _root: LinkedNode<'_>) -> Option<FlowType> {
        Some(FlowType::Content)
    }

    fn check_destructuring(&mut self, _root: LinkedNode<'_>) -> Option<FlowType> {
        Some(FlowType::Any)
    }

    fn check_destruct_assign(&mut self, _root: LinkedNode<'_>) -> Option<FlowType> {
        Some(FlowType::None)
    }

    fn check_expr_in(&mut self, span: Span, root: LinkedNode<'_>) -> FlowType {
        root.find(span)
            .map(|node| self.check(node))
            .unwrap_or(FlowType::Undef)
    }

    fn get_var(&mut self, s: Span, r: IdentRef) -> Option<&mut FlowVar> {
        let def_id = self
            .def_use_info
            .get_ref(&r)
            .or_else(|| Some(self.def_use_info.get_def(s.id()?, &r)?.0))?;

        let var = self.info.vars.entry(def_id).or_insert_with(|| {
            // let store = FlowVarStore {
            //     name: r.name.into(),
            //     id: def_id,
            //     lbs: Vec::new(),
            //     ubs: Vec::new(),
            // };
            // FlowVar(Arc::new(RwLock::new(store)))
            FlowVar {
                name: r.name.into(),
                id: def_id,
                kind: FlowVarKind::Weak(Arc::new(RwLock::new(FlowVarStore {
                    lbs: Vec::new(),
                    ubs: Vec::new(),
                }))),
                // kind: FlowVarKind::Strong(FlowType::Any),
            }
        });

        self.info.mapping.insert(s, var.get_ref());
        Some(var)
    }

    fn check_pattern(
        &mut self,
        pattern: ast::Pattern<'_>,
        value: FlowType,
        root: LinkedNode<'_>,
    ) -> FlowType {
        self.check_pattern_(pattern, value, root)
            .unwrap_or(FlowType::Undef)
    }

    fn check_pattern_(
        &mut self,
        pattern: ast::Pattern<'_>,
        value: FlowType,
        root: LinkedNode<'_>,
    ) -> Option<FlowType> {
        Some(match pattern {
            ast::Pattern::Normal(ast::Expr::Ident(ident)) => {
                let v = self.get_var(ident.span(), to_ident_ref(&root, ident)?)?;
                v.ever_be(value);
                v.get_ref()
            }
            ast::Pattern::Normal(_) => FlowType::Any,
            ast::Pattern::Placeholder(_) => FlowType::Any,
            ast::Pattern::Parenthesized(exp) => self.check_pattern(exp.pattern(), value, root),
            // todo: pattern
            ast::Pattern::Destructuring(_destruct) => FlowType::Any,
        })
    }

    fn check_apply(
        &mut self,
        callee: FlowType,
        args: &FlowArgs,
        syntax_args: &ast::Args,
        candidates: &mut Vec<FlowType>,
    ) -> Option<()> {
        // log::debug!("check func callee {callee:?}");

        match &callee {
            FlowType::Var(v) => {
                let w = self.info.vars.get(&v.0).cloned()?;
                match &w.kind {
                    FlowVarKind::Weak(w) => {
                        let w = w.read();
                        for lb in w.lbs.iter() {
                            self.check_apply(lb.clone(), args, syntax_args, candidates)?;
                        }
                        for ub in w.ubs.iter() {
                            self.check_apply(ub.clone(), args, syntax_args, candidates)?;
                        }
                    }
                }
            }
            FlowType::Func(v) => {
                let f = v.as_ref();
                let mut pos = f.pos.iter();
                // let mut named = f.named.clone();
                // let mut rest = f.rest.clone();

                for pos_in in args.start_match() {
                    let pos_ty = pos.next().unwrap_or(&FlowType::Any);
                    self.constrain(pos_in, pos_ty);
                }

                for (name, named_in) in &args.named {
                    let named_ty = f.named.iter().find(|(n, _)| n == name).map(|(_, ty)| ty);
                    if let Some(named_ty) = named_ty {
                        self.constrain(named_in, named_ty);
                    }
                }

                // log::debug!("check applied {v:?}");

                candidates.push(f.ret.clone());
            }
            FlowType::Dict(_v) => {}
            FlowType::Tuple(_v) => {}
            FlowType::Array(_v) => {}
            // todo: with
            FlowType::With(_e) => {}
            FlowType::Args(_e) => {}
            FlowType::Union(_e) => {}
            FlowType::Let(_) => {}
            FlowType::Value(f) => {
                if let Value::Func(f) = &f.0 {
                    self.check_apply_runtime(f, args, syntax_args, candidates);
                }
            }
            FlowType::ValueDoc(f) => {
                if let Value::Func(f) = &f.0 {
                    self.check_apply_runtime(f, args, syntax_args, candidates);
                }
            }

            FlowType::Clause => {}
            FlowType::Undef => {}
            FlowType::Content => {}
            FlowType::Any => {}
            FlowType::None => {}
            FlowType::Infer => {}
            FlowType::FlowNone => {}
            FlowType::Auto => {}
            FlowType::Builtin(_) => {}
            FlowType::Boolean(_) => {}
            FlowType::At(e) => {
                let primary_type = self.check_primary_type(e.0 .0.clone());
                self.check_apply_method(primary_type, e.0 .1.clone(), args, candidates);
            }
            FlowType::Unary(_) => {}
            FlowType::Binary(_) => {}
            FlowType::If(_) => {}
            FlowType::Element(_elem) => {}
        }

        Some(())
    }

    fn constrain(&mut self, lhs: &FlowType, rhs: &FlowType) {
        static FLOW_STROKE_DICT_TYPE: Lazy<FlowType> =
            Lazy::new(|| FlowType::Dict(FLOW_STROKE_DICT.clone()));
        static FLOW_MARGIN_DICT_TYPE: Lazy<FlowType> =
            Lazy::new(|| FlowType::Dict(FLOW_MARGIN_DICT.clone()));
        static FLOW_INSET_DICT_TYPE: Lazy<FlowType> =
            Lazy::new(|| FlowType::Dict(FLOW_INSET_DICT.clone()));
        static FLOW_OUTSET_DICT_TYPE: Lazy<FlowType> =
            Lazy::new(|| FlowType::Dict(FLOW_OUTSET_DICT.clone()));
        static FLOW_RADIUS_DICT_TYPE: Lazy<FlowType> =
            Lazy::new(|| FlowType::Dict(FLOW_RADIUS_DICT.clone()));

        match (lhs, rhs) {
            (FlowType::Var(v), FlowType::Var(w)) => {
                if v.0 .0 == w.0 .0 {
                    return;
                }

                // todo: merge

                let _ = v.0 .0;
                let _ = w.0 .0;
            }
            (FlowType::Var(v), rhs) => {
                log::debug!("constrain var {v:?} ⪯ {rhs:?}");
                let w = self.info.vars.get_mut(&v.0).unwrap();
                match &w.kind {
                    FlowVarKind::Weak(w) => {
                        let mut w = w.write();
                        w.ubs.push(rhs.clone());
                    }
                }
            }
            (lhs, FlowType::Var(v)) => {
                log::debug!("constrain var {lhs:?} ⪯ {v:?}");
                let v = self.info.vars.get(&v.0).unwrap();
                match &v.kind {
                    FlowVarKind::Weak(v) => {
                        let mut v = v.write();
                        v.lbs.push(lhs.clone());
                    }
                }
            }
            (FlowType::Union(v), rhs) => {
                for e in v.iter() {
                    self.constrain(e, rhs);
                }
            }
            (lhs, FlowType::Union(v)) => {
                for e in v.iter() {
                    self.constrain(lhs, e);
                }
            }
            (lhs, FlowType::Builtin(FlowBuiltinType::Stroke)) => {
                // empty array is also a constructing dict but we can safely ignore it during
                // type checking, since no fields are added yet.
                if lhs.is_dict() {
                    self.constrain(lhs, &FLOW_STROKE_DICT_TYPE);
                }
            }
            (FlowType::Builtin(FlowBuiltinType::Stroke), rhs) => {
                if rhs.is_dict() {
                    self.constrain(&FLOW_STROKE_DICT_TYPE, rhs);
                }
            }
            (lhs, FlowType::Builtin(FlowBuiltinType::Margin)) => {
                if lhs.is_dict() {
                    self.constrain(lhs, &FLOW_MARGIN_DICT_TYPE);
                }
            }
            (FlowType::Builtin(FlowBuiltinType::Margin), rhs) => {
                if rhs.is_dict() {
                    self.constrain(&FLOW_MARGIN_DICT_TYPE, rhs);
                }
            }
            (lhs, FlowType::Builtin(FlowBuiltinType::Inset)) => {
                if lhs.is_dict() {
                    self.constrain(lhs, &FLOW_INSET_DICT_TYPE);
                }
            }
            (FlowType::Builtin(FlowBuiltinType::Inset), rhs) => {
                if rhs.is_dict() {
                    self.constrain(&FLOW_INSET_DICT_TYPE, rhs);
                }
            }
            (lhs, FlowType::Builtin(FlowBuiltinType::Outset)) => {
                if lhs.is_dict() {
                    self.constrain(lhs, &FLOW_OUTSET_DICT_TYPE);
                }
            }
            (FlowType::Builtin(FlowBuiltinType::Outset), rhs) => {
                if rhs.is_dict() {
                    self.constrain(&FLOW_OUTSET_DICT_TYPE, rhs);
                }
            }
            (lhs, FlowType::Builtin(FlowBuiltinType::Radius)) => {
                if lhs.is_dict() {
                    self.constrain(lhs, &FLOW_RADIUS_DICT_TYPE);
                }
            }
            (FlowType::Builtin(FlowBuiltinType::Radius), rhs) => {
                if rhs.is_dict() {
                    self.constrain(&FLOW_RADIUS_DICT_TYPE, rhs);
                }
            }
            (FlowType::Dict(lhs), FlowType::Dict(rhs)) => {
                for ((key, lhs, sl), (_, rhs, sr)) in lhs.intersect_keys(rhs) {
                    log::debug!("constrain record item {key} {lhs:?} ⪯ {rhs:?}");
                    self.constrain(lhs, rhs);
                    if !sl.is_detached() {
                        // todo: intersect/union
                        self.info.mapping.entry(*sl).or_insert(rhs.clone());
                    }
                    if !sr.is_detached() {
                        // todo: intersect/union
                        self.info.mapping.entry(*sr).or_insert(lhs.clone());
                    }
                }
            }
            (FlowType::Value(lhs), rhs) => {
                log::debug!("constrain value {lhs:?} ⪯ {rhs:?}");
                if !lhs.1.is_detached() {
                    // todo: intersect/union
                    self.info.mapping.entry(lhs.1).or_insert(rhs.clone());
                }
            }
            (lhs, FlowType::Value(rhs)) => {
                log::debug!("constrain value {lhs:?} ⪯ {rhs:?}");
                if !rhs.1.is_detached() {
                    // todo: intersect/union
                    self.info.mapping.entry(rhs.1).or_insert(lhs.clone());
                }
            }
            _ => {
                log::debug!("constrain {lhs:?} ⪯ {rhs:?}");
            }
        }
    }

    fn check_primary_type(&self, e: FlowType) -> FlowType {
        match &e {
            FlowType::Var(v) => {
                let w = self.info.vars.get(&v.0).unwrap();
                match &w.kind {
                    FlowVarKind::Weak(w) => {
                        let w = w.read();
                        if !w.ubs.is_empty() {
                            return w.ubs[0].clone();
                        }
                        if !w.lbs.is_empty() {
                            return w.lbs[0].clone();
                        }
                        FlowType::Any
                    }
                }
            }
            FlowType::Func(..) => e,
            FlowType::Dict(..) => e,
            FlowType::With(..) => e,
            FlowType::Args(..) => e,
            FlowType::Union(..) => e,
            FlowType::Let(_) => e,
            FlowType::Value(..) => e,
            FlowType::ValueDoc(..) => e,

            FlowType::Tuple(..) => e,
            FlowType::Array(..) => e,
            FlowType::Clause => e,
            FlowType::Undef => e,
            FlowType::Content => e,
            FlowType::Any => e,
            FlowType::None => e,
            FlowType::Infer => e,
            FlowType::FlowNone => e,
            FlowType::Auto => e,
            FlowType::Builtin(_) => e,
            FlowType::At(e) => self.check_primary_type(e.0 .0.clone()),
            FlowType::Unary(_) => e,
            FlowType::Binary(_) => e,
            FlowType::Boolean(_) => e,
            FlowType::If(_) => e,
            FlowType::Element(_) => e,
        }
    }

    fn check_apply_method(
        &mut self,
        primary_type: FlowType,
        method_name: EcoString,
        args: &FlowArgs,
        _candidates: &mut Vec<FlowType>,
    ) -> Option<()> {
        log::debug!("check method at {method_name:?} on {primary_type:?}");
        match primary_type {
            FlowType::Func(v) => match method_name.as_str() {
                // todo: process where specially
                "with" | "where" => {
                    // log::debug!("check method at args: {v:?}.with({args:?})");

                    let f = v.as_ref();
                    let mut pos = f.pos.iter();
                    // let mut named = f.named.clone();
                    // let mut rest = f.rest.clone();

                    for pos_in in args.start_match() {
                        let pos_ty = pos.next().unwrap_or(&FlowType::Any);
                        self.constrain(pos_in, pos_ty);
                    }

                    for (name, named_in) in &args.named {
                        let named_ty = f.named.iter().find(|(n, _)| n == name).map(|(_, ty)| ty);
                        if let Some(named_ty) = named_ty {
                            self.constrain(named_in, named_ty);
                        }
                    }

                    _candidates.push(self.partial_apply(f, args));
                }
                _ => {}
            },
            FlowType::Array(..) => {}
            FlowType::Dict(..) => {}
            _ => {}
        }

        Some(())
    }

    fn check_apply_runtime(
        &mut self,
        f: &Func,
        args: &FlowArgs,
        syntax_args: &ast::Args,
        candidates: &mut Vec<FlowType>,
    ) -> Option<()> {
        let sig = analyze_dyn_signature(self.ctx, f.clone());

        log::debug!("check runtime func {f:?} at args: {args:?}");

        let mut pos = sig
            .primary()
            .pos
            .iter()
            .map(|e| e.infer_type.as_ref().unwrap_or(&FlowType::Any));
        let mut syntax_pos = syntax_args.items().filter_map(|arg| match arg {
            ast::Arg::Pos(e) => Some(e),
            _ => None,
        });

        for pos_in in args.start_match() {
            let pos_ty = pos.next().unwrap_or(&FlowType::Any);
            self.constrain(pos_in, pos_ty);
            if let Some(syntax_pos) = syntax_pos.next() {
                self.info.mapping.insert(syntax_pos.span(), pos_ty.clone());
            }
        }

        for (name, named_in) in &args.named {
            let named_ty = sig
                .primary()
                .named
                .get(name.as_ref())
                .and_then(|e| e.infer_type.as_ref());
            let syntax_named = syntax_args
                .items()
                .filter_map(|arg| match arg {
                    ast::Arg::Named(n) => Some(n),
                    _ => None,
                })
                .find(|n| n.name().get() == name.as_ref());
            if let Some(named_ty) = named_ty {
                self.constrain(named_in, named_ty);
                if let Some(syntax_named) = syntax_named {
                    self.info
                        .mapping
                        .insert(syntax_named.span(), named_ty.clone());
                    self.info
                        .mapping
                        .insert(syntax_named.expr().span(), named_ty.clone());
                }
            }
        }

        candidates.push(sig.primary().ret_ty.clone().unwrap_or(FlowType::Any));

        Some(())
    }

    fn partial_apply(&self, f: &FlowSignature, args: &FlowArgs) -> FlowType {
        FlowType::With(Box::new((
            FlowType::Func(Box::new(f.clone())),
            vec![args.clone()],
        )))
    }

    fn check_comparable(&self, lhs: &FlowType, rhs: &FlowType) {
        let _ = lhs;
        let _ = rhs;
    }

    fn check_assignable(&self, lhs: &FlowType, rhs: &FlowType) {
        let _ = lhs;
        let _ = rhs;
    }

    fn check_containing(&self, container: &FlowType, elem: &FlowType, expected_in: bool) {
        let _ = container;
        let _ = elem;
        let _ = expected_in;
    }

    fn possible_ever_be(&mut self, lhs: &FlowType, rhs: &FlowType) {
        // todo: instantiataion
        match rhs {
            FlowType::Undef
            | FlowType::Content
            | FlowType::None
            | FlowType::FlowNone
            | FlowType::Auto
            | FlowType::Element(..)
            | FlowType::Builtin(..)
            | FlowType::Value(..)
            | FlowType::Boolean(..)
            | FlowType::ValueDoc(..) => {
                self.constrain(rhs, lhs);
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct TypeCanoStore {
    cano_cache: HashMap<(u128, bool), FlowType>,
    cano_local_cache: HashMap<(DefId, bool), FlowType>,
    negatives: HashSet<DefId>,
    positives: HashSet<DefId>,
}

struct TypeSimplifier<'a, 'b> {
    principal: bool,

    vars: &'a HashMap<DefId, FlowVar>,

    cano_cache: &'b mut HashMap<(u128, bool), FlowType>,
    cano_local_cache: &'b mut HashMap<(DefId, bool), FlowType>,
    negatives: &'b mut HashSet<DefId>,
    positives: &'b mut HashSet<DefId>,
}

impl<'a, 'b> TypeSimplifier<'a, 'b> {
    fn simplify(&mut self, ty: FlowType, principal: bool) -> FlowType {
        // todo: hash safety
        let ty_key = hash128(&ty);
        if let Some(cano) = self.cano_cache.get(&(ty_key, principal)) {
            return cano.clone();
        }

        self.analyze(&ty, true);

        self.transform(&ty, true)
    }

    fn analyze(&mut self, ty: &FlowType, pol: bool) {
        match ty {
            FlowType::Var(v) => {
                let w = self.vars.get(&v.0).unwrap();
                match &w.kind {
                    FlowVarKind::Weak(w) => {
                        let w = w.read();
                        if pol {
                            self.positives.insert(v.0);
                        } else {
                            self.negatives.insert(v.0);
                        }

                        if pol {
                            for lb in w.lbs.iter() {
                                self.analyze(lb, pol);
                            }
                        } else {
                            for ub in w.ubs.iter() {
                                self.analyze(ub, pol);
                            }
                        }
                    }
                }
            }
            FlowType::Func(f) => {
                for p in &f.pos {
                    self.analyze(p, !pol);
                }
                for (_, p) in &f.named {
                    self.analyze(p, !pol);
                }
                if let Some(r) = &f.rest {
                    self.analyze(r, !pol);
                }
                self.analyze(&f.ret, pol);
            }
            FlowType::Dict(r) => {
                for (_, p, _) in &r.fields {
                    self.analyze(p, pol);
                }
            }
            FlowType::Tuple(e) => {
                for ty in e.iter() {
                    self.analyze(ty, pol);
                }
            }
            FlowType::Array(e) => {
                self.analyze(e, pol);
            }
            FlowType::With(w) => {
                self.analyze(&w.0, pol);
                for m in &w.1 {
                    for arg in m.args.iter() {
                        self.analyze(arg, pol);
                    }
                }
            }
            FlowType::Args(args) => {
                for arg in &args.args {
                    self.analyze(arg, pol);
                }
            }
            FlowType::Unary(u) => self.analyze(u.lhs(), pol),
            FlowType::Binary(b) => {
                let (lhs, rhs) = b.repr();
                self.analyze(lhs, pol);
                self.analyze(rhs, pol);
            }
            FlowType::If(i) => {
                self.analyze(&i.cond, pol);
                self.analyze(&i.then, pol);
                self.analyze(&i.else_, pol);
            }
            FlowType::Union(v) => {
                for ty in v.iter() {
                    self.analyze(ty, pol);
                }
            }
            FlowType::At(a) => {
                self.analyze(&a.0 .0, pol);
            }
            FlowType::Let(v) => {
                for lb in v.lbs.iter() {
                    self.analyze(lb, !pol);
                }
                for ub in v.ubs.iter() {
                    self.analyze(ub, pol);
                }
            }
            FlowType::Value(_v) => {}
            FlowType::ValueDoc(_v) => {}
            FlowType::Clause => {}
            FlowType::Undef => {}
            FlowType::Content => {}
            FlowType::Any => {}
            FlowType::None => {}
            FlowType::Infer => {}
            FlowType::FlowNone => {}
            FlowType::Auto => {}
            FlowType::Boolean(_) => {}
            FlowType::Builtin(_) => {}
            FlowType::Element(_) => {}
        }
    }

    fn transform(&mut self, ty: &FlowType, pol: bool) -> FlowType {
        match ty {
            FlowType::Var(v) => {
                if let Some(cano) = self.cano_local_cache.get(&(v.0, self.principal)) {
                    return cano.clone();
                }
                // todo: avoid cycle
                self.cano_local_cache
                    .insert((v.0, self.principal), FlowType::Any);

                let res = match &self.vars.get(&v.0).unwrap().kind {
                    FlowVarKind::Weak(w) => {
                        let w = w.read();

                        let mut lbs = Vec::with_capacity(w.lbs.len());
                        let mut ubs = Vec::with_capacity(w.ubs.len());

                        log::debug!(
                            "transform var [principal={}] {v:?} with {w:?}",
                            self.principal
                        );

                        if !self.principal || ((pol) && !self.negatives.contains(&v.0)) {
                            for lb in w.lbs.iter() {
                                lbs.push(self.transform(lb, pol));
                            }
                        }
                        if !self.principal || ((!pol) && !self.positives.contains(&v.0)) {
                            for ub in w.ubs.iter() {
                                ubs.push(self.transform(ub, !pol));
                            }
                        }

                        if ubs.is_empty() {
                            if lbs.len() == 1 {
                                return lbs.pop().unwrap();
                            }
                            if lbs.is_empty() {
                                return FlowType::Any;
                            }
                        }

                        FlowType::Let(Arc::new(FlowVarStore { lbs, ubs }))
                    }
                };

                self.cano_local_cache
                    .insert((v.0, self.principal), res.clone());

                res
            }
            FlowType::Func(f) => {
                let pos = f.pos.iter().map(|p| self.transform(p, !pol)).collect();
                let named = f
                    .named
                    .iter()
                    .map(|(n, p)| (n.clone(), self.transform(p, !pol)))
                    .collect();
                let rest = f.rest.as_ref().map(|r| self.transform(r, !pol));
                let ret = self.transform(&f.ret, pol);

                FlowType::Func(Box::new(FlowSignature {
                    pos,
                    named,
                    rest,
                    ret,
                }))
            }
            FlowType::Dict(f) => {
                let fields = f
                    .fields
                    .iter()
                    .map(|p| (p.0.clone(), self.transform(&p.1, !pol), p.2))
                    .collect();

                FlowType::Dict(FlowRecord { fields })
            }
            FlowType::Tuple(e) => {
                let e2 = e.iter().map(|ty| self.transform(ty, pol)).collect();

                FlowType::Tuple(e2)
            }
            FlowType::Array(e) => {
                let e2 = self.transform(e, pol);

                FlowType::Array(Box::new(e2))
            }
            FlowType::With(w) => {
                let primary = self.transform(&w.0, pol);
                FlowType::With(Box::new((primary, w.1.clone())))
            }
            FlowType::Args(args) => {
                let args_res = args.args.iter().map(|a| self.transform(a, pol)).collect();
                let named = args
                    .named
                    .iter()
                    .map(|(n, a)| (n.clone(), self.transform(a, pol)))
                    .collect();

                FlowType::Args(Box::new(FlowArgs {
                    args: args_res,
                    named,
                }))
            }
            FlowType::Unary(u) => {
                let u2 = u.clone();

                FlowType::Unary(u2)
            }
            FlowType::Binary(b) => {
                let b2 = b.clone();

                FlowType::Binary(b2)
            }
            FlowType::If(i) => {
                let i2 = i.clone();

                FlowType::If(i2)
            }
            FlowType::Union(v) => {
                let v2 = v.iter().map(|ty| self.transform(ty, pol)).collect();

                FlowType::Union(Box::new(v2))
            }
            FlowType::At(a) => {
                let a2 = a.clone();

                FlowType::At(a2)
            }
            // todo
            FlowType::Let(_) => FlowType::Any,
            FlowType::Value(v) => FlowType::Value(v.clone()),
            FlowType::ValueDoc(v) => FlowType::ValueDoc(v.clone()),
            FlowType::Element(v) => FlowType::Element(*v),
            FlowType::Clause => FlowType::Clause,
            FlowType::Undef => FlowType::Undef,
            FlowType::Content => FlowType::Content,
            FlowType::Any => FlowType::Any,
            FlowType::None => FlowType::None,
            FlowType::Infer => FlowType::Infer,
            FlowType::FlowNone => FlowType::FlowNone,
            FlowType::Auto => FlowType::Auto,
            FlowType::Boolean(b) => FlowType::Boolean(*b),
            FlowType::Builtin(b) => FlowType::Builtin(b.clone()),
        }
    }
}

fn to_ident_ref(root: &LinkedNode, c: ast::Ident) -> Option<IdentRef> {
    Some(IdentRef {
        name: c.get().to_string(),
        range: root.find(c.span())?.range(),
    })
}

struct Joiner {
    break_or_continue_or_return: bool,
    definite: FlowType,
    possibles: Vec<FlowType>,
}
impl Joiner {
    fn finalize(self) -> FlowType {
        if self.possibles.is_empty() {
            return self.definite;
        }

        // let mut definite = self.definite.clone();
        // for p in &self.possibles {
        //     definite = definite.join(p);
        // }

        // println!("possibles: {:?} {:?}", self.definite, self.possibles);

        FlowType::Any
    }

    fn join(&mut self, child: FlowType) {
        if self.break_or_continue_or_return {
            return;
        }

        match (child, &self.definite) {
            (FlowType::Clause, _) => {}
            (FlowType::Undef, _) => {}
            (FlowType::Any, _) | (_, FlowType::Any) => {}
            (FlowType::Infer, _) => {}
            (FlowType::None, _) => {}
            // todo: mystery flow none
            (FlowType::FlowNone, _) => {}
            (FlowType::Content, FlowType::Content) => {}
            (FlowType::Content, FlowType::None) => self.definite = FlowType::Content,
            (FlowType::Content, _) => self.definite = FlowType::Undef,
            (FlowType::Var(v), _) => self.possibles.push(FlowType::Var(v)),
            // todo: check possibles
            (FlowType::Array(e), FlowType::None) => self.definite = FlowType::Array(e),
            (FlowType::Array(..), _) => self.definite = FlowType::Undef,
            (FlowType::Tuple(e), FlowType::None) => self.definite = FlowType::Tuple(e),
            (FlowType::Tuple(..), _) => self.definite = FlowType::Undef,
            // todo: possible some style
            (FlowType::Auto, FlowType::None) => self.definite = FlowType::Auto,
            (FlowType::Auto, _) => self.definite = FlowType::Undef,
            (FlowType::Builtin(b), FlowType::None) => self.definite = FlowType::Builtin(b),
            (FlowType::Builtin(..), _) => self.definite = FlowType::Undef,
            // todo: value join
            (FlowType::Value(v), FlowType::None) => self.definite = FlowType::Value(v),
            (FlowType::Value(..), _) => self.definite = FlowType::Undef,
            (FlowType::ValueDoc(v), FlowType::None) => self.definite = FlowType::ValueDoc(v),
            (FlowType::ValueDoc(..), _) => self.definite = FlowType::Undef,
            (FlowType::Element(e), FlowType::None) => self.definite = FlowType::Element(e),
            (FlowType::Element(..), _) => self.definite = FlowType::Undef,
            (FlowType::Func(f), FlowType::None) => self.definite = FlowType::Func(f),
            (FlowType::Func(..), _) => self.definite = FlowType::Undef,
            (FlowType::Dict(w), FlowType::None) => self.definite = FlowType::Dict(w),
            (FlowType::Dict(..), _) => self.definite = FlowType::Undef,
            (FlowType::With(w), FlowType::None) => self.definite = FlowType::With(w),
            (FlowType::With(..), _) => self.definite = FlowType::Undef,
            (FlowType::Args(w), FlowType::None) => self.definite = FlowType::Args(w),
            (FlowType::Args(..), _) => self.definite = FlowType::Undef,
            (FlowType::At(w), FlowType::None) => self.definite = FlowType::At(w),
            (FlowType::At(..), _) => self.definite = FlowType::Undef,
            (FlowType::Unary(w), FlowType::None) => self.definite = FlowType::Unary(w),
            (FlowType::Unary(..), _) => self.definite = FlowType::Undef,
            (FlowType::Binary(w), FlowType::None) => self.definite = FlowType::Binary(w),
            (FlowType::Binary(..), _) => self.definite = FlowType::Undef,
            (FlowType::If(w), FlowType::None) => self.definite = FlowType::If(w),
            (FlowType::If(..), _) => self.definite = FlowType::Undef,
            (FlowType::Union(w), FlowType::None) => self.definite = FlowType::Union(w),
            (FlowType::Union(..), _) => self.definite = FlowType::Undef,
            (FlowType::Let(w), FlowType::None) => self.definite = FlowType::Let(w),
            (FlowType::Let(..), _) => self.definite = FlowType::Undef,
            (FlowType::Boolean(b), FlowType::None) => self.definite = FlowType::Boolean(b),
            (FlowType::Boolean(..), _) => self.definite = FlowType::Undef,
        }
    }
}
impl Default for Joiner {
    fn default() -> Self {
        Self {
            break_or_continue_or_return: false,
            definite: FlowType::None,
            possibles: Vec::new(),
        }
    }
}
