//! Patterns telling us certain facts about current syntax element, they are used in completion context
//!
//! Most logic in this module first expands the token below the cursor to a maximum node that acts similar to the token itself.
//! This means we for example expand a NameRef token to its outermost Path node, as semantically these act in the same location
//! and the completions usually query for path specific things on the Path context instead. This simplifies some location handling.

use hir::Semantics;
use ide_db::RootDatabase;
use syntax::{
    algo::non_trivia_sibling,
    ast::{self, ArgListOwner, LoopBodyOwner},
    match_ast, AstNode, Direction, SyntaxElement,
    SyntaxKind::*,
    SyntaxNode, SyntaxToken, TextRange, TextSize, T,
};

#[cfg(test)]
use crate::tests::{check_pattern_is_applicable, check_pattern_is_not_applicable};

/// Immediate previous node to what we are completing.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImmediatePrevSibling {
    IfExpr,
    TraitDefName,
    ImplDefType,
    Visibility,
    Attribute,
}

/// Direct parent "thing" of what we are currently completing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImmediateLocation {
    Use,
    UseTree,
    Impl,
    Trait,
    RecordField,
    TupleField,
    RefExpr,
    IdentPat,
    BlockExpr,
    ItemList,
    TypeBound,
    // Fake file ast node
    Attribute(ast::Attr),
    // Fake file ast node
    ModDeclaration(ast::Module),
    Visibility(ast::Visibility),
    // Original file ast node
    MethodCall {
        receiver: Option<ast::Expr>,
        has_parens: bool,
    },
    // Original file ast node
    FieldAccess {
        receiver: Option<ast::Expr>,
        receiver_is_ambiguous_float_literal: bool,
    },
    // Original file ast node
    // Only set from a type arg
    GenericArgList(ast::GenericArgList),
    // Original file ast node
    /// The record expr of the field name we are completing
    RecordExpr(ast::RecordExpr),
    // Original file ast node
    /// The record pat of the field name we are completing
    RecordPat(ast::RecordPat),
}

pub(crate) fn determine_prev_sibling(name_like: &ast::NameLike) -> Option<ImmediatePrevSibling> {
    let node = match name_like {
        ast::NameLike::NameRef(name_ref) => maximize_name_ref(name_ref),
        ast::NameLike::Name(n) => n.syntax().clone(),
        ast::NameLike::Lifetime(lt) => lt.syntax().clone(),
    };
    let node = match node.parent().and_then(ast::MacroCall::cast) {
        // When a path is being typed after the name of a trait/type of an impl it is being
        // parsed as a macro, so when the trait/impl has a block following it an we are between the
        // name and block the macro will attach the block to itself so maximizing fails to take
        // that into account
        // FIXME path expr and statement have a similar problem with attrs
        Some(call)
            if call.excl_token().is_none()
                && call.token_tree().map_or(false, |t| t.l_curly_token().is_some())
                && call.semicolon_token().is_none() =>
        {
            call.syntax().clone()
        }
        _ => node,
    };
    let prev_sibling = non_trivia_sibling(node.into(), Direction::Prev)?.into_node()?;
    if prev_sibling.kind() == ERROR {
        let prev_sibling = prev_sibling.first_child()?;
        let res = match_ast! {
            match prev_sibling {
                // vis followed by random ident will always error the parser
                ast::Visibility(_it) => ImmediatePrevSibling::Visibility,
                _ => return None,
            }
        };
        return Some(res);
    }
    let res = match_ast! {
        match prev_sibling {
            ast::ExprStmt(it) => {
                let node = it.expr().filter(|_| it.semicolon_token().is_none())?.syntax().clone();
                match_ast! {
                    match node {
                        ast::IfExpr(_it) => ImmediatePrevSibling::IfExpr,
                        _ => return None,
                    }
                }
            },
            ast::Trait(it) => if it.assoc_item_list().is_none() {
                    ImmediatePrevSibling::TraitDefName
                } else {
                    return None
            },
            ast::Impl(it) => if it.assoc_item_list().is_none()
                && (it.for_token().is_none() || it.self_ty().is_some()) {
                    ImmediatePrevSibling::ImplDefType
                } else {
                    return None
            },
            ast::Attr(_it) => ImmediatePrevSibling::Attribute,
            _ => return None,
        }
    };
    Some(res)
}

pub(crate) fn determine_location(
    sema: &Semantics<RootDatabase>,
    original_file: &SyntaxNode,
    offset: TextSize,
    name_like: &ast::NameLike,
) -> Option<ImmediateLocation> {
    let node = match name_like {
        ast::NameLike::NameRef(name_ref) => {
            if ast::RecordExprField::for_field_name(name_ref).is_some() {
                return sema
                    .find_node_at_offset_with_macros(original_file, offset)
                    .map(ImmediateLocation::RecordExpr);
            }
            if ast::RecordPatField::for_field_name_ref(name_ref).is_some() {
                return sema
                    .find_node_at_offset_with_macros(original_file, offset)
                    .map(ImmediateLocation::RecordPat);
            }
            maximize_name_ref(name_ref)
        }
        ast::NameLike::Name(name) => {
            if ast::RecordPatField::for_field_name(name).is_some() {
                return sema
                    .find_node_at_offset_with_macros(original_file, offset)
                    .map(ImmediateLocation::RecordPat);
            }
            name.syntax().clone()
        }
        ast::NameLike::Lifetime(lt) => lt.syntax().clone(),
    };

    match_ast! {
        match node {
            ast::TypeBoundList(_it) => return Some(ImmediateLocation::TypeBound),
            _ => (),
        }
    };

    let parent = match node.parent() {
        Some(parent) => match ast::MacroCall::cast(parent.clone()) {
            // When a path is being typed in an (Assoc)ItemList the parser will always emit a macro_call.
            // This is usually fine as the node expansion code above already accounts for that with
            // the ancestors call, but there is one exception to this which is that when an attribute
            // precedes it the code above will not walk the Path to the parent MacroCall as their ranges differ.
            // FIXME path expr and statement have a similar problem
            Some(call)
                if call.excl_token().is_none()
                    && call.token_tree().is_none()
                    && call.semicolon_token().is_none() =>
            {
                call.syntax().parent()?
            }
            _ => parent,
        },
        // SourceFile
        None => {
            return match node.kind() {
                MACRO_ITEMS | SOURCE_FILE => Some(ImmediateLocation::ItemList),
                _ => None,
            }
        }
    };
    let res = match_ast! {
        match parent {
            ast::IdentPat(_it) => ImmediateLocation::IdentPat,
            ast::Use(_it) => ImmediateLocation::Use,
            ast::UseTree(_it) => ImmediateLocation::UseTree,
            ast::UseTreeList(_it) => ImmediateLocation::UseTree,
            ast::BlockExpr(_it) => ImmediateLocation::BlockExpr,
            ast::SourceFile(_it) => ImmediateLocation::ItemList,
            ast::ItemList(_it) => ImmediateLocation::ItemList,
            ast::RefExpr(_it) => ImmediateLocation::RefExpr,
            ast::RecordField(it) => if it.ty().map_or(false, |it| it.syntax().text_range().contains(offset)) {
                return None;
            } else {
                ImmediateLocation::RecordField
            },
            ast::TupleField(_it) => ImmediateLocation::TupleField,
            ast::TupleFieldList(_it) => ImmediateLocation::TupleField,
            ast::TypeBound(_it) => ImmediateLocation::TypeBound,
            ast::TypeBoundList(_it) => ImmediateLocation::TypeBound,
            ast::AssocItemList(it) => match it.syntax().parent().map(|it| it.kind()) {
                Some(IMPL) => ImmediateLocation::Impl,
                Some(TRAIT) => ImmediateLocation::Trait,
                _ => return None,
            },
            ast::GenericArgList(_it) => sema
                .find_node_at_offset_with_macros(original_file, offset)
                .map(ImmediateLocation::GenericArgList)?,
            ast::Module(it) => {
                if it.item_list().is_none() {
                    ImmediateLocation::ModDeclaration(it)
                } else {
                    return None;
                }
            },
            ast::Attr(it) => ImmediateLocation::Attribute(it),
            ast::FieldExpr(it) => {
                let receiver = it
                    .expr()
                    .map(|e| e.syntax().text_range())
                    .and_then(|r| find_node_with_range(original_file, r));
                let receiver_is_ambiguous_float_literal = if let Some(ast::Expr::Literal(l)) = &receiver {
                    match l.kind() {
                        ast::LiteralKind::FloatNumber { .. } => l.token().text().ends_with('.'),
                        _ => false,
                    }
                } else {
                    false
                };
                ImmediateLocation::FieldAccess {
                    receiver,
                    receiver_is_ambiguous_float_literal,
                }
            },
            ast::MethodCallExpr(it) => ImmediateLocation::MethodCall {
                receiver: it
                    .receiver()
                    .map(|e| e.syntax().text_range())
                    .and_then(|r| find_node_with_range(original_file, r)),
                has_parens: it.arg_list().map_or(false, |it| it.l_paren_token().is_some())
            },
            ast::Visibility(it) => it.pub_token()
                .and_then(|t| (t.text_range().end() < offset).then(|| ImmediateLocation::Visibility(it)))?,
            _ => return None,
        }
    };
    Some(res)
}

fn maximize_name_ref(name_ref: &ast::NameRef) -> SyntaxNode {
    // Maximize a nameref to its enclosing path if its the last segment of said path
    if let Some(segment) = name_ref.syntax().parent().and_then(ast::PathSegment::cast) {
        let p = segment.parent_path();
        if p.parent_path().is_none() {
            if let Some(it) = p
                .syntax()
                .ancestors()
                .take_while(|it| it.text_range() == p.syntax().text_range())
                .last()
            {
                return it;
            }
        }
    }
    name_ref.syntax().clone()
}

fn find_node_with_range<N: AstNode>(syntax: &SyntaxNode, range: TextRange) -> Option<N> {
    syntax.covering_element(range).ancestors().find_map(N::cast)
}

pub(crate) fn inside_impl_trait_block(element: SyntaxElement) -> bool {
    // Here we search `impl` keyword up through the all ancestors, unlike in `has_impl_parent`,
    // where we only check the first parent with different text range.
    element
        .ancestors()
        .find(|it| it.kind() == IMPL)
        .map(|it| ast::Impl::cast(it).unwrap())
        .map(|it| it.trait_().is_some())
        .unwrap_or(false)
}
#[test]
fn test_inside_impl_trait_block() {
    check_pattern_is_applicable(r"impl Foo for Bar { f$0 }", inside_impl_trait_block);
    check_pattern_is_applicable(r"impl Foo for Bar { fn f$0 }", inside_impl_trait_block);
    check_pattern_is_not_applicable(r"impl A { f$0 }", inside_impl_trait_block);
    check_pattern_is_not_applicable(r"impl A { fn f$0 }", inside_impl_trait_block);
}

pub(crate) fn previous_token(element: SyntaxElement) -> Option<SyntaxToken> {
    element.into_token().and_then(previous_non_trivia_token)
}

/// Check if the token previous to the previous one is `for`.
/// For example, `for _ i$0` => true.
pub(crate) fn for_is_prev2(element: SyntaxElement) -> bool {
    element
        .into_token()
        .and_then(previous_non_trivia_token)
        .and_then(previous_non_trivia_token)
        .filter(|it| it.kind() == T![for])
        .is_some()
}
#[test]
fn test_for_is_prev2() {
    check_pattern_is_applicable(r"for i i$0", for_is_prev2);
}

pub(crate) fn is_in_loop_body(node: &SyntaxNode) -> bool {
    node.ancestors()
        .take_while(|it| it.kind() != FN && it.kind() != CLOSURE_EXPR)
        .find_map(|it| {
            let loop_body = match_ast! {
                match it {
                    ast::ForExpr(it) => it.loop_body(),
                    ast::WhileExpr(it) => it.loop_body(),
                    ast::LoopExpr(it) => it.loop_body(),
                    _ => None,
                }
            };
            loop_body.filter(|it| it.syntax().text_range().contains_range(node.text_range()))
        })
        .is_some()
}

fn previous_non_trivia_token(token: SyntaxToken) -> Option<SyntaxToken> {
    let mut token = token.prev_token();
    while let Some(inner) = token.clone() {
        if !inner.kind().is_trivia() {
            return Some(inner);
        } else {
            token = inner.prev_token();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use syntax::algo::find_node_at_offset;

    use crate::tests::position;

    use super::*;

    fn check_location(code: &str, loc: impl Into<Option<ImmediateLocation>>) {
        let (db, pos) = position(code);

        let sema = Semantics::new(&db);
        let original_file = sema.parse(pos.file_id);

        let name_like = find_node_at_offset(original_file.syntax(), pos.offset).unwrap();
        assert_eq!(
            determine_location(&sema, original_file.syntax(), pos.offset, &name_like),
            loc.into()
        );
    }

    fn check_prev_sibling(code: &str, sibling: impl Into<Option<ImmediatePrevSibling>>) {
        check_pattern_is_applicable(code, |e| {
            let name = &e.parent().and_then(ast::NameLike::cast).expect("Expected a namelike");
            assert_eq!(determine_prev_sibling(name), sibling.into());
            true
        });
    }

    #[test]
    fn test_trait_loc() {
        check_location(r"trait A { f$0 }", ImmediateLocation::Trait);
        check_location(r"trait A { #[attr] f$0 }", ImmediateLocation::Trait);
        check_location(r"trait A { f$0 fn f() {} }", ImmediateLocation::Trait);
        check_location(r"trait A { fn f() {} f$0 }", ImmediateLocation::Trait);
        check_location(r"trait A$0 {}", None);
        check_location(r"trait A { fn f$0 }", None);
    }

    #[test]
    fn test_impl_loc() {
        check_location(r"impl A { f$0 }", ImmediateLocation::Impl);
        check_location(r"impl A { #[attr] f$0 }", ImmediateLocation::Impl);
        check_location(r"impl A { f$0 fn f() {} }", ImmediateLocation::Impl);
        check_location(r"impl A { fn f() {} f$0 }", ImmediateLocation::Impl);
        check_location(r"impl A$0 {}", None);
        check_location(r"impl A { fn f$0 }", None);
    }

    #[test]
    fn test_use_loc() {
        check_location(r"use f$0", ImmediateLocation::Use);
        check_location(r"use f$0;", ImmediateLocation::Use);
        check_location(r"use f::{f$0}", ImmediateLocation::UseTree);
        check_location(r"use {f$0}", ImmediateLocation::UseTree);
    }

    #[test]
    fn test_record_field_loc() {
        check_location(r"struct Foo { f$0 }", ImmediateLocation::RecordField);
        check_location(r"struct Foo { f$0 pub f: i32}", ImmediateLocation::RecordField);
        check_location(r"struct Foo { pub f: i32, f$0 }", ImmediateLocation::RecordField);
    }

    #[test]
    fn test_block_expr_loc() {
        check_location(r"fn my_fn() { let a = 2; f$0 }", ImmediateLocation::BlockExpr);
        check_location(r"fn my_fn() { f$0 f }", ImmediateLocation::BlockExpr);
    }

    #[test]
    fn test_ident_pat_loc() {
        check_location(r"fn my_fn(m$0) {}", ImmediateLocation::IdentPat);
        check_location(r"fn my_fn() { let m$0 }", ImmediateLocation::IdentPat);
        check_location(r"fn my_fn(&m$0) {}", ImmediateLocation::IdentPat);
        check_location(r"fn my_fn() { let &m$0 }", ImmediateLocation::IdentPat);
    }

    #[test]
    fn test_ref_expr_loc() {
        check_location(r"fn my_fn() { let x = &m$0 foo; }", ImmediateLocation::RefExpr);
    }

    #[test]
    fn test_item_list_loc() {
        check_location(r"i$0", ImmediateLocation::ItemList);
        check_location(r"#[attr] i$0", ImmediateLocation::ItemList);
        check_location(r"fn f() {} i$0", ImmediateLocation::ItemList);
        check_location(r"mod foo { f$0 }", ImmediateLocation::ItemList);
        check_location(r"mod foo { #[attr] f$0 }", ImmediateLocation::ItemList);
        check_location(r"mod foo { fn f() {} f$0 }", ImmediateLocation::ItemList);
        check_location(r"mod foo$0 {}", None);
    }

    #[test]
    fn test_impl_prev_sibling() {
        check_prev_sibling(r"impl A w$0 ", ImmediatePrevSibling::ImplDefType);
        check_prev_sibling(r"impl A w$0 {}", ImmediatePrevSibling::ImplDefType);
        check_prev_sibling(r"impl A for A w$0 ", ImmediatePrevSibling::ImplDefType);
        check_prev_sibling(r"impl A for A w$0 {}", ImmediatePrevSibling::ImplDefType);
        check_prev_sibling(r"impl A for w$0 {}", None);
        check_prev_sibling(r"impl A for w$0", None);
    }

    #[test]
    fn test_trait_prev_sibling() {
        check_prev_sibling(r"trait A w$0 ", ImmediatePrevSibling::TraitDefName);
        check_prev_sibling(r"trait A w$0 {}", ImmediatePrevSibling::TraitDefName);
    }

    #[test]
    fn test_if_expr_prev_sibling() {
        check_prev_sibling(r"fn foo() { if true {} w$0", ImmediatePrevSibling::IfExpr);
        check_prev_sibling(r"fn foo() { if true {}; w$0", None);
    }

    #[test]
    fn test_vis_prev_sibling() {
        check_prev_sibling(r"pub w$0", ImmediatePrevSibling::Visibility);
    }

    #[test]
    fn test_attr_prev_sibling() {
        check_prev_sibling(r"#[attr] w$0", ImmediatePrevSibling::Attribute);
    }
}
