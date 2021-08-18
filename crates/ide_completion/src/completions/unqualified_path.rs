//! Completion of names from the current scope, e.g. locals and imported items.

use hir::ScopeDef;
use syntax::{ast, AstNode};

use crate::{patterns::ImmediateLocation, CompletionContext, Completions};

pub(crate) fn complete_unqualified_path(acc: &mut Completions, ctx: &CompletionContext) {
    if ctx.is_path_disallowed() || !ctx.is_trivial_path() || ctx.has_impl_or_trait_prev_sibling() {
        return;
    }

    if ctx.in_use_tree() {
        // only show modules in a fresh UseTree
        cov_mark::hit!(unqualified_path_only_modules_in_import);
        ctx.process_all_names(&mut |name, res| {
            if let ScopeDef::ModuleDef(hir::ModuleDef::Module(_)) = res {
                acc.add_resolution(ctx, name, &res);
            }
        });

        std::array::IntoIter::new(["self::", "super::", "crate::"])
            .for_each(|kw| acc.add_keyword(ctx, kw));
        return;
    }
    std::array::IntoIter::new(["self", "super", "crate"]).for_each(|kw| acc.add_keyword(ctx, kw));

    match &ctx.completion_location {
        Some(ImmediateLocation::Visibility(_)) => return,
        Some(ImmediateLocation::ItemList | ImmediateLocation::Trait | ImmediateLocation::Impl) => {
            // only show macros in {Assoc}ItemList
            ctx.process_all_names(&mut |name, res| {
                if let hir::ScopeDef::MacroDef(mac) = res {
                    if mac.is_fn_like() {
                        acc.add_macro(ctx, Some(name.clone()), mac);
                    }
                }
                if let hir::ScopeDef::ModuleDef(hir::ModuleDef::Module(_)) = res {
                    acc.add_resolution(ctx, name, &res);
                }
            });
            return;
        }
        Some(ImmediateLocation::TypeBound) => {
            ctx.process_all_names(&mut |name, res| {
                let add_resolution = match res {
                    ScopeDef::MacroDef(mac) => mac.is_fn_like(),
                    ScopeDef::ModuleDef(hir::ModuleDef::Trait(_) | hir::ModuleDef::Module(_)) => {
                        true
                    }
                    _ => false,
                };
                if add_resolution {
                    acc.add_resolution(ctx, name, &res);
                }
            });
            return;
        }
        _ => (),
    }

    if !ctx.expects_type() {
        if let Some(hir::Adt::Enum(e)) =
            ctx.expected_type.as_ref().and_then(|ty| ty.strip_references().as_adt())
        {
            super::enum_variants_with_paths(acc, ctx, e, |acc, ctx, variant, path| {
                acc.add_qualified_enum_variant(ctx, variant, path)
            });
        }
    }

    if let Some(ImmediateLocation::GenericArgList(arg_list)) = &ctx.completion_location {
        if let Some(path_seg) = arg_list.syntax().parent().and_then(ast::PathSegment::cast) {
            if let Some(hir::PathResolution::Def(hir::ModuleDef::Trait(trait_))) =
                ctx.sema.resolve_path(&path_seg.parent_path())
            {
                trait_.items(ctx.sema.db).into_iter().for_each(|it| {
                    if let hir::AssocItem::TypeAlias(alias) = it {
                        acc.add_type_alias_with_eq(ctx, alias)
                    }
                });
            }
        }
    }

    ctx.process_all_names(&mut |name, res| {
        let add_resolution = match res {
            ScopeDef::GenericParam(hir::GenericParam::LifetimeParam(_)) | ScopeDef::Label(_) => {
                cov_mark::hit!(unqualified_skip_lifetime_completion);
                return;
            }
            ScopeDef::ImplSelfType(_) => {
                !ctx.previous_token_is(syntax::T![impl]) && !ctx.previous_token_is(syntax::T![for])
            }
            // Don't suggest attribute macros and derives.
            ScopeDef::MacroDef(mac) => mac.is_fn_like(),
            // no values in type places
            ScopeDef::ModuleDef(
                hir::ModuleDef::Function(_)
                | hir::ModuleDef::Variant(_)
                | hir::ModuleDef::Static(_),
            )
            | ScopeDef::Local(_) => !ctx.expects_type(),
            // unless its a constant in a generic arg list position
            ScopeDef::ModuleDef(hir::ModuleDef::Const(_))
            | ScopeDef::GenericParam(hir::GenericParam::ConstParam(_)) => {
                !ctx.expects_type() || ctx.expects_generic_arg()
            }
            _ => true,
        };
        if add_resolution {
            acc.add_resolution(ctx, name, &res);
        }
    });
}

#[cfg(test)]
mod tests {
    use expect_test::{expect, Expect};

    use crate::{
        tests::{check_edit, filtered_completion_list_with_config, TEST_CONFIG},
        CompletionConfig, CompletionKind,
    };

    fn check(ra_fixture: &str, expect: Expect) {
        check_with_config(TEST_CONFIG, ra_fixture, expect);
    }

    fn check_with_config(config: CompletionConfig, ra_fixture: &str, expect: Expect) {
        let actual =
            filtered_completion_list_with_config(config, ra_fixture, CompletionKind::Reference);
        expect.assert_eq(&actual)
    }

    #[test]
    fn completes_if_prefix_is_keyword() {
        cov_mark::check!(completes_if_prefix_is_keyword);
        check_edit(
            "wherewolf",
            r#"
fn main() {
    let wherewolf = 92;
    drop(where$0)
}
"#,
            r#"
fn main() {
    let wherewolf = 92;
    drop(wherewolf)
}
"#,
        )
    }

    /// Regression test for issue #6091.
    #[test]
    fn correctly_completes_module_items_prefixed_with_underscore() {
        check_edit(
            "_alpha",
            r#"
fn main() {
    _$0
}
fn _alpha() {}
"#,
            r#"
fn main() {
    _alpha()$0
}
fn _alpha() {}
"#,
        )
    }

    #[test]
    fn completes_prelude() {
        check(
            r#"
//- /main.rs crate:main deps:std
fn foo() { let x: $0 }

//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2018 {
        pub struct Option;
    }
}
"#,
            expect![[r#"
                md std
                st Option
            "#]],
        );
    }

    #[test]
    fn completes_prelude_macros() {
        check(
            r#"
//- /main.rs crate:main deps:std
fn f() {$0}

//- /std/lib.rs crate:std
pub mod prelude {
    pub mod rust_2018 {
        pub use crate::concat;
    }
}

mod macros {
    #[rustc_builtin_macro]
    #[macro_export]
    macro_rules! concat { }
}
"#,
            expect![[r##"
                fn f()        fn()
                ma concat!(…) #[macro_export] macro_rules! concat
                md std
            "##]],
        );
    }

    #[test]
    fn completes_std_prelude_if_core_is_defined() {
        check(
            r#"
//- /main.rs crate:main deps:core,std
fn foo() { let x: $0 }

//- /core/lib.rs crate:core
pub mod prelude {
    pub mod rust_2018 {
        pub struct Option;
    }
}

//- /std/lib.rs crate:std deps:core
pub mod prelude {
    pub mod rust_2018 {
        pub struct String;
    }
}
"#,
            expect![[r#"
                md std
                md core
                st String
            "#]],
        );
    }

    #[test]
    fn respects_doc_hidden() {
        check(
            r#"
//- /lib.rs crate:lib deps:std
fn f() {
    format_$0
}

//- /std.rs crate:std
#[doc(hidden)]
#[macro_export]
macro_rules! format_args_nl {
    () => {}
}

pub mod prelude {
    pub mod rust_2018 {}
}
            "#,
            expect![[r#"
                fn f() fn()
                md std
            "#]],
        );
    }

    #[test]
    fn respects_doc_hidden_in_assoc_item_list() {
        check(
            r#"
//- /lib.rs crate:lib deps:std
struct S;
impl S {
    format_$0
}

//- /std.rs crate:std
#[doc(hidden)]
#[macro_export]
macro_rules! format_args_nl {
    () => {}
}

pub mod prelude {
    pub mod rust_2018 {}
}
            "#,
            expect![[r#"
                md std
            "#]],
        );
    }
}
