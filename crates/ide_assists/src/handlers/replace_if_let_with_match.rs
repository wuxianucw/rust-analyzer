use std::iter::{self, successors};

use either::Either;
use ide_db::{ty_filter::TryEnum, RootDatabase};
use syntax::{
    ast::{
        self,
        edit::{AstNodeEdit, IndentLevel},
        make,
    },
    AstNode,
};

use crate::{
    utils::{does_pat_match_variant, unwrap_trivial_block},
    AssistContext, AssistId, AssistKind, Assists,
};

// Assist: replace_if_let_with_match
//
// Replaces a `if let` expression with a `match` expression.
//
// ```
// enum Action { Move { distance: u32 }, Stop }
//
// fn handle(action: Action) {
//     $0if let Action::Move { distance } = action {
//         foo(distance)
//     } else {
//         bar()
//     }
// }
// ```
// ->
// ```
// enum Action { Move { distance: u32 }, Stop }
//
// fn handle(action: Action) {
//     match action {
//         Action::Move { distance } => foo(distance),
//         _ => bar(),
//     }
// }
// ```
pub(crate) fn replace_if_let_with_match(acc: &mut Assists, ctx: &AssistContext) -> Option<()> {
    let if_expr: ast::IfExpr = ctx.find_node_at_offset()?;
    let mut else_block = None;
    let if_exprs = successors(Some(if_expr.clone()), |expr| match expr.else_branch()? {
        ast::ElseBranch::IfExpr(expr) => Some(expr),
        ast::ElseBranch::Block(block) => {
            else_block = Some(block);
            None
        }
    });
    let scrutinee_to_be_expr = if_expr.condition()?.expr()?;

    let mut pat_seen = false;
    let mut cond_bodies = Vec::new();
    for if_expr in if_exprs {
        let cond = if_expr.condition()?;
        let expr = cond.expr()?;
        let cond = match cond.pat() {
            Some(pat) => {
                if scrutinee_to_be_expr.syntax().text() != expr.syntax().text() {
                    // Only if all condition expressions are equal we can merge them into a match
                    return None;
                }
                pat_seen = true;
                Either::Left(pat)
            }
            None => Either::Right(expr),
        };
        let body = if_expr.then_branch()?;
        cond_bodies.push((cond, body));
    }

    if !pat_seen {
        // Don't offer turning an if (chain) without patterns into a match
        return None;
    }

    let target = if_expr.syntax().text_range();
    acc.add(
        AssistId("replace_if_let_with_match", AssistKind::RefactorRewrite),
        "Replace if let with match",
        target,
        move |edit| {
            let match_expr = {
                let else_arm = make_else_arm(else_block, &cond_bodies, ctx);
                let make_match_arm = |(pat, body): (_, ast::BlockExpr)| {
                    let body = body.reset_indent().indent(IndentLevel(1));
                    match pat {
                        Either::Left(pat) => {
                            make::match_arm(iter::once(pat), None, unwrap_trivial_block(body))
                        }
                        Either::Right(expr) => make::match_arm(
                            iter::once(make::wildcard_pat().into()),
                            Some(expr),
                            unwrap_trivial_block(body),
                        ),
                    }
                };
                let arms = cond_bodies.into_iter().map(make_match_arm).chain(iter::once(else_arm));
                let match_expr = make::expr_match(scrutinee_to_be_expr, make::match_arm_list(arms));
                match_expr.indent(IndentLevel::from_node(if_expr.syntax()))
            };

            let has_preceding_if_expr =
                if_expr.syntax().parent().map_or(false, |it| ast::IfExpr::can_cast(it.kind()));
            let expr = if has_preceding_if_expr {
                // make sure we replace the `else if let ...` with a block so we don't end up with `else expr`
                make::block_expr(None, Some(match_expr)).into()
            } else {
                match_expr
            };
            edit.replace_ast::<ast::Expr>(if_expr.into(), expr);
        },
    )
}

fn make_else_arm(
    else_block: Option<ast::BlockExpr>,
    cond_bodies: &Vec<(Either<ast::Pat, ast::Expr>, ast::BlockExpr)>,
    ctx: &AssistContext,
) -> ast::MatchArm {
    if let Some(else_block) = else_block {
        let pattern = if let [(Either::Left(pat), _)] = &**cond_bodies {
            ctx.sema
                .type_of_pat(&pat)
                .and_then(|ty| TryEnum::from_ty(&ctx.sema, &ty))
                .zip(Some(pat))
        } else {
            None
        };
        let pattern = match pattern {
            Some((it, pat)) => {
                if does_pat_match_variant(&pat, &it.sad_pattern()) {
                    it.happy_pattern()
                } else {
                    it.sad_pattern()
                }
            }
            None => make::wildcard_pat().into(),
        };
        make::match_arm(iter::once(pattern), None, unwrap_trivial_block(else_block))
    } else {
        make::match_arm(iter::once(make::wildcard_pat().into()), None, make::expr_unit().into())
    }
}

// Assist: replace_match_with_if_let
//
// Replaces a binary `match` with a wildcard pattern and no guards with an `if let` expression.
//
// ```
// enum Action { Move { distance: u32 }, Stop }
//
// fn handle(action: Action) {
//     $0match action {
//         Action::Move { distance } => foo(distance),
//         _ => bar(),
//     }
// }
// ```
// ->
// ```
// enum Action { Move { distance: u32 }, Stop }
//
// fn handle(action: Action) {
//     if let Action::Move { distance } = action {
//         foo(distance)
//     } else {
//         bar()
//     }
// }
// ```
pub(crate) fn replace_match_with_if_let(acc: &mut Assists, ctx: &AssistContext) -> Option<()> {
    let match_expr: ast::MatchExpr = ctx.find_node_at_offset()?;

    let mut arms = match_expr.match_arm_list()?.arms();
    let (first_arm, second_arm) = (arms.next()?, arms.next()?);
    if arms.next().is_some() || first_arm.guard().is_some() || second_arm.guard().is_some() {
        return None;
    }

    let (if_let_pat, then_expr, else_expr) = pick_pattern_and_expr_order(
        &ctx.sema,
        first_arm.pat()?,
        second_arm.pat()?,
        first_arm.expr()?,
        second_arm.expr()?,
    )?;
    let scrutinee = match_expr.expr()?;

    let target = match_expr.syntax().text_range();
    acc.add(
        AssistId("replace_match_with_if_let", AssistKind::RefactorRewrite),
        "Replace match with if let",
        target,
        move |edit| {
            let condition = make::condition(scrutinee, Some(if_let_pat));
            let then_block = match then_expr.reset_indent() {
                ast::Expr::BlockExpr(block) => block,
                expr => make::block_expr(iter::empty(), Some(expr)),
            };
            let else_expr = match else_expr {
                ast::Expr::BlockExpr(block) if block.is_empty() => None,
                ast::Expr::TupleExpr(tuple) if tuple.fields().next().is_none() => None,
                expr => Some(expr),
            };
            let if_let_expr = make::expr_if(
                condition,
                then_block,
                else_expr
                    .map(|expr| match expr {
                        ast::Expr::BlockExpr(block) => block,
                        expr => (make::block_expr(iter::empty(), Some(expr))),
                    })
                    .map(ast::ElseBranch::Block),
            )
            .indent(IndentLevel::from_node(match_expr.syntax()));

            edit.replace_ast::<ast::Expr>(match_expr.into(), if_let_expr);
        },
    )
}

/// Pick the pattern for the if let condition and return the expressions for the `then` body and `else` body in that order.
fn pick_pattern_and_expr_order(
    sema: &hir::Semantics<RootDatabase>,
    pat: ast::Pat,
    pat2: ast::Pat,
    expr: ast::Expr,
    expr2: ast::Expr,
) -> Option<(ast::Pat, ast::Expr, ast::Expr)> {
    let res = match (pat, pat2) {
        (ast::Pat::WildcardPat(_), _) => return None,
        (pat, sad_pat) if is_sad_pat(sema, &sad_pat) => (pat, expr, expr2),
        (sad_pat, pat) if is_sad_pat(sema, &sad_pat) => (pat, expr2, expr),
        (pat, pat2) => match (binds_name(&pat), binds_name(&pat2)) {
            (true, true) => return None,
            (true, false) => (pat, expr, expr2),
            (false, true) => (pat2, expr2, expr),
            (false, false) => (pat, expr, expr2),
        },
    };
    Some(res)
}

fn binds_name(pat: &ast::Pat) -> bool {
    let binds_name_v = |pat| binds_name(&pat);
    match pat {
        ast::Pat::IdentPat(_) => true,
        ast::Pat::MacroPat(_) => true,
        ast::Pat::OrPat(pat) => pat.pats().any(binds_name_v),
        ast::Pat::SlicePat(pat) => pat.pats().any(binds_name_v),
        ast::Pat::TuplePat(it) => it.fields().any(binds_name_v),
        ast::Pat::TupleStructPat(it) => it.fields().any(binds_name_v),
        ast::Pat::RecordPat(it) => it
            .record_pat_field_list()
            .map_or(false, |rpfl| rpfl.fields().flat_map(|rpf| rpf.pat()).any(binds_name_v)),
        ast::Pat::RefPat(pat) => pat.pat().map_or(false, binds_name_v),
        ast::Pat::BoxPat(pat) => pat.pat().map_or(false, binds_name_v),
        ast::Pat::ParenPat(pat) => pat.pat().map_or(false, binds_name_v),
        _ => false,
    }
}

fn is_sad_pat(sema: &hir::Semantics<RootDatabase>, pat: &ast::Pat) -> bool {
    sema.type_of_pat(pat)
        .and_then(|ty| TryEnum::from_ty(sema, &ty))
        .map_or(false, |it| does_pat_match_variant(pat, &it.sad_pattern()))
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::tests::{check_assist, check_assist_not_applicable, check_assist_target};

    #[test]
    fn test_if_let_with_match_unapplicable_for_simple_ifs() {
        check_assist_not_applicable(
            replace_if_let_with_match,
            r#"
fn main() {
    if $0true {} else if false {} else {}
}
"#,
        )
    }

    #[test]
    fn test_if_let_with_match_no_else() {
        check_assist(
            replace_if_let_with_match,
            r#"
impl VariantData {
    pub fn foo(&self) {
        if $0let VariantData::Struct(..) = *self {
            self.foo();
        }
    }
}
"#,
            r#"
impl VariantData {
    pub fn foo(&self) {
        match *self {
            VariantData::Struct(..) => {
                self.foo();
            }
            _ => (),
        }
    }
}
"#,
        )
    }

    #[test]
    fn test_if_let_with_match_basic() {
        check_assist(
            replace_if_let_with_match,
            r#"
impl VariantData {
    pub fn is_struct(&self) -> bool {
        if $0let VariantData::Struct(..) = *self {
            true
        } else if let VariantData::Tuple(..) = *self {
            false
        } else if cond() {
            true
        } else {
            bar(
                123
            )
        }
    }
}
"#,
            r#"
impl VariantData {
    pub fn is_struct(&self) -> bool {
        match *self {
            VariantData::Struct(..) => true,
            VariantData::Tuple(..) => false,
            _ if cond() => true,
            _ => {
                    bar(
                        123
                    )
                }
        }
    }
}
"#,
        )
    }

    #[test]
    fn test_if_let_with_match_on_tail_if_let() {
        check_assist(
            replace_if_let_with_match,
            r#"
impl VariantData {
    pub fn is_struct(&self) -> bool {
        if let VariantData::Struct(..) = *self {
            true
        } else if let$0 VariantData::Tuple(..) = *self {
            false
        } else {
            false
        }
    }
}
"#,
            r#"
impl VariantData {
    pub fn is_struct(&self) -> bool {
        if let VariantData::Struct(..) = *self {
            true
        } else {
    match *self {
            VariantData::Tuple(..) => false,
            _ => false,
        }
}
    }
}
"#,
        )
    }

    #[test]
    fn special_case_option() {
        check_assist(
            replace_if_let_with_match,
            r#"
//- minicore: option
fn foo(x: Option<i32>) {
    $0if let Some(x) = x {
        println!("{}", x)
    } else {
        println!("none")
    }
}
"#,
            r#"
fn foo(x: Option<i32>) {
    match x {
        Some(x) => println!("{}", x),
        None => println!("none"),
    }
}
"#,
        );
    }

    #[test]
    fn special_case_inverted_option() {
        check_assist(
            replace_if_let_with_match,
            r#"
//- minicore: option
fn foo(x: Option<i32>) {
    $0if let None = x {
        println!("none")
    } else {
        println!("some")
    }
}
"#,
            r#"
fn foo(x: Option<i32>) {
    match x {
        None => println!("none"),
        Some(_) => println!("some"),
    }
}
"#,
        );
    }

    #[test]
    fn special_case_result() {
        check_assist(
            replace_if_let_with_match,
            r#"
//- minicore: result
fn foo(x: Result<i32, ()>) {
    $0if let Ok(x) = x {
        println!("{}", x)
    } else {
        println!("none")
    }
}
"#,
            r#"
fn foo(x: Result<i32, ()>) {
    match x {
        Ok(x) => println!("{}", x),
        Err(_) => println!("none"),
    }
}
"#,
        );
    }

    #[test]
    fn special_case_inverted_result() {
        check_assist(
            replace_if_let_with_match,
            r#"
//- minicore: result
fn foo(x: Result<i32, ()>) {
    $0if let Err(x) = x {
        println!("{}", x)
    } else {
        println!("ok")
    }
}
"#,
            r#"
fn foo(x: Result<i32, ()>) {
    match x {
        Err(x) => println!("{}", x),
        Ok(_) => println!("ok"),
    }
}
"#,
        );
    }

    #[test]
    fn nested_indent() {
        check_assist(
            replace_if_let_with_match,
            r#"
fn main() {
    if true {
        $0if let Ok(rel_path) = path.strip_prefix(root_path) {
            let rel_path = RelativePathBuf::from_path(rel_path).ok()?;
            Some((*id, rel_path))
        } else {
            None
        }
    }
}
"#,
            r#"
fn main() {
    if true {
        match path.strip_prefix(root_path) {
            Ok(rel_path) => {
                let rel_path = RelativePathBuf::from_path(rel_path).ok()?;
                Some((*id, rel_path))
            }
            _ => None,
        }
    }
}
"#,
        )
    }

    #[test]
    fn test_replace_match_with_if_let_unwraps_simple_expressions() {
        check_assist(
            replace_match_with_if_let,
            r#"
impl VariantData {
    pub fn is_struct(&self) -> bool {
        $0match *self {
            VariantData::Struct(..) => true,
            _ => false,
        }
    }
}           "#,
            r#"
impl VariantData {
    pub fn is_struct(&self) -> bool {
        if let VariantData::Struct(..) = *self {
            true
        } else {
            false
        }
    }
}           "#,
        )
    }

    #[test]
    fn test_replace_match_with_if_let_doesnt_unwrap_multiline_expressions() {
        check_assist(
            replace_match_with_if_let,
            r#"
fn foo() {
    $0match a {
        VariantData::Struct(..) => {
            bar(
                123
            )
        }
        _ => false,
    }
}           "#,
            r#"
fn foo() {
    if let VariantData::Struct(..) = a {
        bar(
            123
        )
    } else {
        false
    }
}           "#,
        )
    }

    #[test]
    fn replace_match_with_if_let_target() {
        check_assist_target(
            replace_match_with_if_let,
            r#"
impl VariantData {
    pub fn is_struct(&self) -> bool {
        $0match *self {
            VariantData::Struct(..) => true,
            _ => false,
        }
    }
}           "#,
            r#"match *self {
            VariantData::Struct(..) => true,
            _ => false,
        }"#,
        );
    }

    #[test]
    fn special_case_option_match_to_if_let() {
        check_assist(
            replace_match_with_if_let,
            r#"
//- minicore: option
fn foo(x: Option<i32>) {
    $0match x {
        Some(x) => println!("{}", x),
        None => println!("none"),
    }
}
"#,
            r#"
fn foo(x: Option<i32>) {
    if let Some(x) = x {
        println!("{}", x)
    } else {
        println!("none")
    }
}
"#,
        );
    }

    #[test]
    fn special_case_result_match_to_if_let() {
        check_assist(
            replace_match_with_if_let,
            r#"
//- minicore: result
fn foo(x: Result<i32, ()>) {
    $0match x {
        Ok(x) => println!("{}", x),
        Err(_) => println!("none"),
    }
}
"#,
            r#"
fn foo(x: Result<i32, ()>) {
    if let Ok(x) = x {
        println!("{}", x)
    } else {
        println!("none")
    }
}
"#,
        );
    }

    #[test]
    fn nested_indent_match_to_if_let() {
        check_assist(
            replace_match_with_if_let,
            r#"
fn main() {
    if true {
        $0match path.strip_prefix(root_path) {
            Ok(rel_path) => {
                let rel_path = RelativePathBuf::from_path(rel_path).ok()?;
                Some((*id, rel_path))
            }
            _ => None,
        }
    }
}
"#,
            r#"
fn main() {
    if true {
        if let Ok(rel_path) = path.strip_prefix(root_path) {
            let rel_path = RelativePathBuf::from_path(rel_path).ok()?;
            Some((*id, rel_path))
        } else {
            None
        }
    }
}
"#,
        )
    }

    #[test]
    fn replace_match_with_if_let_empty_wildcard_expr() {
        check_assist(
            replace_match_with_if_let,
            r#"
fn main() {
    $0match path.strip_prefix(root_path) {
        Ok(rel_path) => println!("{}", rel_path),
        _ => (),
    }
}
"#,
            r#"
fn main() {
    if let Ok(rel_path) = path.strip_prefix(root_path) {
        println!("{}", rel_path)
    }
}
"#,
        )
    }

    #[test]
    fn replace_match_with_if_let_exhaustive() {
        check_assist(
            replace_match_with_if_let,
            r#"
fn print_source(def_source: ModuleSource) {
    match def_so$0urce {
        ModuleSource::SourceFile(..) => { println!("source file"); }
        ModuleSource::Module(..) => { println!("module"); }
    }
}
"#,
            r#"
fn print_source(def_source: ModuleSource) {
    if let ModuleSource::SourceFile(..) = def_source { println!("source file"); } else { println!("module"); }
}
"#,
        )
    }

    #[test]
    fn replace_match_with_if_let_prefer_name_bind() {
        check_assist(
            replace_match_with_if_let,
            r#"
fn foo() {
    match $0Foo(0) {
        Foo(_) => (),
        Bar(bar) => println!("bar {}", bar),
    }
}
"#,
            r#"
fn foo() {
    if let Bar(bar) = Foo(0) {
        println!("bar {}", bar)
    }
}
"#,
        );
        check_assist(
            replace_match_with_if_let,
            r#"
fn foo() {
    match $0Foo(0) {
        Bar(bar) => println!("bar {}", bar),
        Foo(_) => (),
    }
}
"#,
            r#"
fn foo() {
    if let Bar(bar) = Foo(0) {
        println!("bar {}", bar)
    }
}
"#,
        );
    }

    #[test]
    fn replace_match_with_if_let_rejects_double_name_bindings() {
        check_assist_not_applicable(
            replace_match_with_if_let,
            r#"
fn foo() {
    match $0Foo(0) {
        Foo(foo) => println!("bar {}", foo),
        Bar(bar) => println!("bar {}", bar),
    }
}
"#,
        );
    }
}
