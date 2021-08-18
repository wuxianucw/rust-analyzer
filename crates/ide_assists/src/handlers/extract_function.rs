use std::{hash::BuildHasherDefault, iter};

use ast::make;
use either::Either;
use hir::{HirDisplay, Local, Semantics, TypeInfo};
use ide_db::{
    defs::{Definition, NameRefClass},
    search::{FileReference, ReferenceAccess, SearchScope},
    RootDatabase,
};
use itertools::Itertools;
use rustc_hash::FxHasher;
use stdx::format_to;
use syntax::{
    ast::{
        self,
        edit::{AstNodeEdit, IndentLevel},
        AstNode,
    },
    match_ast, ted,
    SyntaxKind::{self, COMMENT},
    SyntaxNode, SyntaxToken, TextRange, TextSize, TokenAtOffset, WalkEvent, T,
};

use crate::{
    assist_context::{AssistContext, Assists, TreeMutator},
    AssistId,
};

type FxIndexSet<T> = indexmap::IndexSet<T, BuildHasherDefault<FxHasher>>;

// Assist: extract_function
//
// Extracts selected statements into new function.
//
// ```
// fn main() {
//     let n = 1;
//     $0let m = n + 2;
//     let k = m + n;$0
//     let g = 3;
// }
// ```
// ->
// ```
// fn main() {
//     let n = 1;
//     fun_name(n);
//     let g = 3;
// }
//
// fn $0fun_name(n: i32) {
//     let m = n + 2;
//     let k = m + n;
// }
// ```
pub(crate) fn extract_function(acc: &mut Assists, ctx: &AssistContext) -> Option<()> {
    let range = ctx.frange.range;
    if range.is_empty() {
        return None;
    }

    let node = ctx.covering_element();
    if node.kind() == COMMENT {
        cov_mark::hit!(extract_function_in_comment_is_not_applicable);
        return None;
    }

    let node = match node {
        syntax::NodeOrToken::Node(n) => n,
        syntax::NodeOrToken::Token(t) => t.parent()?,
    };
    let body = extraction_target(&node, range)?;
    let container_info = body.analyze_container(&ctx.sema)?;

    let (locals_used, self_param) = body.analyze(&ctx.sema);

    let anchor = if self_param.is_some() { Anchor::Method } else { Anchor::Freestanding };
    let insert_after = node_to_insert_after(&body, anchor)?;
    let module = ctx.sema.scope(&insert_after).module()?;

    let ret_ty = body.return_ty(ctx)?;
    let control_flow = body.external_control_flow(ctx, &container_info)?;
    let ret_values = body.ret_values(ctx, node.parent().as_ref().unwrap_or(&node));

    let target_range = body.text_range();

    acc.add(
        AssistId("extract_function", crate::AssistKind::RefactorExtract),
        "Extract into function",
        target_range,
        move |builder| {
            let outliving_locals: Vec<_> = ret_values.collect();
            if stdx::never!(!outliving_locals.is_empty() && !ret_ty.is_unit()) {
                // We should not have variables that outlive body if we have expression block
                return;
            }

            let params =
                body.extracted_function_params(ctx, &container_info, locals_used.iter().copied());

            let fun = Function {
                name: make::name_ref("fun_name"),
                self_param,
                params,
                control_flow,
                ret_ty,
                body,
                outliving_locals,
                mods: container_info,
            };

            let new_indent = IndentLevel::from_node(&insert_after);
            let old_indent = fun.body.indent_level();

            builder.replace(target_range, make_call(ctx, &fun, old_indent));

            let fn_def = format_function(ctx, module, &fun, old_indent, new_indent);
            let insert_offset = insert_after.text_range().end();
            match ctx.config.snippet_cap {
                Some(cap) => builder.insert_snippet(cap, insert_offset, fn_def),
                None => builder.insert(insert_offset, fn_def),
            }
        },
    )
}

/// Try to guess what user wants to extract
///
/// We have basically have two cases:
/// * We want whole node, like `loop {}`, `2 + 2`, `{ let n = 1; }` exprs.
///   Then we can use `ast::Expr`
/// * We want a few statements for a block. E.g.
///   ```rust,no_run
///   fn foo() -> i32 {
///     let m = 1;
///     $0
///     let n = 2;
///     let k = 3;
///     k + n
///     $0
///   }
///   ```
///
fn extraction_target(node: &SyntaxNode, selection_range: TextRange) -> Option<FunctionBody> {
    if let Some(stmt) = ast::Stmt::cast(node.clone()) {
        return match stmt {
            ast::Stmt::Item(_) => None,
            ast::Stmt::ExprStmt(_) | ast::Stmt::LetStmt(_) => Some(FunctionBody::from_range(
                node.parent().and_then(ast::BlockExpr::cast)?,
                node.text_range(),
            )),
        };
    }

    let expr = ast::Expr::cast(node.clone())?;
    // A node got selected fully
    if node.text_range() == selection_range {
        return FunctionBody::from_expr(expr.clone());
    }

    // Covering element returned the parent block of one or multiple statements that have been selected
    if let ast::Expr::BlockExpr(block) = expr {
        // Extract the full statements.
        return Some(FunctionBody::from_range(block, selection_range));
    }

    node.ancestors().find_map(ast::Expr::cast).and_then(FunctionBody::from_expr)
}

#[derive(Debug)]
struct Function {
    name: ast::NameRef,
    self_param: Option<ast::SelfParam>,
    params: Vec<Param>,
    control_flow: ControlFlow,
    ret_ty: RetType,
    body: FunctionBody,
    outliving_locals: Vec<OutlivedLocal>,
    mods: ContainerInfo,
}

#[derive(Debug)]
struct Param {
    var: Local,
    ty: hir::Type,
    move_local: bool,
    requires_mut: bool,
    is_copy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamKind {
    Value,
    MutValue,
    SharedRef,
    MutRef,
}

#[derive(Debug, Eq, PartialEq)]
enum FunType {
    Unit,
    Single(hir::Type),
    Tuple(Vec<hir::Type>),
}

/// Where to put extracted function definition
#[derive(Debug)]
enum Anchor {
    /// Extract free function and put right after current top-level function
    Freestanding,
    /// Extract method and put right after current function in the impl-block
    Method,
}

// FIXME: ControlFlow and ContainerInfo both track some function modifiers, feels like these two should
// probably be merged somehow.
#[derive(Debug)]
struct ControlFlow {
    kind: Option<FlowKind>,
    is_async: bool,
    is_unsafe: bool,
}

/// The thing whose expression we are extracting from. Can be a function, const, static, const arg, ...
#[derive(Clone, Debug)]
struct ContainerInfo {
    is_const: bool,
    is_in_tail: bool,
    parent_loop: Option<SyntaxNode>,
    /// The function's return type, const's type etc.
    ret_type: Option<hir::Type>,
}

/// Control flow that is exported from extracted function
///
/// E.g.:
/// ```rust,no_run
/// loop {
///     $0
///     if 42 == 42 {
///         break;
///     }
///     $0
/// }
/// ```
#[derive(Debug, Clone)]
enum FlowKind {
    /// Return with value (`return $expr;`)
    Return(Option<ast::Expr>),
    Try {
        kind: TryKind,
    },
    /// Break with value (`break $expr;`)
    Break(Option<ast::Expr>),
    /// Continue
    Continue,
}

#[derive(Debug, Clone)]
enum TryKind {
    Option,
    Result { ty: hir::Type },
}

#[derive(Debug)]
enum RetType {
    Expr(hir::Type),
    Stmt,
}

impl RetType {
    fn is_unit(&self) -> bool {
        match self {
            RetType::Expr(ty) => ty.is_unit(),
            RetType::Stmt => true,
        }
    }
}

/// Semantically same as `ast::Expr`, but preserves identity when using only part of the Block
/// This is the future function body, the part that is being extracted.
#[derive(Debug)]
enum FunctionBody {
    Expr(ast::Expr),
    Span { parent: ast::BlockExpr, text_range: TextRange },
}

#[derive(Debug)]
struct OutlivedLocal {
    local: Local,
    mut_usage_outside_body: bool,
}

/// Container of local variable usages
///
/// Semanticall same as `UsageSearchResult`, but provides more convenient interface
struct LocalUsages(ide_db::search::UsageSearchResult);

impl LocalUsages {
    fn find_local_usages(ctx: &AssistContext, var: Local) -> Self {
        Self(
            Definition::Local(var)
                .usages(&ctx.sema)
                .in_scope(SearchScope::single_file(ctx.frange.file_id))
                .all(),
        )
    }

    fn iter(&self) -> impl Iterator<Item = &FileReference> + '_ {
        self.0.iter().flat_map(|(_, rs)| rs)
    }
}

impl Function {
    fn return_type(&self, ctx: &AssistContext) -> FunType {
        match &self.ret_ty {
            RetType::Expr(ty) if ty.is_unit() => FunType::Unit,
            RetType::Expr(ty) => FunType::Single(ty.clone()),
            RetType::Stmt => match self.outliving_locals.as_slice() {
                [] => FunType::Unit,
                [var] => FunType::Single(var.local.ty(ctx.db())),
                vars => {
                    let types = vars.iter().map(|v| v.local.ty(ctx.db())).collect();
                    FunType::Tuple(types)
                }
            },
        }
    }
}

impl ParamKind {
    fn is_ref(&self) -> bool {
        matches!(self, ParamKind::SharedRef | ParamKind::MutRef)
    }
}

impl Param {
    fn kind(&self) -> ParamKind {
        match (self.move_local, self.requires_mut, self.is_copy) {
            (false, true, _) => ParamKind::MutRef,
            (false, false, false) => ParamKind::SharedRef,
            (true, true, _) => ParamKind::MutValue,
            (_, false, _) => ParamKind::Value,
        }
    }

    fn to_arg(&self, ctx: &AssistContext) -> ast::Expr {
        let var = path_expr_from_local(ctx, self.var);
        match self.kind() {
            ParamKind::Value | ParamKind::MutValue => var,
            ParamKind::SharedRef => make::expr_ref(var, false),
            ParamKind::MutRef => make::expr_ref(var, true),
        }
    }

    fn to_param(&self, ctx: &AssistContext, module: hir::Module) -> ast::Param {
        let var = self.var.name(ctx.db()).unwrap().to_string();
        let var_name = make::name(&var);
        let pat = match self.kind() {
            ParamKind::MutValue => make::ident_pat(false, true, var_name),
            ParamKind::Value | ParamKind::SharedRef | ParamKind::MutRef => {
                make::ext::simple_ident_pat(var_name)
            }
        };

        let ty = make_ty(&self.ty, ctx, module);
        let ty = match self.kind() {
            ParamKind::Value | ParamKind::MutValue => ty,
            ParamKind::SharedRef => make::ty_ref(ty, false),
            ParamKind::MutRef => make::ty_ref(ty, true),
        };

        make::param(pat.into(), ty)
    }
}

impl TryKind {
    fn of_ty(ty: hir::Type, ctx: &AssistContext) -> Option<TryKind> {
        if ty.is_unknown() {
            // We favour Result for `expr?`
            return Some(TryKind::Result { ty });
        }
        let adt = ty.as_adt()?;
        let name = adt.name(ctx.db());
        // FIXME: use lang items to determine if it is std type or user defined
        //        E.g. if user happens to define type named `Option`, we would have false positive
        match name.to_string().as_str() {
            "Option" => Some(TryKind::Option),
            "Result" => Some(TryKind::Result { ty }),
            _ => None,
        }
    }
}

impl FlowKind {
    fn make_result_handler(&self, expr: Option<ast::Expr>) -> ast::Expr {
        match self {
            FlowKind::Return(_) => make::expr_return(expr),
            FlowKind::Break(_) => make::expr_break(expr),
            FlowKind::Try { .. } => {
                stdx::never!("cannot have result handler with try");
                expr.unwrap_or_else(|| make::expr_return(None))
            }
            FlowKind::Continue => {
                stdx::always!(expr.is_none(), "continue with value is not possible");
                make::expr_continue()
            }
        }
    }

    fn expr_ty(&self, ctx: &AssistContext) -> Option<hir::Type> {
        match self {
            FlowKind::Return(Some(expr)) | FlowKind::Break(Some(expr)) => {
                ctx.sema.type_of_expr(expr).map(TypeInfo::adjusted)
            }
            FlowKind::Try { .. } => {
                stdx::never!("try does not have defined expr_ty");
                None
            }
            _ => None,
        }
    }
}

impl FunctionBody {
    fn parent(&self) -> Option<SyntaxNode> {
        match self {
            FunctionBody::Expr(expr) => expr.syntax().parent(),
            FunctionBody::Span { parent, .. } => Some(parent.syntax().clone()),
        }
    }

    fn from_expr(expr: ast::Expr) -> Option<Self> {
        match expr {
            ast::Expr::BreakExpr(it) => it.expr().map(Self::Expr),
            ast::Expr::ReturnExpr(it) => it.expr().map(Self::Expr),
            ast::Expr::BlockExpr(it) if !it.is_standalone() => None,
            expr => Some(Self::Expr(expr)),
        }
    }

    fn from_range(parent: ast::BlockExpr, selected: TextRange) -> FunctionBody {
        let mut text_range = parent
            .statements()
            .map(|stmt| stmt.syntax().text_range())
            .filter(|&stmt| selected.intersect(stmt).filter(|it| !it.is_empty()).is_some())
            .fold1(|acc, stmt| acc.cover(stmt));
        if let Some(tail_range) = parent
            .tail_expr()
            .map(|it| it.syntax().text_range())
            .filter(|&it| selected.intersect(it).is_some())
        {
            text_range = Some(match text_range {
                Some(text_range) => text_range.cover(tail_range),
                None => tail_range,
            });
        }
        Self::Span { parent, text_range: text_range.unwrap_or(selected) }
    }

    fn indent_level(&self) -> IndentLevel {
        match &self {
            FunctionBody::Expr(expr) => IndentLevel::from_node(expr.syntax()),
            FunctionBody::Span { parent, .. } => IndentLevel::from_node(parent.syntax()) + 1,
        }
    }

    fn tail_expr(&self) -> Option<ast::Expr> {
        match &self {
            FunctionBody::Expr(expr) => Some(expr.clone()),
            FunctionBody::Span { parent, text_range } => {
                let tail_expr = parent.tail_expr()?;
                text_range.contains_range(tail_expr.syntax().text_range()).then(|| tail_expr)
            }
        }
    }

    fn walk_expr(&self, cb: &mut dyn FnMut(ast::Expr)) {
        match self {
            FunctionBody::Expr(expr) => expr.walk(cb),
            FunctionBody::Span { parent, text_range } => {
                parent
                    .statements()
                    .filter(|stmt| text_range.contains_range(stmt.syntax().text_range()))
                    .filter_map(|stmt| match stmt {
                        ast::Stmt::ExprStmt(expr_stmt) => expr_stmt.expr(),
                        ast::Stmt::Item(_) => None,
                        ast::Stmt::LetStmt(stmt) => stmt.initializer(),
                    })
                    .for_each(|expr| expr.walk(cb));
                if let Some(expr) = parent
                    .tail_expr()
                    .filter(|it| text_range.contains_range(it.syntax().text_range()))
                {
                    expr.walk(cb);
                }
            }
        }
    }

    fn preorder_expr(&self, cb: &mut dyn FnMut(WalkEvent<ast::Expr>) -> bool) {
        match self {
            FunctionBody::Expr(expr) => expr.preorder(cb),
            FunctionBody::Span { parent, text_range } => {
                parent
                    .statements()
                    .filter(|stmt| text_range.contains_range(stmt.syntax().text_range()))
                    .filter_map(|stmt| match stmt {
                        ast::Stmt::ExprStmt(expr_stmt) => expr_stmt.expr(),
                        ast::Stmt::Item(_) => None,
                        ast::Stmt::LetStmt(stmt) => stmt.initializer(),
                    })
                    .for_each(|expr| expr.preorder(cb));
                if let Some(expr) = parent
                    .tail_expr()
                    .filter(|it| text_range.contains_range(it.syntax().text_range()))
                {
                    expr.preorder(cb);
                }
            }
        }
    }

    fn walk_pat(&self, cb: &mut dyn FnMut(ast::Pat)) {
        match self {
            FunctionBody::Expr(expr) => expr.walk_patterns(cb),
            FunctionBody::Span { parent, text_range } => {
                parent
                    .statements()
                    .filter(|stmt| text_range.contains_range(stmt.syntax().text_range()))
                    .for_each(|stmt| match stmt {
                        ast::Stmt::ExprStmt(expr_stmt) => {
                            if let Some(expr) = expr_stmt.expr() {
                                expr.walk_patterns(cb)
                            }
                        }
                        ast::Stmt::Item(_) => (),
                        ast::Stmt::LetStmt(stmt) => {
                            if let Some(pat) = stmt.pat() {
                                pat.walk(cb);
                            }
                            if let Some(expr) = stmt.initializer() {
                                expr.walk_patterns(cb);
                            }
                        }
                    });
                if let Some(expr) = parent
                    .tail_expr()
                    .filter(|it| text_range.contains_range(it.syntax().text_range()))
                {
                    expr.walk_patterns(cb);
                }
            }
        }
    }

    fn text_range(&self) -> TextRange {
        match self {
            FunctionBody::Expr(expr) => expr.syntax().text_range(),
            &FunctionBody::Span { text_range, .. } => text_range,
        }
    }

    fn contains_range(&self, range: TextRange) -> bool {
        self.text_range().contains_range(range)
    }

    fn precedes_range(&self, range: TextRange) -> bool {
        self.text_range().end() <= range.start()
    }

    fn contains_node(&self, node: &SyntaxNode) -> bool {
        self.contains_range(node.text_range())
    }
}

impl FunctionBody {
    /// Analyzes a function body, returning the used local variables that are referenced in it as well as
    /// whether it contains an await expression.
    fn analyze(
        &self,
        sema: &Semantics<RootDatabase>,
    ) -> (FxIndexSet<Local>, Option<ast::SelfParam>) {
        // FIXME: currently usages inside macros are not found
        let mut self_param = None;
        let mut res = FxIndexSet::default();
        self.walk_expr(&mut |expr| {
            let name_ref = match expr {
                ast::Expr::PathExpr(path_expr) => {
                    path_expr.path().and_then(|it| it.as_single_name_ref())
                }
                _ => return,
            };
            if let Some(name_ref) = name_ref {
                if let Some(
                    NameRefClass::Definition(Definition::Local(local_ref))
                    | NameRefClass::FieldShorthand { local_ref, field_ref: _ },
                ) = NameRefClass::classify(sema, &name_ref)
                {
                    if local_ref.is_self(sema.db) {
                        match local_ref.source(sema.db).value {
                            Either::Right(it) => {
                                stdx::always!(
                                    self_param.replace(it).is_none(),
                                    "body references two different self params"
                                );
                            }
                            Either::Left(_) => {
                                stdx::never!(
                                    "Local::is_self returned true, but source is IdentPat"
                                );
                            }
                        }
                    } else {
                        res.insert(local_ref);
                    }
                }
            }
        });
        (res, self_param)
    }

    fn analyze_container(&self, sema: &Semantics<RootDatabase>) -> Option<ContainerInfo> {
        let mut ancestors = self.parent()?.ancestors();
        let infer_expr_opt = |expr| sema.type_of_expr(&expr?).map(TypeInfo::adjusted);
        let mut parent_loop = None;
        let mut set_parent_loop = |loop_: &dyn ast::LoopBodyOwner| {
            if loop_
                .loop_body()
                .map_or(false, |it| it.syntax().text_range().contains_range(self.text_range()))
            {
                parent_loop.get_or_insert(loop_.syntax().clone());
            }
        };
        let (is_const, expr, ty) = loop {
            let anc = ancestors.next()?;
            break match_ast! {
                match anc {
                    ast::ClosureExpr(closure) => (false, closure.body(), infer_expr_opt(closure.body())),
                    ast::EffectExpr(effect) => {
                        let (constness, block) = match effect.effect() {
                            ast::Effect::Const(_) => (true, effect.block_expr()),
                            ast::Effect::Try(_) => (false, effect.block_expr()),
                            ast::Effect::Label(label) if label.lifetime().is_some() => (false, effect.block_expr()),
                            _ => continue,
                        };
                        let expr = block.map(ast::Expr::BlockExpr);
                        (constness, expr.clone(), infer_expr_opt(expr))
                    },
                    ast::Fn(fn_) => {
                        (fn_.const_token().is_some(), fn_.body().map(ast::Expr::BlockExpr), Some(sema.to_def(&fn_)?.ret_type(sema.db)))
                    },
                    ast::Static(statik) => {
                        (true, statik.body(), Some(sema.to_def(&statik)?.ty(sema.db)))
                    },
                    ast::ConstArg(ca) => {
                        (true, ca.expr(), infer_expr_opt(ca.expr()))
                    },
                    ast::Const(konst) => {
                        (true, konst.body(), Some(sema.to_def(&konst)?.ty(sema.db)))
                    },
                    ast::ConstParam(cp) => {
                        (true, cp.default_val(), Some(sema.to_def(&cp)?.ty(sema.db)))
                    },
                    ast::ConstBlockPat(cbp) => {
                        let expr = cbp.block_expr().map(ast::Expr::BlockExpr);
                        (true, expr.clone(), infer_expr_opt(expr))
                    },
                    ast::Variant(__) => return None,
                    ast::Meta(__) => return None,
                    ast::LoopExpr(it) => {
                        set_parent_loop(&it);
                        continue;
                    },
                    ast::ForExpr(it) => {
                        set_parent_loop(&it);
                        continue;
                    },
                    ast::WhileExpr(it) => {
                        set_parent_loop(&it);
                        continue;
                    },
                    _ => continue,
                }
            };
        };
        let container_tail = match expr? {
            ast::Expr::BlockExpr(block) => block.tail_expr(),
            expr => Some(expr),
        };
        let is_in_tail =
            container_tail.zip(self.tail_expr()).map_or(false, |(container_tail, body_tail)| {
                container_tail.syntax().text_range().contains_range(body_tail.syntax().text_range())
            });
        Some(ContainerInfo { is_in_tail, is_const, parent_loop, ret_type: ty })
    }

    fn return_ty(&self, ctx: &AssistContext) -> Option<RetType> {
        match self.tail_expr() {
            Some(expr) => ctx.sema.type_of_expr(&expr).map(TypeInfo::original).map(RetType::Expr),
            None => Some(RetType::Stmt),
        }
    }

    /// Local variables defined inside `body` that are accessed outside of it
    fn ret_values<'a>(
        &self,
        ctx: &'a AssistContext,
        parent: &SyntaxNode,
    ) -> impl Iterator<Item = OutlivedLocal> + 'a {
        let parent = parent.clone();
        let range = self.text_range();
        locals_defined_in_body(&ctx.sema, self)
            .into_iter()
            .filter_map(move |local| local_outlives_body(ctx, range, local, &parent))
    }

    /// Analyses the function body for external control flow.
    fn external_control_flow(
        &self,
        ctx: &AssistContext,
        container_info: &ContainerInfo,
    ) -> Option<ControlFlow> {
        let mut ret_expr = None;
        let mut try_expr = None;
        let mut break_expr = None;
        let mut continue_expr = None;
        let mut is_async = false;
        let mut _is_unsafe = false;

        let mut unsafe_depth = 0;
        let mut loop_depth = 0;

        self.preorder_expr(&mut |expr| {
            let expr = match expr {
                WalkEvent::Enter(e) => e,
                WalkEvent::Leave(expr) => {
                    match expr {
                        ast::Expr::LoopExpr(_)
                        | ast::Expr::ForExpr(_)
                        | ast::Expr::WhileExpr(_) => loop_depth -= 1,
                        ast::Expr::EffectExpr(effect) if effect.unsafe_token().is_some() => {
                            unsafe_depth -= 1
                        }
                        _ => (),
                    }
                    return false;
                }
            };
            match expr {
                ast::Expr::LoopExpr(_) | ast::Expr::ForExpr(_) | ast::Expr::WhileExpr(_) => {
                    loop_depth += 1;
                }
                ast::Expr::EffectExpr(effect) if effect.unsafe_token().is_some() => {
                    unsafe_depth += 1
                }
                ast::Expr::ReturnExpr(it) => {
                    ret_expr = Some(it);
                }
                ast::Expr::TryExpr(it) => {
                    try_expr = Some(it);
                }
                ast::Expr::BreakExpr(it) if loop_depth == 0 => {
                    break_expr = Some(it);
                }
                ast::Expr::ContinueExpr(it) if loop_depth == 0 => {
                    continue_expr = Some(it);
                }
                ast::Expr::AwaitExpr(_) => is_async = true,
                // FIXME: Do unsafe analysis on expression, sem highlighting knows this so we should be able
                // to just lift that out of there
                // expr if unsafe_depth ==0 && expr.is_unsafe => is_unsafe = true,
                _ => {}
            }
            false
        });

        let kind = match (try_expr, ret_expr, break_expr, continue_expr) {
            (Some(_), _, None, None) => {
                let ret_ty = container_info.ret_type.clone()?;
                let kind = TryKind::of_ty(ret_ty, ctx)?;

                Some(FlowKind::Try { kind })
            }
            (Some(_), _, _, _) => {
                cov_mark::hit!(external_control_flow_try_and_bc);
                return None;
            }
            (None, Some(r), None, None) => Some(FlowKind::Return(r.expr())),
            (None, Some(_), _, _) => {
                cov_mark::hit!(external_control_flow_return_and_bc);
                return None;
            }
            (None, None, Some(_), Some(_)) => {
                cov_mark::hit!(external_control_flow_break_and_continue);
                return None;
            }
            (None, None, Some(b), None) => Some(FlowKind::Break(b.expr())),
            (None, None, None, Some(_)) => Some(FlowKind::Continue),
            (None, None, None, None) => None,
        };

        Some(ControlFlow { kind, is_async, is_unsafe: _is_unsafe })
    }

    /// find variables that should be extracted as params
    ///
    /// Computes additional info that affects param type and mutability
    fn extracted_function_params(
        &self,
        ctx: &AssistContext,
        container_info: &ContainerInfo,
        locals: impl Iterator<Item = Local>,
    ) -> Vec<Param> {
        locals
            .map(|local| (local, local.source(ctx.db())))
            .filter(|(_, src)| is_defined_outside_of_body(ctx, self, src))
            .filter_map(|(local, src)| {
                if let Either::Left(src) = src.value {
                    Some((local, src))
                } else {
                    stdx::never!(false, "Local::is_self returned false, but source is SelfParam");
                    None
                }
            })
            .map(|(var, src)| {
                let usages = LocalUsages::find_local_usages(ctx, var);
                let ty = var.ty(ctx.db());

                let defined_outside_parent_loop = container_info
                    .parent_loop
                    .as_ref()
                    .map_or(true, |it| it.text_range().contains_range(src.syntax().text_range()));

                let is_copy = ty.is_copy(ctx.db());
                let has_usages = self.has_usages_after_body(&usages);
                let requires_mut =
                    !ty.is_mutable_reference() && has_exclusive_usages(ctx, &usages, self);
                // We can move the value into the function call if it's not used after the call,
                // if the var is not used but defined outside a loop we are extracting from we can't move it either
                // as the function will reuse it in the next iteration.
                let move_local = !has_usages && defined_outside_parent_loop;
                Param { var, ty, move_local, requires_mut, is_copy }
            })
            .collect()
    }

    fn has_usages_after_body(&self, usages: &LocalUsages) -> bool {
        usages.iter().any(|reference| self.precedes_range(reference.range))
    }
}

/// checks if relevant var is used with `&mut` access inside body
fn has_exclusive_usages(ctx: &AssistContext, usages: &LocalUsages, body: &FunctionBody) -> bool {
    usages
        .iter()
        .filter(|reference| body.contains_range(reference.range))
        .any(|reference| reference_is_exclusive(reference, body, ctx))
}

/// checks if this reference requires `&mut` access inside node
fn reference_is_exclusive(
    reference: &FileReference,
    node: &dyn HasTokenAtOffset,
    ctx: &AssistContext,
) -> bool {
    // we directly modify variable with set: `n = 0`, `n += 1`
    if reference.access == Some(ReferenceAccess::Write) {
        return true;
    }

    // we take `&mut` reference to variable: `&mut v`
    let path = match path_element_of_reference(node, reference) {
        Some(path) => path,
        None => return false,
    };

    expr_require_exclusive_access(ctx, &path).unwrap_or(false)
}

/// checks if this expr requires `&mut` access, recurses on field access
fn expr_require_exclusive_access(ctx: &AssistContext, expr: &ast::Expr) -> Option<bool> {
    match expr {
        ast::Expr::MacroCall(_) => {
            // FIXME: expand macro and check output for mutable usages of the variable?
            return None;
        }
        _ => (),
    }

    let parent = expr.syntax().parent()?;

    if let Some(bin_expr) = ast::BinExpr::cast(parent.clone()) {
        if matches!(bin_expr.op_kind()?, ast::BinaryOp::Assignment { .. }) {
            return Some(bin_expr.lhs()?.syntax() == expr.syntax());
        }
        return Some(false);
    }

    if let Some(ref_expr) = ast::RefExpr::cast(parent.clone()) {
        return Some(ref_expr.mut_token().is_some());
    }

    if let Some(method_call) = ast::MethodCallExpr::cast(parent.clone()) {
        let func = ctx.sema.resolve_method_call(&method_call)?;
        let self_param = func.self_param(ctx.db())?;
        let access = self_param.access(ctx.db());

        return Some(matches!(access, hir::Access::Exclusive));
    }

    if let Some(field) = ast::FieldExpr::cast(parent) {
        return expr_require_exclusive_access(ctx, &field.into());
    }

    Some(false)
}

trait HasTokenAtOffset {
    fn token_at_offset(&self, offset: TextSize) -> TokenAtOffset<SyntaxToken>;
}

impl HasTokenAtOffset for SyntaxNode {
    fn token_at_offset(&self, offset: TextSize) -> TokenAtOffset<SyntaxToken> {
        SyntaxNode::token_at_offset(self, offset)
    }
}

impl HasTokenAtOffset for FunctionBody {
    fn token_at_offset(&self, offset: TextSize) -> TokenAtOffset<SyntaxToken> {
        match self {
            FunctionBody::Expr(expr) => expr.syntax().token_at_offset(offset),
            FunctionBody::Span { parent, text_range } => {
                match parent.syntax().token_at_offset(offset) {
                    TokenAtOffset::None => TokenAtOffset::None,
                    TokenAtOffset::Single(t) => {
                        if text_range.contains_range(t.text_range()) {
                            TokenAtOffset::Single(t)
                        } else {
                            TokenAtOffset::None
                        }
                    }
                    TokenAtOffset::Between(a, b) => {
                        match (
                            text_range.contains_range(a.text_range()),
                            text_range.contains_range(b.text_range()),
                        ) {
                            (true, true) => TokenAtOffset::Between(a, b),
                            (true, false) => TokenAtOffset::Single(a),
                            (false, true) => TokenAtOffset::Single(b),
                            (false, false) => TokenAtOffset::None,
                        }
                    }
                }
            }
        }
    }
}

/// find relevant `ast::Expr` for reference
///
/// # Preconditions
///
/// `node` must cover `reference`, that is `node.text_range().contains_range(reference.range)`
fn path_element_of_reference(
    node: &dyn HasTokenAtOffset,
    reference: &FileReference,
) -> Option<ast::Expr> {
    let token = node.token_at_offset(reference.range.start()).right_biased().or_else(|| {
        stdx::never!(false, "cannot find token at variable usage: {:?}", reference);
        None
    })?;
    let path = token.ancestors().find_map(ast::Expr::cast).or_else(|| {
        stdx::never!(false, "cannot find path parent of variable usage: {:?}", token);
        None
    })?;
    stdx::always!(
        matches!(path, ast::Expr::PathExpr(_) | ast::Expr::MacroCall(_)),
        "unexpected expression type for variable usage: {:?}",
        path
    );
    Some(path)
}

/// list local variables defined inside `body`
fn locals_defined_in_body(
    sema: &Semantics<RootDatabase>,
    body: &FunctionBody,
) -> FxIndexSet<Local> {
    // FIXME: this doesn't work well with macros
    //        see https://github.com/rust-analyzer/rust-analyzer/pull/7535#discussion_r570048550
    let mut res = FxIndexSet::default();
    body.walk_pat(&mut |pat| {
        if let ast::Pat::IdentPat(pat) = pat {
            if let Some(local) = sema.to_def(&pat) {
                res.insert(local);
            }
        }
    });
    res
}

/// Returns usage details if local variable is used after(outside of) body
fn local_outlives_body(
    ctx: &AssistContext,
    body_range: TextRange,
    local: Local,
    parent: &SyntaxNode,
) -> Option<OutlivedLocal> {
    let usages = LocalUsages::find_local_usages(ctx, local);
    let mut has_mut_usages = false;
    let mut any_outlives = false;
    for usage in usages.iter() {
        if body_range.end() <= usage.range.start() {
            has_mut_usages |= reference_is_exclusive(usage, parent, ctx);
            any_outlives |= true;
            if has_mut_usages {
                break; // no need to check more elements we have all the info we wanted
            }
        }
    }
    if !any_outlives {
        return None;
    }
    Some(OutlivedLocal { local, mut_usage_outside_body: has_mut_usages })
}

/// checks if the relevant local was defined before(outside of) body
fn is_defined_outside_of_body(
    ctx: &AssistContext,
    body: &FunctionBody,
    src: &hir::InFile<Either<ast::IdentPat, ast::SelfParam>>,
) -> bool {
    src.file_id.original_file(ctx.db()) == ctx.frange.file_id
        && !body.contains_node(either_syntax(&src.value))
}

fn either_syntax(value: &Either<ast::IdentPat, ast::SelfParam>) -> &SyntaxNode {
    match value {
        Either::Left(pat) => pat.syntax(),
        Either::Right(it) => it.syntax(),
    }
}

/// find where to put extracted function definition
///
/// Function should be put right after returned node
fn node_to_insert_after(body: &FunctionBody, anchor: Anchor) -> Option<SyntaxNode> {
    let node = match body {
        FunctionBody::Expr(e) => e.syntax(),
        FunctionBody::Span { parent, .. } => parent.syntax(),
    };
    let mut ancestors = node.ancestors().peekable();
    let mut last_ancestor = None;
    while let Some(next_ancestor) = ancestors.next() {
        match next_ancestor.kind() {
            SyntaxKind::SOURCE_FILE => break,
            SyntaxKind::ITEM_LIST if !matches!(anchor, Anchor::Freestanding) => continue,
            SyntaxKind::ITEM_LIST => {
                if ancestors.peek().map(SyntaxNode::kind) == Some(SyntaxKind::MODULE) {
                    break;
                }
            }
            SyntaxKind::ASSOC_ITEM_LIST if !matches!(anchor, Anchor::Method) => {
                continue;
            }
            SyntaxKind::ASSOC_ITEM_LIST => {
                if ancestors.peek().map(SyntaxNode::kind) == Some(SyntaxKind::IMPL) {
                    break;
                }
            }
            _ => (),
        }
        last_ancestor = Some(next_ancestor);
    }
    last_ancestor
}

fn make_call(ctx: &AssistContext, fun: &Function, indent: IndentLevel) -> String {
    let ret_ty = fun.return_type(ctx);

    let args = make::arg_list(fun.params.iter().map(|param| param.to_arg(ctx)));
    let name = fun.name.clone();
    let call_expr = if fun.self_param.is_some() {
        let self_arg = make::expr_path(make::ext::ident_path("self"));
        make::expr_method_call(self_arg, name, args)
    } else {
        let func = make::expr_path(make::path_unqualified(make::path_segment(name)));
        make::expr_call(func, args)
    };

    let handler = FlowHandler::from_ret_ty(fun, &ret_ty);

    let expr = handler.make_call_expr(call_expr).indent(indent);

    let mut_modifier = |var: &OutlivedLocal| if var.mut_usage_outside_body { "mut " } else { "" };

    let mut buf = String::new();
    match fun.outliving_locals.as_slice() {
        [] => {}
        [var] => {
            format_to!(buf, "let {}{} = ", mut_modifier(var), var.local.name(ctx.db()).unwrap())
        }
        vars => {
            buf.push_str("let (");
            let bindings = vars.iter().format_with(", ", |local, f| {
                f(&format_args!("{}{}", mut_modifier(local), local.local.name(ctx.db()).unwrap()))
            });
            format_to!(buf, "{}", bindings);
            buf.push_str(") = ");
        }
    }
    format_to!(buf, "{}", expr);
    if fun.control_flow.is_async {
        buf.push_str(".await");
    }
    let insert_comma = fun
        .body
        .parent()
        .and_then(ast::MatchArm::cast)
        .map_or(false, |it| it.comma_token().is_none());
    if insert_comma {
        buf.push(',');
    } else if fun.ret_ty.is_unit() && (!fun.outliving_locals.is_empty() || !expr.is_block_like()) {
        buf.push(';');
    }
    buf
}

enum FlowHandler {
    None,
    Try { kind: TryKind },
    If { action: FlowKind },
    IfOption { action: FlowKind },
    MatchOption { none: FlowKind },
    MatchResult { err: FlowKind },
}

impl FlowHandler {
    fn from_ret_ty(fun: &Function, ret_ty: &FunType) -> FlowHandler {
        match &fun.control_flow.kind {
            None => FlowHandler::None,
            Some(flow_kind) => {
                let action = flow_kind.clone();
                if *ret_ty == FunType::Unit {
                    match flow_kind {
                        FlowKind::Return(None) | FlowKind::Break(None) | FlowKind::Continue => {
                            FlowHandler::If { action }
                        }
                        FlowKind::Return(_) | FlowKind::Break(_) => {
                            FlowHandler::IfOption { action }
                        }
                        FlowKind::Try { kind } => FlowHandler::Try { kind: kind.clone() },
                    }
                } else {
                    match flow_kind {
                        FlowKind::Return(None) | FlowKind::Break(None) | FlowKind::Continue => {
                            FlowHandler::MatchOption { none: action }
                        }
                        FlowKind::Return(_) | FlowKind::Break(_) => {
                            FlowHandler::MatchResult { err: action }
                        }
                        FlowKind::Try { kind } => FlowHandler::Try { kind: kind.clone() },
                    }
                }
            }
        }
    }

    fn make_call_expr(&self, call_expr: ast::Expr) -> ast::Expr {
        match self {
            FlowHandler::None => call_expr,
            FlowHandler::Try { kind: _ } => make::expr_try(call_expr),
            FlowHandler::If { action } => {
                let action = action.make_result_handler(None);
                let stmt = make::expr_stmt(action);
                let block = make::block_expr(iter::once(stmt.into()), None);
                let condition = make::condition(call_expr, None);
                make::expr_if(condition, block, None)
            }
            FlowHandler::IfOption { action } => {
                let path = make::ext::ident_path("Some");
                let value_pat = make::ext::simple_ident_pat(make::name("value"));
                let pattern = make::tuple_struct_pat(path, iter::once(value_pat.into()));
                let cond = make::condition(call_expr, Some(pattern.into()));
                let value = make::expr_path(make::ext::ident_path("value"));
                let action_expr = action.make_result_handler(Some(value));
                let action_stmt = make::expr_stmt(action_expr);
                let then = make::block_expr(iter::once(action_stmt.into()), None);
                make::expr_if(cond, then, None)
            }
            FlowHandler::MatchOption { none } => {
                let some_name = "value";

                let some_arm = {
                    let path = make::ext::ident_path("Some");
                    let value_pat = make::ext::simple_ident_pat(make::name(some_name));
                    let pat = make::tuple_struct_pat(path, iter::once(value_pat.into()));
                    let value = make::expr_path(make::ext::ident_path(some_name));
                    make::match_arm(iter::once(pat.into()), None, value)
                };
                let none_arm = {
                    let path = make::ext::ident_path("None");
                    let pat = make::path_pat(path);
                    make::match_arm(iter::once(pat), None, none.make_result_handler(None))
                };
                let arms = make::match_arm_list(vec![some_arm, none_arm]);
                make::expr_match(call_expr, arms)
            }
            FlowHandler::MatchResult { err } => {
                let ok_name = "value";
                let err_name = "value";

                let ok_arm = {
                    let path = make::ext::ident_path("Ok");
                    let value_pat = make::ext::simple_ident_pat(make::name(ok_name));
                    let pat = make::tuple_struct_pat(path, iter::once(value_pat.into()));
                    let value = make::expr_path(make::ext::ident_path(ok_name));
                    make::match_arm(iter::once(pat.into()), None, value)
                };
                let err_arm = {
                    let path = make::ext::ident_path("Err");
                    let value_pat = make::ext::simple_ident_pat(make::name(err_name));
                    let pat = make::tuple_struct_pat(path, iter::once(value_pat.into()));
                    let value = make::expr_path(make::ext::ident_path(err_name));
                    make::match_arm(
                        iter::once(pat.into()),
                        None,
                        err.make_result_handler(Some(value)),
                    )
                };
                let arms = make::match_arm_list(vec![ok_arm, err_arm]);
                make::expr_match(call_expr, arms)
            }
        }
    }
}

fn path_expr_from_local(ctx: &AssistContext, var: Local) -> ast::Expr {
    let name = var.name(ctx.db()).unwrap().to_string();
    make::expr_path(make::ext::ident_path(&name))
}

fn format_function(
    ctx: &AssistContext,
    module: hir::Module,
    fun: &Function,
    old_indent: IndentLevel,
    new_indent: IndentLevel,
) -> String {
    let mut fn_def = String::new();
    let params = fun.make_param_list(ctx, module);
    let ret_ty = fun.make_ret_ty(ctx, module);
    let body = make_body(ctx, old_indent, new_indent, fun);
    let const_kw = if fun.mods.is_const { "const " } else { "" };
    let async_kw = if fun.control_flow.is_async { "async " } else { "" };
    let unsafe_kw = if fun.control_flow.is_unsafe { "unsafe " } else { "" };
    match ctx.config.snippet_cap {
        Some(_) => format_to!(
            fn_def,
            "\n\n{}{}{}{}fn $0{}{}",
            new_indent,
            const_kw,
            async_kw,
            unsafe_kw,
            fun.name,
            params
        ),
        None => format_to!(
            fn_def,
            "\n\n{}{}{}{}fn {}{}",
            new_indent,
            const_kw,
            async_kw,
            unsafe_kw,
            fun.name,
            params
        ),
    }
    if let Some(ret_ty) = ret_ty {
        format_to!(fn_def, " {}", ret_ty);
    }
    format_to!(fn_def, " {}", body);

    fn_def
}

impl Function {
    fn make_param_list(&self, ctx: &AssistContext, module: hir::Module) -> ast::ParamList {
        let self_param = self.self_param.clone();
        let params = self.params.iter().map(|param| param.to_param(ctx, module));
        make::param_list(self_param, params)
    }

    fn make_ret_ty(&self, ctx: &AssistContext, module: hir::Module) -> Option<ast::RetType> {
        let fun_ty = self.return_type(ctx);
        let handler = if self.mods.is_in_tail {
            FlowHandler::None
        } else {
            FlowHandler::from_ret_ty(self, &fun_ty)
        };
        let ret_ty = match &handler {
            FlowHandler::None => {
                if matches!(fun_ty, FunType::Unit) {
                    return None;
                }
                fun_ty.make_ty(ctx, module)
            }
            FlowHandler::Try { kind: TryKind::Option } => {
                make::ext::ty_option(fun_ty.make_ty(ctx, module))
            }
            FlowHandler::Try { kind: TryKind::Result { ty: parent_ret_ty } } => {
                let handler_ty = parent_ret_ty
                    .type_arguments()
                    .nth(1)
                    .map(|ty| make_ty(&ty, ctx, module))
                    .unwrap_or_else(make::ty_unit);
                make::ext::ty_result(fun_ty.make_ty(ctx, module), handler_ty)
            }
            FlowHandler::If { .. } => make::ext::ty_bool(),
            FlowHandler::IfOption { action } => {
                let handler_ty = action
                    .expr_ty(ctx)
                    .map(|ty| make_ty(&ty, ctx, module))
                    .unwrap_or_else(make::ty_unit);
                make::ext::ty_option(handler_ty)
            }
            FlowHandler::MatchOption { .. } => make::ext::ty_option(fun_ty.make_ty(ctx, module)),
            FlowHandler::MatchResult { err } => {
                let handler_ty = err
                    .expr_ty(ctx)
                    .map(|ty| make_ty(&ty, ctx, module))
                    .unwrap_or_else(make::ty_unit);
                make::ext::ty_result(fun_ty.make_ty(ctx, module), handler_ty)
            }
        };
        Some(make::ret_type(ret_ty))
    }
}

impl FunType {
    fn make_ty(&self, ctx: &AssistContext, module: hir::Module) -> ast::Type {
        match self {
            FunType::Unit => make::ty_unit(),
            FunType::Single(ty) => make_ty(ty, ctx, module),
            FunType::Tuple(types) => match types.as_slice() {
                [] => {
                    stdx::never!("tuple type with 0 elements");
                    make::ty_unit()
                }
                [ty] => {
                    stdx::never!("tuple type with 1 element");
                    make_ty(ty, ctx, module)
                }
                types => {
                    let types = types.iter().map(|ty| make_ty(ty, ctx, module));
                    make::ty_tuple(types)
                }
            },
        }
    }
}

fn make_body(
    ctx: &AssistContext,
    old_indent: IndentLevel,
    new_indent: IndentLevel,
    fun: &Function,
) -> ast::BlockExpr {
    let ret_ty = fun.return_type(ctx);
    let handler = if fun.mods.is_in_tail {
        FlowHandler::None
    } else {
        FlowHandler::from_ret_ty(fun, &ret_ty)
    };
    let block = match &fun.body {
        FunctionBody::Expr(expr) => {
            let expr = rewrite_body_segment(ctx, &fun.params, &handler, expr.syntax());
            let expr = ast::Expr::cast(expr).unwrap();
            match expr {
                ast::Expr::BlockExpr(block) => {
                    // If the extracted expression is itself a block, there is no need to wrap it inside another block.
                    let block = block.dedent(old_indent);
                    // Recreate the block for formatting consistency with other extracted functions.
                    make::block_expr(block.statements(), block.tail_expr())
                }
                _ => {
                    let expr = expr.dedent(old_indent).indent(IndentLevel(1));

                    make::block_expr(Vec::new(), Some(expr))
                }
            }
        }
        FunctionBody::Span { parent, text_range } => {
            let mut elements: Vec<_> = parent
                .syntax()
                .children()
                .filter(|it| text_range.contains_range(it.text_range()))
                .map(|it| rewrite_body_segment(ctx, &fun.params, &handler, &it))
                .collect();

            let mut tail_expr = match elements.pop() {
                Some(node) => ast::Expr::cast(node.clone()).or_else(|| {
                    elements.push(node);
                    None
                }),
                None => None,
            };

            if tail_expr.is_none() {
                match fun.outliving_locals.as_slice() {
                    [] => {}
                    [var] => {
                        tail_expr = Some(path_expr_from_local(ctx, var.local));
                    }
                    vars => {
                        let exprs = vars.iter().map(|var| path_expr_from_local(ctx, var.local));
                        let expr = make::expr_tuple(exprs);
                        tail_expr = Some(expr);
                    }
                }
            }

            let elements = elements.into_iter().filter_map(|node| match ast::Stmt::cast(node) {
                Some(stmt) => Some(stmt),
                None => {
                    stdx::never!("block contains non-statement");
                    None
                }
            });

            let body_indent = IndentLevel(1);
            let elements = elements.map(|stmt| stmt.dedent(old_indent).indent(body_indent));
            let tail_expr = tail_expr.map(|expr| expr.dedent(old_indent).indent(body_indent));

            make::block_expr(elements, tail_expr)
        }
    };

    let block = match &handler {
        FlowHandler::None => block,
        FlowHandler::Try { kind } => {
            let block = with_default_tail_expr(block, make::expr_unit());
            map_tail_expr(block, |tail_expr| {
                let constructor = match kind {
                    TryKind::Option => "Some",
                    TryKind::Result { .. } => "Ok",
                };
                let func = make::expr_path(make::ext::ident_path(constructor));
                let args = make::arg_list(iter::once(tail_expr));
                make::expr_call(func, args)
            })
        }
        FlowHandler::If { .. } => {
            let lit_false = make::expr_literal("false");
            with_tail_expr(block, lit_false.into())
        }
        FlowHandler::IfOption { .. } => {
            let none = make::expr_path(make::ext::ident_path("None"));
            with_tail_expr(block, none)
        }
        FlowHandler::MatchOption { .. } => map_tail_expr(block, |tail_expr| {
            let some = make::expr_path(make::ext::ident_path("Some"));
            let args = make::arg_list(iter::once(tail_expr));
            make::expr_call(some, args)
        }),
        FlowHandler::MatchResult { .. } => map_tail_expr(block, |tail_expr| {
            let ok = make::expr_path(make::ext::ident_path("Ok"));
            let args = make::arg_list(iter::once(tail_expr));
            make::expr_call(ok, args)
        }),
    };

    block.indent(new_indent)
}

fn map_tail_expr(block: ast::BlockExpr, f: impl FnOnce(ast::Expr) -> ast::Expr) -> ast::BlockExpr {
    let tail_expr = match block.tail_expr() {
        Some(tail_expr) => tail_expr,
        None => return block,
    };
    make::block_expr(block.statements(), Some(f(tail_expr)))
}

fn with_default_tail_expr(block: ast::BlockExpr, tail_expr: ast::Expr) -> ast::BlockExpr {
    match block.tail_expr() {
        Some(_) => block,
        None => make::block_expr(block.statements(), Some(tail_expr)),
    }
}

fn with_tail_expr(block: ast::BlockExpr, tail_expr: ast::Expr) -> ast::BlockExpr {
    let stmt_tail = block.tail_expr().map(|expr| make::expr_stmt(expr).into());
    let stmts = block.statements().chain(stmt_tail);
    make::block_expr(stmts, Some(tail_expr))
}

fn format_type(ty: &hir::Type, ctx: &AssistContext, module: hir::Module) -> String {
    ty.display_source_code(ctx.db(), module.into()).ok().unwrap_or_else(|| "()".to_string())
}

fn make_ty(ty: &hir::Type, ctx: &AssistContext, module: hir::Module) -> ast::Type {
    let ty_str = format_type(ty, ctx, module);
    make::ty(&ty_str)
}

fn rewrite_body_segment(
    ctx: &AssistContext,
    params: &[Param],
    handler: &FlowHandler,
    syntax: &SyntaxNode,
) -> SyntaxNode {
    let syntax = fix_param_usages(ctx, params, syntax);
    update_external_control_flow(handler, &syntax);
    syntax
}

/// change all usages to account for added `&`/`&mut` for some params
fn fix_param_usages(ctx: &AssistContext, params: &[Param], syntax: &SyntaxNode) -> SyntaxNode {
    let mut usages_for_param: Vec<(&Param, Vec<ast::Expr>)> = Vec::new();

    let tm = TreeMutator::new(syntax);

    for param in params {
        if !param.kind().is_ref() {
            continue;
        }

        let usages = LocalUsages::find_local_usages(ctx, param.var);
        let usages = usages
            .iter()
            .filter(|reference| syntax.text_range().contains_range(reference.range))
            .filter_map(|reference| path_element_of_reference(syntax, reference))
            .map(|expr| tm.make_mut(&expr));

        usages_for_param.push((param, usages.collect()));
    }

    let res = tm.make_syntax_mut(syntax);

    for (param, usages) in usages_for_param {
        for usage in usages {
            match usage.syntax().ancestors().skip(1).find_map(ast::Expr::cast) {
                Some(ast::Expr::MethodCallExpr(_) | ast::Expr::FieldExpr(_)) => {
                    // do nothing
                }
                Some(ast::Expr::RefExpr(node))
                    if param.kind() == ParamKind::MutRef && node.mut_token().is_some() =>
                {
                    ted::replace(node.syntax(), node.expr().unwrap().syntax());
                }
                Some(ast::Expr::RefExpr(node))
                    if param.kind() == ParamKind::SharedRef && node.mut_token().is_none() =>
                {
                    ted::replace(node.syntax(), node.expr().unwrap().syntax());
                }
                Some(_) | None => {
                    let p = &make::expr_prefix(T![*], usage.clone()).clone_for_update();
                    ted::replace(usage.syntax(), p.syntax())
                }
            }
        }
    }

    res
}

fn update_external_control_flow(handler: &FlowHandler, syntax: &SyntaxNode) {
    let mut nested_loop = None;
    let mut nested_scope = None;
    for event in syntax.preorder() {
        match event {
            WalkEvent::Enter(e) => match e.kind() {
                SyntaxKind::LOOP_EXPR | SyntaxKind::WHILE_EXPR | SyntaxKind::FOR_EXPR => {
                    if nested_loop.is_none() {
                        nested_loop = Some(e.clone());
                    }
                }
                SyntaxKind::FN
                | SyntaxKind::CONST
                | SyntaxKind::STATIC
                | SyntaxKind::IMPL
                | SyntaxKind::MODULE => {
                    if nested_scope.is_none() {
                        nested_scope = Some(e.clone());
                    }
                }
                _ => {}
            },
            WalkEvent::Leave(e) => {
                if nested_scope.is_none() {
                    if let Some(expr) = ast::Expr::cast(e.clone()) {
                        match expr {
                            ast::Expr::ReturnExpr(return_expr) if nested_scope.is_none() => {
                                let expr = return_expr.expr();
                                if let Some(replacement) = make_rewritten_flow(handler, expr) {
                                    ted::replace(return_expr.syntax(), replacement.syntax())
                                }
                            }
                            ast::Expr::BreakExpr(break_expr) if nested_loop.is_none() => {
                                let expr = break_expr.expr();
                                if let Some(replacement) = make_rewritten_flow(handler, expr) {
                                    ted::replace(break_expr.syntax(), replacement.syntax())
                                }
                            }
                            ast::Expr::ContinueExpr(continue_expr) if nested_loop.is_none() => {
                                if let Some(replacement) = make_rewritten_flow(handler, None) {
                                    ted::replace(continue_expr.syntax(), replacement.syntax())
                                }
                            }
                            _ => {
                                // do nothing
                            }
                        }
                    }
                }

                if nested_loop.as_ref() == Some(&e) {
                    nested_loop = None;
                }
                if nested_scope.as_ref() == Some(&e) {
                    nested_scope = None;
                }
            }
        };
    }
}

fn make_rewritten_flow(handler: &FlowHandler, arg_expr: Option<ast::Expr>) -> Option<ast::Expr> {
    let value = match handler {
        FlowHandler::None | FlowHandler::Try { .. } => return None,
        FlowHandler::If { .. } => make::expr_literal("true").into(),
        FlowHandler::IfOption { .. } => {
            let expr = arg_expr.unwrap_or_else(|| make::expr_tuple(Vec::new()));
            let args = make::arg_list(iter::once(expr));
            make::expr_call(make::expr_path(make::ext::ident_path("Some")), args)
        }
        FlowHandler::MatchOption { .. } => make::expr_path(make::ext::ident_path("None")),
        FlowHandler::MatchResult { .. } => {
            let expr = arg_expr.unwrap_or_else(|| make::expr_tuple(Vec::new()));
            let args = make::arg_list(iter::once(expr));
            make::expr_call(make::expr_path(make::ext::ident_path("Err")), args)
        }
    };
    Some(make::expr_return(Some(value)).clone_for_update())
}

#[cfg(test)]
mod tests {
    use crate::tests::{check_assist, check_assist_not_applicable};

    use super::*;

    #[test]
    fn no_args_from_binary_expr() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    foo($01 + 1$0);
}
"#,
            r#"
fn foo() {
    foo(fun_name());
}

fn $0fun_name() -> i32 {
    1 + 1
}
"#,
        );
    }

    #[test]
    fn no_args_from_binary_expr_in_module() {
        check_assist(
            extract_function,
            r#"
mod bar {
    fn foo() {
        foo($01 + 1$0);
    }
}
"#,
            r#"
mod bar {
    fn foo() {
        foo(fun_name());
    }

    fn $0fun_name() -> i32 {
        1 + 1
    }
}
"#,
        );
    }

    #[test]
    fn no_args_from_binary_expr_indented() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    $0{ 1 + 1 }$0;
}
"#,
            r#"
fn foo() {
    fun_name();
}

fn $0fun_name() -> i32 {
    1 + 1
}
"#,
        );
    }

    #[test]
    fn no_args_from_stmt_with_last_expr() {
        check_assist(
            extract_function,
            r#"
fn foo() -> i32 {
    let k = 1;
    $0let m = 1;
    m + 1$0
}
"#,
            r#"
fn foo() -> i32 {
    let k = 1;
    fun_name()
}

fn $0fun_name() -> i32 {
    let m = 1;
    m + 1
}
"#,
        );
    }

    #[test]
    fn no_args_from_stmt_unit() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let k = 3;
    $0let m = 1;
    let n = m + 1;$0
    let g = 5;
}
"#,
            r#"
fn foo() {
    let k = 3;
    fun_name();
    let g = 5;
}

fn $0fun_name() {
    let m = 1;
    let n = m + 1;
}
"#,
        );
    }

    #[test]
    fn no_args_if() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    $0if true { }$0
}
"#,
            r#"
fn foo() {
    fun_name();
}

fn $0fun_name() {
    if true { }
}
"#,
        );
    }

    #[test]
    fn no_args_if_else() {
        check_assist(
            extract_function,
            r#"
fn foo() -> i32 {
    $0if true { 1 } else { 2 }$0
}
"#,
            r#"
fn foo() -> i32 {
    fun_name()
}

fn $0fun_name() -> i32 {
    if true { 1 } else { 2 }
}
"#,
        );
    }

    #[test]
    fn no_args_if_let_else() {
        check_assist(
            extract_function,
            r#"
fn foo() -> i32 {
    $0if let true = false { 1 } else { 2 }$0
}
"#,
            r#"
fn foo() -> i32 {
    fun_name()
}

fn $0fun_name() -> i32 {
    if let true = false { 1 } else { 2 }
}
"#,
        );
    }

    #[test]
    fn no_args_match() {
        check_assist(
            extract_function,
            r#"
fn foo() -> i32 {
    $0match true {
        true => 1,
        false => 2,
    }$0
}
"#,
            r#"
fn foo() -> i32 {
    fun_name()
}

fn $0fun_name() -> i32 {
    match true {
        true => 1,
        false => 2,
    }
}
"#,
        );
    }

    #[test]
    fn no_args_while() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    $0while true { }$0
}
"#,
            r#"
fn foo() {
    fun_name();
}

fn $0fun_name() {
    while true { }
}
"#,
        );
    }

    #[test]
    fn no_args_for() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    $0for v in &[0, 1] { }$0
}
"#,
            r#"
fn foo() {
    fun_name();
}

fn $0fun_name() {
    for v in &[0, 1] { }
}
"#,
        );
    }

    #[test]
    fn no_args_from_loop_unit() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    $0loop {
        let m = 1;
    }$0
}
"#,
            r#"
fn foo() {
    fun_name()
}

fn $0fun_name() -> ! {
    loop {
        let m = 1;
    }
}
"#,
        );
    }

    #[test]
    fn no_args_from_loop_with_return() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let v = $0loop {
        let m = 1;
        break m;
    }$0;
}
"#,
            r#"
fn foo() {
    let v = fun_name();
}

fn $0fun_name() -> i32 {
    loop {
        let m = 1;
        break m;
    }
}
"#,
        );
    }

    #[test]
    fn no_args_from_match() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let v: i32 = $0match Some(1) {
        Some(x) => x,
        None => 0,
    }$0;
}
"#,
            r#"
fn foo() {
    let v: i32 = fun_name();
}

fn $0fun_name() -> i32 {
    match Some(1) {
        Some(x) => x,
        None => 0,
    }
}
"#,
        );
    }

    #[test]
    fn extract_partial_block_single_line() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let n = 1;
    let mut v = $0n * n;$0
    v += 1;
}
"#,
            r#"
fn foo() {
    let n = 1;
    let mut v = fun_name(n);
    v += 1;
}

fn $0fun_name(n: i32) -> i32 {
    let mut v = n * n;
    v
}
"#,
        );
    }

    #[test]
    fn extract_partial_block() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let m = 2;
    let n = 1;
    let mut v = m $0* n;
    let mut w = 3;$0
    v += 1;
    w += 1;
}
"#,
            r#"
fn foo() {
    let m = 2;
    let n = 1;
    let (mut v, mut w) = fun_name(m, n);
    v += 1;
    w += 1;
}

fn $0fun_name(m: i32, n: i32) -> (i32, i32) {
    let mut v = m * n;
    let mut w = 3;
    (v, w)
}
"#,
        );
    }

    #[test]
    fn argument_form_expr() {
        check_assist(
            extract_function,
            r#"
fn foo() -> u32 {
    let n = 2;
    $0n+2$0
}
"#,
            r#"
fn foo() -> u32 {
    let n = 2;
    fun_name(n)
}

fn $0fun_name(n: u32) -> u32 {
    n+2
}
"#,
        )
    }

    #[test]
    fn argument_used_twice_form_expr() {
        check_assist(
            extract_function,
            r#"
fn foo() -> u32 {
    let n = 2;
    $0n+n$0
}
"#,
            r#"
fn foo() -> u32 {
    let n = 2;
    fun_name(n)
}

fn $0fun_name(n: u32) -> u32 {
    n+n
}
"#,
        )
    }

    #[test]
    fn two_arguments_form_expr() {
        check_assist(
            extract_function,
            r#"
fn foo() -> u32 {
    let n = 2;
    let m = 3;
    $0n+n*m$0
}
"#,
            r#"
fn foo() -> u32 {
    let n = 2;
    let m = 3;
    fun_name(n, m)
}

fn $0fun_name(n: u32, m: u32) -> u32 {
    n+n*m
}
"#,
        )
    }

    #[test]
    fn argument_and_locals() {
        check_assist(
            extract_function,
            r#"
fn foo() -> u32 {
    let n = 2;
    $0let m = 1;
    n + m$0
}
"#,
            r#"
fn foo() -> u32 {
    let n = 2;
    fun_name(n)
}

fn $0fun_name(n: u32) -> u32 {
    let m = 1;
    n + m
}
"#,
        )
    }

    #[test]
    fn in_comment_is_not_applicable() {
        cov_mark::check!(extract_function_in_comment_is_not_applicable);
        check_assist_not_applicable(extract_function, r"fn main() { 1 + /* $0comment$0 */ 1; }");
    }

    #[test]
    fn part_of_expr_stmt() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    $01$0 + 1;
}
"#,
            r#"
fn foo() {
    fun_name() + 1;
}

fn $0fun_name() -> i32 {
    1
}
"#,
        );
    }

    #[test]
    fn function_expr() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    $0bar(1 + 1)$0
}
"#,
            r#"
fn foo() {
    fun_name();
}

fn $0fun_name() {
    bar(1 + 1)
}
"#,
        )
    }

    #[test]
    fn extract_from_nested() {
        check_assist(
            extract_function,
            r#"
fn main() {
    let x = true;
    let tuple = match x {
        true => ($02 + 2$0, true)
        _ => (0, false)
    };
}
"#,
            r#"
fn main() {
    let x = true;
    let tuple = match x {
        true => (fun_name(), true)
        _ => (0, false)
    };
}

fn $0fun_name() -> i32 {
    2 + 2
}
"#,
        );
    }

    #[test]
    fn param_from_closure() {
        check_assist(
            extract_function,
            r#"
fn main() {
    let lambda = |x: u32| $0x * 2$0;
}
"#,
            r#"
fn main() {
    let lambda = |x: u32| fun_name(x);
}

fn $0fun_name(x: u32) -> u32 {
    x * 2
}
"#,
        );
    }

    #[test]
    fn extract_return_stmt() {
        check_assist(
            extract_function,
            r#"
fn foo() -> u32 {
    $0return 2 + 2$0;
}
"#,
            r#"
fn foo() -> u32 {
    return fun_name();
}

fn $0fun_name() -> u32 {
    2 + 2
}
"#,
        );
    }

    #[test]
    fn does_not_add_extra_whitespace() {
        check_assist(
            extract_function,
            r#"
fn foo() -> u32 {


    $0return 2 + 2$0;
}
"#,
            r#"
fn foo() -> u32 {


    return fun_name();
}

fn $0fun_name() -> u32 {
    2 + 2
}
"#,
        );
    }

    #[test]
    fn break_stmt() {
        check_assist(
            extract_function,
            r#"
fn main() {
    let result = loop {
        $0break 2 + 2$0;
    };
}
"#,
            r#"
fn main() {
    let result = loop {
        break fun_name();
    };
}

fn $0fun_name() -> i32 {
    2 + 2
}
"#,
        );
    }

    #[test]
    fn extract_cast() {
        check_assist(
            extract_function,
            r#"
fn main() {
    let v = $00f32 as u32$0;
}
"#,
            r#"
fn main() {
    let v = fun_name();
}

fn $0fun_name() -> u32 {
    0f32 as u32
}
"#,
        );
    }

    #[test]
    fn return_not_applicable() {
        check_assist_not_applicable(extract_function, r"fn foo() { $0return$0; } ");
    }

    #[test]
    fn method_to_freestanding() {
        check_assist(
            extract_function,
            r#"
struct S;

impl S {
    fn foo(&self) -> i32 {
        $01+1$0
    }
}
"#,
            r#"
struct S;

impl S {
    fn foo(&self) -> i32 {
        fun_name()
    }
}

fn $0fun_name() -> i32 {
    1+1
}
"#,
        );
    }

    #[test]
    fn method_with_reference() {
        check_assist(
            extract_function,
            r#"
struct S { f: i32 };

impl S {
    fn foo(&self) -> i32 {
        $01+self.f$0
    }
}
"#,
            r#"
struct S { f: i32 };

impl S {
    fn foo(&self) -> i32 {
        self.fun_name()
    }

    fn $0fun_name(&self) -> i32 {
        1+self.f
    }
}
"#,
        );
    }

    #[test]
    fn method_with_mut() {
        check_assist(
            extract_function,
            r#"
struct S { f: i32 };

impl S {
    fn foo(&mut self) {
        $0self.f += 1;$0
    }
}
"#,
            r#"
struct S { f: i32 };

impl S {
    fn foo(&mut self) {
        self.fun_name();
    }

    fn $0fun_name(&mut self) {
        self.f += 1;
    }
}
"#,
        );
    }

    #[test]
    fn variable_defined_inside_and_used_after_no_ret() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let n = 1;
    $0let k = n * n;$0
    let m = k + 1;
}
"#,
            r#"
fn foo() {
    let n = 1;
    let k = fun_name(n);
    let m = k + 1;
}

fn $0fun_name(n: i32) -> i32 {
    let k = n * n;
    k
}
"#,
        );
    }

    #[test]
    fn variable_defined_inside_and_used_after_mutably_no_ret() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let n = 1;
    $0let mut k = n * n;$0
    k += 1;
}
"#,
            r#"
fn foo() {
    let n = 1;
    let mut k = fun_name(n);
    k += 1;
}

fn $0fun_name(n: i32) -> i32 {
    let mut k = n * n;
    k
}
"#,
        );
    }

    #[test]
    fn two_variables_defined_inside_and_used_after_no_ret() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let n = 1;
    $0let k = n * n;
    let m = k + 2;$0
    let h = k + m;
}
"#,
            r#"
fn foo() {
    let n = 1;
    let (k, m) = fun_name(n);
    let h = k + m;
}

fn $0fun_name(n: i32) -> (i32, i32) {
    let k = n * n;
    let m = k + 2;
    (k, m)
}
"#,
        );
    }

    #[test]
    fn multi_variables_defined_inside_and_used_after_mutably_no_ret() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let n = 1;
    $0let mut k = n * n;
    let mut m = k + 2;
    let mut o = m + 3;
    o += 1;$0
    k += o;
    m = 1;
}
"#,
            r#"
fn foo() {
    let n = 1;
    let (mut k, mut m, o) = fun_name(n);
    k += o;
    m = 1;
}

fn $0fun_name(n: i32) -> (i32, i32, i32) {
    let mut k = n * n;
    let mut m = k + 2;
    let mut o = m + 3;
    o += 1;
    (k, m, o)
}
"#,
        );
    }

    #[test]
    fn nontrivial_patterns_define_variables() {
        check_assist(
            extract_function,
            r#"
struct Counter(i32);
fn foo() {
    $0let Counter(n) = Counter(0);$0
    let m = n;
}
"#,
            r#"
struct Counter(i32);
fn foo() {
    let n = fun_name();
    let m = n;
}

fn $0fun_name() -> i32 {
    let Counter(n) = Counter(0);
    n
}
"#,
        );
    }

    #[test]
    fn struct_with_two_fields_pattern_define_variables() {
        check_assist(
            extract_function,
            r#"
struct Counter { n: i32, m: i32 };
fn foo() {
    $0let Counter { n, m: k } = Counter { n: 1, m: 2 };$0
    let h = n + k;
}
"#,
            r#"
struct Counter { n: i32, m: i32 };
fn foo() {
    let (n, k) = fun_name();
    let h = n + k;
}

fn $0fun_name() -> (i32, i32) {
    let Counter { n, m: k } = Counter { n: 1, m: 2 };
    (n, k)
}
"#,
        );
    }

    #[test]
    fn mut_var_from_outer_scope() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let mut n = 1;
    $0n += 1;$0
    let m = n + 1;
}
"#,
            r#"
fn foo() {
    let mut n = 1;
    fun_name(&mut n);
    let m = n + 1;
}

fn $0fun_name(n: &mut i32) {
    *n += 1;
}
"#,
        );
    }

    #[test]
    fn mut_field_from_outer_scope() {
        check_assist(
            extract_function,
            r#"
struct C { n: i32 }
fn foo() {
    let mut c = C { n: 0 };
    $0c.n += 1;$0
    let m = c.n + 1;
}
"#,
            r#"
struct C { n: i32 }
fn foo() {
    let mut c = C { n: 0 };
    fun_name(&mut c);
    let m = c.n + 1;
}

fn $0fun_name(c: &mut C) {
    c.n += 1;
}
"#,
        );
    }

    #[test]
    fn mut_nested_field_from_outer_scope() {
        check_assist(
            extract_function,
            r#"
struct P { n: i32}
struct C { p: P }
fn foo() {
    let mut c = C { p: P { n: 0 } };
    let mut v = C { p: P { n: 0 } };
    let u = C { p: P { n: 0 } };
    $0c.p.n += u.p.n;
    let r = &mut v.p.n;$0
    let m = c.p.n + v.p.n + u.p.n;
}
"#,
            r#"
struct P { n: i32}
struct C { p: P }
fn foo() {
    let mut c = C { p: P { n: 0 } };
    let mut v = C { p: P { n: 0 } };
    let u = C { p: P { n: 0 } };
    fun_name(&mut c, &u, &mut v);
    let m = c.p.n + v.p.n + u.p.n;
}

fn $0fun_name(c: &mut C, u: &C, v: &mut C) {
    c.p.n += u.p.n;
    let r = &mut v.p.n;
}
"#,
        );
    }

    #[test]
    fn mut_param_many_usages_stmt() {
        check_assist(
            extract_function,
            r#"
fn bar(k: i32) {}
trait I: Copy {
    fn succ(&self) -> Self;
    fn inc(&mut self) -> Self { let v = self.succ(); *self = v; v }
}
impl I for i32 {
    fn succ(&self) -> Self { *self + 1 }
}
fn foo() {
    let mut n = 1;
    $0n += n;
    bar(n);
    bar(n+1);
    bar(n*n);
    bar(&n);
    n.inc();
    let v = &mut n;
    *v = v.succ();
    n.succ();$0
    let m = n + 1;
}
"#,
            r#"
fn bar(k: i32) {}
trait I: Copy {
    fn succ(&self) -> Self;
    fn inc(&mut self) -> Self { let v = self.succ(); *self = v; v }
}
impl I for i32 {
    fn succ(&self) -> Self { *self + 1 }
}
fn foo() {
    let mut n = 1;
    fun_name(&mut n);
    let m = n + 1;
}

fn $0fun_name(n: &mut i32) {
    *n += *n;
    bar(*n);
    bar(*n+1);
    bar(*n**n);
    bar(&*n);
    n.inc();
    let v = n;
    *v = v.succ();
    n.succ();
}
"#,
        );
    }

    #[test]
    fn mut_param_many_usages_expr() {
        check_assist(
            extract_function,
            r#"
fn bar(k: i32) {}
trait I: Copy {
    fn succ(&self) -> Self;
    fn inc(&mut self) -> Self { let v = self.succ(); *self = v; v }
}
impl I for i32 {
    fn succ(&self) -> Self { *self + 1 }
}
fn foo() {
    let mut n = 1;
    $0{
        n += n;
        bar(n);
        bar(n+1);
        bar(n*n);
        bar(&n);
        n.inc();
        let v = &mut n;
        *v = v.succ();
        n.succ();
    }$0
    let m = n + 1;
}
"#,
            r#"
fn bar(k: i32) {}
trait I: Copy {
    fn succ(&self) -> Self;
    fn inc(&mut self) -> Self { let v = self.succ(); *self = v; v }
}
impl I for i32 {
    fn succ(&self) -> Self { *self + 1 }
}
fn foo() {
    let mut n = 1;
    fun_name(&mut n);
    let m = n + 1;
}

fn $0fun_name(n: &mut i32) {
    *n += *n;
    bar(*n);
    bar(*n+1);
    bar(*n**n);
    bar(&*n);
    n.inc();
    let v = n;
    *v = v.succ();
    n.succ();
}
"#,
        );
    }

    #[test]
    fn mut_param_by_value() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let mut n = 1;
    $0n += 1;$0
}
"#,
            r"
fn foo() {
    let mut n = 1;
    fun_name(n);
}

fn $0fun_name(mut n: i32) {
    n += 1;
}
",
        );
    }

    #[test]
    fn mut_param_because_of_mut_ref() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let mut n = 1;
    $0let v = &mut n;
    *v += 1;$0
    let k = n;
}
"#,
            r#"
fn foo() {
    let mut n = 1;
    fun_name(&mut n);
    let k = n;
}

fn $0fun_name(n: &mut i32) {
    let v = n;
    *v += 1;
}
"#,
        );
    }

    #[test]
    fn mut_param_by_value_because_of_mut_ref() {
        check_assist(
            extract_function,
            r"
fn foo() {
    let mut n = 1;
    $0let v = &mut n;
    *v += 1;$0
}
",
            r#"
fn foo() {
    let mut n = 1;
    fun_name(n);
}

fn $0fun_name(mut n: i32) {
    let v = &mut n;
    *v += 1;
}
"#,
        );
    }

    #[test]
    fn mut_method_call() {
        check_assist(
            extract_function,
            r#"
trait I {
    fn inc(&mut self);
}
impl I for i32 {
    fn inc(&mut self) { *self += 1 }
}
fn foo() {
    let mut n = 1;
    $0n.inc();$0
}
"#,
            r#"
trait I {
    fn inc(&mut self);
}
impl I for i32 {
    fn inc(&mut self) { *self += 1 }
}
fn foo() {
    let mut n = 1;
    fun_name(n);
}

fn $0fun_name(mut n: i32) {
    n.inc();
}
"#,
        );
    }

    #[test]
    fn shared_method_call() {
        check_assist(
            extract_function,
            r#"
trait I {
    fn succ(&self);
}
impl I for i32 {
    fn succ(&self) { *self + 1 }
}
fn foo() {
    let mut n = 1;
    $0n.succ();$0
}
"#,
            r"
trait I {
    fn succ(&self);
}
impl I for i32 {
    fn succ(&self) { *self + 1 }
}
fn foo() {
    let mut n = 1;
    fun_name(n);
}

fn $0fun_name(n: i32) {
    n.succ();
}
",
        );
    }

    #[test]
    fn mut_method_call_with_other_receiver() {
        check_assist(
            extract_function,
            r#"
trait I {
    fn inc(&mut self, n: i32);
}
impl I for i32 {
    fn inc(&mut self, n: i32) { *self += n }
}
fn foo() {
    let mut n = 1;
    $0let mut m = 2;
    m.inc(n);$0
}
"#,
            r"
trait I {
    fn inc(&mut self, n: i32);
}
impl I for i32 {
    fn inc(&mut self, n: i32) { *self += n }
}
fn foo() {
    let mut n = 1;
    fun_name(n);
}

fn $0fun_name(n: i32) {
    let mut m = 2;
    m.inc(n);
}
",
        );
    }

    #[test]
    fn non_copy_without_usages_after() {
        check_assist(
            extract_function,
            r#"
struct Counter(i32);
fn foo() {
    let c = Counter(0);
    $0let n = c.0;$0
}
"#,
            r"
struct Counter(i32);
fn foo() {
    let c = Counter(0);
    fun_name(c);
}

fn $0fun_name(c: Counter) {
    let n = c.0;
}
",
        );
    }

    #[test]
    fn non_copy_used_after() {
        check_assist(
            extract_function,
            r"
struct Counter(i32);
fn foo() {
    let c = Counter(0);
    $0let n = c.0;$0
    let m = c.0;
}
",
            r#"
struct Counter(i32);
fn foo() {
    let c = Counter(0);
    fun_name(&c);
    let m = c.0;
}

fn $0fun_name(c: &Counter) {
    let n = c.0;
}
"#,
        );
    }

    #[test]
    fn copy_used_after() {
        check_assist(
            extract_function,
            r#"
//- minicore: copy
fn foo() {
    let n = 0;
    $0let m = n;$0
    let k = n;
}
"#,
            r#"
fn foo() {
    let n = 0;
    fun_name(n);
    let k = n;
}

fn $0fun_name(n: i32) {
    let m = n;
}
"#,
        )
    }

    #[test]
    fn copy_custom_used_after() {
        check_assist(
            extract_function,
            r#"
//- minicore: copy, derive
#[derive(Clone, Copy)]
struct Counter(i32);
fn foo() {
    let c = Counter(0);
    $0let n = c.0;$0
    let m = c.0;
}
"#,
            r#"
#[derive(Clone, Copy)]
struct Counter(i32);
fn foo() {
    let c = Counter(0);
    fun_name(c);
    let m = c.0;
}

fn $0fun_name(c: Counter) {
    let n = c.0;
}
"#,
        );
    }

    #[test]
    fn indented_stmts() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    if true {
        loop {
            $0let n = 1;
            let m = 2;$0
        }
    }
}
"#,
            r#"
fn foo() {
    if true {
        loop {
            fun_name();
        }
    }
}

fn $0fun_name() {
    let n = 1;
    let m = 2;
}
"#,
        );
    }

    #[test]
    fn indented_stmts_inside_mod() {
        check_assist(
            extract_function,
            r#"
mod bar {
    fn foo() {
        if true {
            loop {
                $0let n = 1;
                let m = 2;$0
            }
        }
    }
}
"#,
            r#"
mod bar {
    fn foo() {
        if true {
            loop {
                fun_name();
            }
        }
    }

    fn $0fun_name() {
        let n = 1;
        let m = 2;
    }
}
"#,
        );
    }

    #[test]
    fn break_loop() {
        check_assist(
            extract_function,
            r#"
//- minicore: option
fn foo() {
    loop {
        let n = 1;
        $0let m = n + 1;
        break;
        let k = 2;$0
        let h = 1 + k;
    }
}
"#,
            r#"
fn foo() {
    loop {
        let n = 1;
        let k = match fun_name(n) {
            Some(value) => value,
            None => break,
        };
        let h = 1 + k;
    }
}

fn $0fun_name(n: i32) -> Option<i32> {
    let m = n + 1;
    return None;
    let k = 2;
    Some(k)
}
"#,
        );
    }

    #[test]
    fn return_to_parent() {
        check_assist(
            extract_function,
            r#"
//- minicore: copy, result
fn foo() -> i64 {
    let n = 1;
    $0let m = n + 1;
    return 1;
    let k = 2;$0
    (n + k) as i64
}
"#,
            r#"
fn foo() -> i64 {
    let n = 1;
    let k = match fun_name(n) {
        Ok(value) => value,
        Err(value) => return value,
    };
    (n + k) as i64
}

fn $0fun_name(n: i32) -> Result<i32, i64> {
    let m = n + 1;
    return Err(1);
    let k = 2;
    Ok(k)
}
"#,
        );
    }

    #[test]
    fn break_and_continue() {
        cov_mark::check!(external_control_flow_break_and_continue);
        check_assist_not_applicable(
            extract_function,
            r#"
fn foo() {
    loop {
        let n = 1;
        $0let m = n + 1;
        break;
        let k = 2;
        continue;
        let k = k + 1;$0
        let r = n + k;
    }
}
"#,
        );
    }

    #[test]
    fn return_and_break() {
        cov_mark::check!(external_control_flow_return_and_bc);
        check_assist_not_applicable(
            extract_function,
            r#"
fn foo() {
    loop {
        let n = 1;
        $0let m = n + 1;
        break;
        let k = 2;
        return;
        let k = k + 1;$0
        let r = n + k;
    }
}
"#,
        );
    }

    #[test]
    fn break_loop_with_if() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    loop {
        let mut n = 1;
        $0let m = n + 1;
        break;
        n += m;$0
        let h = 1 + n;
    }
}
"#,
            r#"
fn foo() {
    loop {
        let mut n = 1;
        if fun_name(&mut n) {
            break;
        }
        let h = 1 + n;
    }
}

fn $0fun_name(n: &mut i32) -> bool {
    let m = *n + 1;
    return true;
    *n += m;
    false
}
"#,
        );
    }

    #[test]
    fn break_loop_nested() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    loop {
        let mut n = 1;
        $0let m = n + 1;
        if m == 42 {
            break;
        }$0
        let h = 1;
    }
}
"#,
            r#"
fn foo() {
    loop {
        let mut n = 1;
        if fun_name(n) {
            break;
        }
        let h = 1;
    }
}

fn $0fun_name(n: i32) -> bool {
    let m = n + 1;
    if m == 42 {
        return true;
    }
    false
}
"#,
        );
    }

    #[test]
    fn return_from_nested_loop() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    loop {
        let n = 1;$0
        let k = 1;
        loop {
            return;
        }
        let m = k + 1;$0
        let h = 1 + m;
    }
}
"#,
            r#"
fn foo() {
    loop {
        let n = 1;
        let m = match fun_name() {
            Some(value) => value,
            None => return,
        };
        let h = 1 + m;
    }
}

fn $0fun_name() -> Option<i32> {
    let k = 1;
    loop {
        return None;
    }
    let m = k + 1;
    Some(m)
}
"#,
        );
    }

    #[test]
    fn break_from_nested_loop() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    loop {
        let n = 1;
        $0let k = 1;
        loop {
            break;
        }
        let m = k + 1;$0
        let h = 1 + m;
    }
}
"#,
            r#"
fn foo() {
    loop {
        let n = 1;
        let m = fun_name();
        let h = 1 + m;
    }
}

fn $0fun_name() -> i32 {
    let k = 1;
    loop {
        break;
    }
    let m = k + 1;
    m
}
"#,
        );
    }

    #[test]
    fn break_from_nested_and_outer_loops() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    loop {
        let n = 1;
        $0let k = 1;
        loop {
            break;
        }
        if k == 42 {
            break;
        }
        let m = k + 1;$0
        let h = 1 + m;
    }
}
"#,
            r#"
fn foo() {
    loop {
        let n = 1;
        let m = match fun_name() {
            Some(value) => value,
            None => break,
        };
        let h = 1 + m;
    }
}

fn $0fun_name() -> Option<i32> {
    let k = 1;
    loop {
        break;
    }
    if k == 42 {
        return None;
    }
    let m = k + 1;
    Some(m)
}
"#,
        );
    }

    #[test]
    fn return_from_nested_fn() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    loop {
        let n = 1;
        $0let k = 1;
        fn test() {
            return;
        }
        let m = k + 1;$0
        let h = 1 + m;
    }
}
"#,
            r#"
fn foo() {
    loop {
        let n = 1;
        let m = fun_name();
        let h = 1 + m;
    }
}

fn $0fun_name() -> i32 {
    let k = 1;
    fn test() {
        return;
    }
    let m = k + 1;
    m
}
"#,
        );
    }

    #[test]
    fn break_with_value() {
        check_assist(
            extract_function,
            r#"
fn foo() -> i32 {
    loop {
        let n = 1;
        $0let k = 1;
        if k == 42 {
            break 3;
        }
        let m = k + 1;$0
        let h = 1;
    }
}
"#,
            r#"
fn foo() -> i32 {
    loop {
        let n = 1;
        if let Some(value) = fun_name() {
            break value;
        }
        let h = 1;
    }
}

fn $0fun_name() -> Option<i32> {
    let k = 1;
    if k == 42 {
        return Some(3);
    }
    let m = k + 1;
    None
}
"#,
        );
    }

    #[test]
    fn break_with_value_and_return() {
        check_assist(
            extract_function,
            r#"
fn foo() -> i64 {
    loop {
        let n = 1;$0
        let k = 1;
        if k == 42 {
            break 3;
        }
        let m = k + 1;$0
        let h = 1 + m;
    }
}
"#,
            r#"
fn foo() -> i64 {
    loop {
        let n = 1;
        let m = match fun_name() {
            Ok(value) => value,
            Err(value) => break value,
        };
        let h = 1 + m;
    }
}

fn $0fun_name() -> Result<i32, i64> {
    let k = 1;
    if k == 42 {
        return Err(3);
    }
    let m = k + 1;
    Ok(m)
}
"#,
        );
    }

    #[test]
    fn try_option() {
        check_assist(
            extract_function,
            r#"
//- minicore: option
fn bar() -> Option<i32> { None }
fn foo() -> Option<()> {
    let n = bar()?;
    $0let k = foo()?;
    let m = k + 1;$0
    let h = 1 + m;
    Some(())
}
"#,
            r#"
fn bar() -> Option<i32> { None }
fn foo() -> Option<()> {
    let n = bar()?;
    let m = fun_name()?;
    let h = 1 + m;
    Some(())
}

fn $0fun_name() -> Option<i32> {
    let k = foo()?;
    let m = k + 1;
    Some(m)
}
"#,
        );
    }

    #[test]
    fn try_option_unit() {
        check_assist(
            extract_function,
            r#"
//- minicore: option
fn foo() -> Option<()> {
    let n = 1;
    $0let k = foo()?;
    let m = k + 1;$0
    let h = 1 + n;
    Some(())
}
"#,
            r#"
fn foo() -> Option<()> {
    let n = 1;
    fun_name()?;
    let h = 1 + n;
    Some(())
}

fn $0fun_name() -> Option<()> {
    let k = foo()?;
    let m = k + 1;
    Some(())
}
"#,
        );
    }

    #[test]
    fn try_result() {
        check_assist(
            extract_function,
            r#"
//- minicore: result
fn foo() -> Result<(), i64> {
    let n = 1;
    $0let k = foo()?;
    let m = k + 1;$0
    let h = 1 + m;
    Ok(())
}
"#,
            r#"
fn foo() -> Result<(), i64> {
    let n = 1;
    let m = fun_name()?;
    let h = 1 + m;
    Ok(())
}

fn $0fun_name() -> Result<i32, i64> {
    let k = foo()?;
    let m = k + 1;
    Ok(m)
}
"#,
        );
    }

    #[test]
    fn try_option_with_return() {
        check_assist(
            extract_function,
            r#"
//- minicore: option
fn foo() -> Option<()> {
    let n = 1;
    $0let k = foo()?;
    if k == 42 {
        return None;
    }
    let m = k + 1;$0
    let h = 1 + m;
    Some(())
}
"#,
            r#"
fn foo() -> Option<()> {
    let n = 1;
    let m = fun_name()?;
    let h = 1 + m;
    Some(())
}

fn $0fun_name() -> Option<i32> {
    let k = foo()?;
    if k == 42 {
        return None;
    }
    let m = k + 1;
    Some(m)
}
"#,
        );
    }

    #[test]
    fn try_result_with_return() {
        check_assist(
            extract_function,
            r#"
//- minicore: result
fn foo() -> Result<(), i64> {
    let n = 1;
    $0let k = foo()?;
    if k == 42 {
        return Err(1);
    }
    let m = k + 1;$0
    let h = 1 + m;
    Ok(())
}
"#,
            r#"
fn foo() -> Result<(), i64> {
    let n = 1;
    let m = fun_name()?;
    let h = 1 + m;
    Ok(())
}

fn $0fun_name() -> Result<i32, i64> {
    let k = foo()?;
    if k == 42 {
        return Err(1);
    }
    let m = k + 1;
    Ok(m)
}
"#,
        );
    }

    #[test]
    fn try_and_break() {
        cov_mark::check!(external_control_flow_try_and_bc);
        check_assist_not_applicable(
            extract_function,
            r#"
//- minicore: option
fn foo() -> Option<()> {
    loop {
        let n = Some(1);
        $0let m = n? + 1;
        break;
        let k = 2;
        let k = k + 1;$0
        let r = n + k;
    }
    Some(())
}
"#,
        );
    }

    #[test]
    fn try_and_return_ok() {
        check_assist(
            extract_function,
            r#"
//- minicore: result
fn foo() -> Result<(), i64> {
    let n = 1;
    $0let k = foo()?;
    if k == 42 {
        return Ok(1);
    }
    let m = k + 1;$0
    let h = 1 + m;
    Ok(())
}
"#,
            r#"
fn foo() -> Result<(), i64> {
    let n = 1;
    let m = fun_name()?;
    let h = 1 + m;
    Ok(())
}

fn $0fun_name() -> Result<i32, i64> {
    let k = foo()?;
    if k == 42 {
        return Ok(1);
    }
    let m = k + 1;
    Ok(m)
}
"#,
        );
    }

    #[test]
    fn param_usage_in_macro() {
        check_assist(
            extract_function,
            r#"
macro_rules! m {
    ($val:expr) => { $val };
}

fn foo() {
    let n = 1;
    $0let k = n * m!(n);$0
    let m = k + 1;
}
"#,
            r#"
macro_rules! m {
    ($val:expr) => { $val };
}

fn foo() {
    let n = 1;
    let k = fun_name(n);
    let m = k + 1;
}

fn $0fun_name(n: i32) -> i32 {
    let k = n * m!(n);
    k
}
"#,
        );
    }

    #[test]
    fn extract_with_await() {
        check_assist(
            extract_function,
            r#"
fn main() {
    $0some_function().await;$0
}

async fn some_function() {

}
"#,
            r#"
fn main() {
    fun_name().await;
}

async fn $0fun_name() {
    some_function().await;
}

async fn some_function() {

}
"#,
        );
    }

    #[test]
    fn extract_with_await_in_args() {
        check_assist(
            extract_function,
            r#"
fn main() {
    $0function_call("a", some_function().await);$0
}

async fn some_function() {

}
"#,
            r#"
fn main() {
    fun_name().await;
}

async fn $0fun_name() {
    function_call("a", some_function().await);
}

async fn some_function() {

}
"#,
        );
    }

    #[test]
    fn extract_does_not_extract_standalone_blocks() {
        check_assist_not_applicable(
            extract_function,
            r#"
fn main() $0{}$0
"#,
        );
    }

    #[test]
    fn extract_adds_comma_for_match_arm() {
        check_assist(
            extract_function,
            r#"
fn main() {
    match 6 {
        100 => $0{ 100 }$0
        _ => 0,
    }
}
"#,
            r#"
fn main() {
    match 6 {
        100 => fun_name(),
        _ => 0,
    }
}

fn $0fun_name() -> i32 {
    100
}
"#,
        );
        check_assist(
            extract_function,
            r#"
fn main() {
    match 6 {
        100 => $0{ 100 }$0,
        _ => 0,
    }
}
"#,
            r#"
fn main() {
    match 6 {
        100 => fun_name(),
        _ => 0,
    }
}

fn $0fun_name() -> i32 {
    100
}
"#,
        );
    }

    #[test]
    fn extract_does_not_tear_comments_apart() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    /*$0*/
    foo();
    foo();
    /*$0*/
}
"#,
            r#"
fn foo() {
    /**/
    fun_name();
    /**/
}

fn $0fun_name() {
    foo();
    foo();
}
"#,
        );
    }

    #[test]
    fn extract_does_not_wrap_res_in_res() {
        check_assist(
            extract_function,
            r#"
//- minicore: result
fn foo() -> Result<(), i64> {
    $0Result::<i32, i64>::Ok(0)?;
    Ok(())$0
}
"#,
            r#"
fn foo() -> Result<(), i64> {
    fun_name()?
}

fn $0fun_name() -> Result<(), i64> {
    Result::<i32, i64>::Ok(0)?;
    Ok(())
}
"#,
        );
    }

    #[test]
    fn extract_knows_const() {
        check_assist(
            extract_function,
            r#"
const fn foo() {
    $0()$0
}
"#,
            r#"
const fn foo() {
    fun_name();
}

const fn $0fun_name() {
    ()
}
"#,
        );
        check_assist(
            extract_function,
            r#"
const FOO: () = {
    $0()$0
};
"#,
            r#"
const FOO: () = {
    fun_name();
};

const fn $0fun_name() {
    ()
}
"#,
        );
    }

    #[test]
    fn extract_does_not_move_outer_loop_vars() {
        check_assist(
            extract_function,
            r#"
fn foo() {
    let mut x = 5;
    for _ in 0..10 {
        $0x += 1;$0
    }
}
"#,
            r#"
fn foo() {
    let mut x = 5;
    for _ in 0..10 {
        fun_name(&mut x);
    }
}

fn $0fun_name(x: &mut i32) {
    *x += 1;
}
"#,
        );
        check_assist(
            extract_function,
            r#"
fn foo() {
    for _ in 0..10 {
        let mut x = 5;
        $0x += 1;$0
    }
}
"#,
            r#"
fn foo() {
    for _ in 0..10 {
        let mut x = 5;
        fun_name(x);
    }
}

fn $0fun_name(mut x: i32) {
    x += 1;
}
"#,
        );
        check_assist(
            extract_function,
            r#"
fn foo() {
    loop {
        let mut x = 5;
        for _ in 0..10 {
            $0x += 1;$0
        }
    }
}
"#,
            r#"
fn foo() {
    loop {
        let mut x = 5;
        for _ in 0..10 {
            fun_name(&mut x);
        }
    }
}

fn $0fun_name(x: &mut i32) {
    *x += 1;
}
"#,
        );
    }

    // regression test for #9822
    #[test]
    fn extract_mut_ref_param_has_no_mut_binding_in_loop() {
        check_assist(
            extract_function,
            r#"
struct Foo;
impl Foo {
    fn foo(&mut self) {}
}
fn foo() {
    let mut x = Foo;
    while false {
        let y = &mut x;
        $0y.foo();$0
    }
    let z = x;
}
"#,
            r#"
struct Foo;
impl Foo {
    fn foo(&mut self) {}
}
fn foo() {
    let mut x = Foo;
    while false {
        let y = &mut x;
        fun_name(y);
    }
    let z = x;
}

fn $0fun_name(y: &mut Foo) {
    y.foo();
}
"#,
        );
    }
}
