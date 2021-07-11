//! Completes references after dot (fields and method calls).

use either::Either;
use hir::{HasVisibility, ScopeDef};
use rustc_hash::FxHashSet;

use crate::{context::CompletionContext, patterns::ImmediateLocation, Completions};

/// Complete dot accesses, i.e. fields or methods.
pub(crate) fn complete_dot(acc: &mut Completions, ctx: &CompletionContext) {
    let dot_receiver = match ctx.dot_receiver() {
        Some(expr) => expr,
        _ => return complete_undotted_self(acc, ctx),
    };

    let receiver_ty = match ctx.sema.type_of_expr(dot_receiver) {
        Some(ty) => ty,
        _ => return,
    };

    if matches!(ctx.completion_location, Some(ImmediateLocation::MethodCall { .. })) {
        cov_mark::hit!(test_no_struct_field_completion_for_method_call);
    } else {
        complete_fields(ctx, &receiver_ty, |field, ty| match field {
            Either::Left(field) => acc.add_field(ctx, None, field, &ty),
            Either::Right(tuple_idx) => acc.add_tuple_field(ctx, None, tuple_idx, &ty),
        });
    }
    complete_methods(ctx, &receiver_ty, |func| acc.add_method(ctx, func, None, None));
}

fn complete_undotted_self(acc: &mut Completions, ctx: &CompletionContext) {
    if !ctx.config.enable_self_on_the_fly {
        return;
    }
    if !ctx.is_trivial_path() || ctx.is_path_disallowed() || !ctx.expects_expression() {
        return;
    }
    ctx.scope.process_all_names(&mut |name, def| {
        if let ScopeDef::Local(local) = &def {
            if local.is_self(ctx.db) {
                let ty = local.ty(ctx.db);
                complete_fields(ctx, &ty, |field, ty| match field {
                    either::Either::Left(field) => {
                        acc.add_field(ctx, Some(name.clone()), field, &ty)
                    }
                    either::Either::Right(tuple_idx) => {
                        acc.add_tuple_field(ctx, Some(name.clone()), tuple_idx, &ty)
                    }
                });
                complete_methods(ctx, &ty, |func| {
                    acc.add_method(ctx, func, Some(name.clone()), None)
                });
            }
        }
    });
}

fn complete_fields(
    ctx: &CompletionContext,
    receiver: &hir::Type,
    mut f: impl FnMut(Either<hir::Field, usize>, hir::Type),
) {
    for receiver in receiver.autoderef(ctx.db) {
        for (field, ty) in receiver.fields(ctx.db) {
            if ctx.scope.module().map_or(false, |m| !field.is_visible_from(ctx.db, m)) {
                // Skip private field. FIXME: If the definition location of the
                // field is editable, we should show the completion
                continue;
            }
            f(Either::Left(field), ty);
        }
        for (i, ty) in receiver.tuple_fields(ctx.db).into_iter().enumerate() {
            // FIXME: Handle visibility
            f(Either::Right(i), ty);
        }
    }
}

fn complete_methods(
    ctx: &CompletionContext,
    receiver: &hir::Type,
    mut f: impl FnMut(hir::Function),
) {
    if let Some(krate) = ctx.krate {
        let mut seen_methods = FxHashSet::default();
        let traits_in_scope = ctx.scope.traits_in_scope();
        receiver.iterate_method_candidates(ctx.db, krate, &traits_in_scope, None, |_ty, func| {
            if func.self_param(ctx.db).is_some()
                && ctx.scope.module().map_or(true, |m| func.is_visible_from(ctx.db, m))
                && seen_methods.insert(func.name(ctx.db))
            {
                f(func);
            }
            None::<()>
        });
    }
}

#[cfg(test)]
mod tests {
    use expect_test::{expect, Expect};

    use crate::{
        tests::{check_edit, filtered_completion_list},
        CompletionKind,
    };

    fn check(ra_fixture: &str, expect: Expect) {
        let actual = filtered_completion_list(ra_fixture, CompletionKind::Reference);
        expect.assert_eq(&actual);
    }

    #[test]
    fn test_struct_field_and_method_completion() {
        check(
            r#"
struct S { foo: u32 }
impl S {
    fn bar(&self) {}
}
fn foo(s: S) { s.$0 }
"#,
            expect![[r#"
                fd foo   u32
                me bar() fn(&self)
            "#]],
        );
    }

    #[test]
    fn test_struct_field_completion_self() {
        check(
            r#"
struct S { the_field: (u32,) }
impl S {
    fn foo(self) { self.$0 }
}
"#,
            expect![[r#"
                fd the_field (u32,)
                me foo()     fn(self)
            "#]],
        )
    }

    #[test]
    fn test_struct_field_completion_autoderef() {
        check(
            r#"
struct A { the_field: (u32, i32) }
impl A {
    fn foo(&self) { self.$0 }
}
"#,
            expect![[r#"
                fd the_field (u32, i32)
                me foo()     fn(&self)
            "#]],
        )
    }

    #[test]
    fn test_no_struct_field_completion_for_method_call() {
        cov_mark::check!(test_no_struct_field_completion_for_method_call);
        check(
            r#"
struct A { the_field: u32 }
fn foo(a: A) { a.$0() }
"#,
            expect![[""]],
        );
    }

    #[test]
    fn test_visibility_filtering() {
        check(
            r#"
mod inner {
    pub struct A {
        private_field: u32,
        pub pub_field: u32,
        pub(crate) crate_field: u32,
        pub(crate) super_field: u32,
    }
}
fn foo(a: inner::A) { a.$0 }
"#,
            expect![[r#"
                fd pub_field   u32
                fd crate_field u32
                fd super_field u32
            "#]],
        );

        check(
            r#"
struct A {}
mod m {
    impl super::A {
        fn private_method(&self) {}
        pub(crate) fn the_method(&self) {}
    }
}
fn foo(a: A) { a.$0 }
"#,
            expect![[r#"
                me the_method() fn(&self)
            "#]],
        );
    }

    #[test]
    fn test_union_field_completion() {
        check(
            r#"
union U { field: u8, other: u16 }
fn foo(u: U) { u.$0 }
"#,
            expect![[r#"
                fd field u8
                fd other u16
            "#]],
        );
    }

    #[test]
    fn test_method_completion_only_fitting_impls() {
        check(
            r#"
struct A<T> {}
impl A<u32> {
    fn the_method(&self) {}
}
impl A<i32> {
    fn the_other_method(&self) {}
}
fn foo(a: A<u32>) { a.$0 }
"#,
            expect![[r#"
                me the_method() fn(&self)
            "#]],
        )
    }

    #[test]
    fn test_trait_method_completion() {
        check(
            r#"
struct A {}
trait Trait { fn the_method(&self); }
impl Trait for A {}
fn foo(a: A) { a.$0 }
"#,
            expect![[r#"
                me the_method() (as Trait) fn(&self)
            "#]],
        );
        check_edit(
            "the_method",
            r#"
struct A {}
trait Trait { fn the_method(&self); }
impl Trait for A {}
fn foo(a: A) { a.$0 }
"#,
            r#"
struct A {}
trait Trait { fn the_method(&self); }
impl Trait for A {}
fn foo(a: A) { a.the_method()$0 }
"#,
        );
    }

    #[test]
    fn test_trait_method_completion_deduplicated() {
        check(
            r"
struct A {}
trait Trait { fn the_method(&self); }
impl<T> Trait for T {}
fn foo(a: &A) { a.$0 }
",
            expect![[r#"
                me the_method() (as Trait) fn(&self)
            "#]],
        );
    }

    #[test]
    fn completes_trait_method_from_other_module() {
        check(
            r"
struct A {}
mod m {
    pub trait Trait { fn the_method(&self); }
}
use m::Trait;
impl Trait for A {}
fn foo(a: A) { a.$0 }
",
            expect![[r#"
                me the_method() (as Trait) fn(&self)
            "#]],
        );
    }

    #[test]
    fn test_no_non_self_method() {
        check(
            r#"
struct A {}
impl A {
    fn the_method() {}
}
fn foo(a: A) {
   a.$0
}
"#,
            expect![[""]],
        );
    }

    #[test]
    fn test_tuple_field_completion() {
        check(
            r#"
fn foo() {
   let b = (0, 3.14);
   b.$0
}
"#,
            expect![[r#"
                fd 0 i32
                fd 1 f64
            "#]],
        )
    }

    #[test]
    fn test_tuple_field_inference() {
        check(
            r#"
pub struct S;
impl S { pub fn blah(&self) {} }

struct T(S);

impl T {
    fn foo(&self) {
        // FIXME: This doesn't work without the trailing `a` as `0.` is a float
        self.0.a$0
    }
}
"#,
            expect![[r#"
                me blah() fn(&self)
            "#]],
        );
    }

    #[test]
    fn test_completion_works_in_consts() {
        check(
            r#"
struct A { the_field: u32 }
const X: u32 = {
    A { the_field: 92 }.$0
};
"#,
            expect![[r#"
                fd the_field u32
            "#]],
        );
    }

    #[test]
    fn works_in_simple_macro_1() {
        check(
            r#"
macro_rules! m { ($e:expr) => { $e } }
struct A { the_field: u32 }
fn foo(a: A) {
    m!(a.x$0)
}
"#,
            expect![[r#"
                fd the_field u32
            "#]],
        );
    }

    #[test]
    fn works_in_simple_macro_2() {
        // this doesn't work yet because the macro doesn't expand without the token -- maybe it can be fixed with better recovery
        check(
            r#"
macro_rules! m { ($e:expr) => { $e } }
struct A { the_field: u32 }
fn foo(a: A) {
    m!(a.$0)
}
"#,
            expect![[r#"
                fd the_field u32
            "#]],
        );
    }

    #[test]
    fn works_in_simple_macro_recursive_1() {
        check(
            r#"
macro_rules! m { ($e:expr) => { $e } }
struct A { the_field: u32 }
fn foo(a: A) {
    m!(m!(m!(a.x$0)))
}
"#,
            expect![[r#"
                fd the_field u32
            "#]],
        );
    }

    #[test]
    fn macro_expansion_resilient() {
        check(
            r#"
macro_rules! d {
    () => {};
    ($val:expr) => {
        match $val { tmp => { tmp } }
    };
    // Trailing comma with single argument is ignored
    ($val:expr,) => { $crate::d!($val) };
    ($($val:expr),+ $(,)?) => {
        ($($crate::d!($val)),+,)
    };
}
struct A { the_field: u32 }
fn foo(a: A) {
    d!(a.$0)
}
"#,
            expect![[r#"
                fd the_field u32
            "#]],
        );
    }

    #[test]
    fn test_method_completion_issue_3547() {
        check(
            r#"
struct HashSet<T> {}
impl<T> HashSet<T> {
    pub fn the_method(&self) {}
}
fn foo() {
    let s: HashSet<_>;
    s.$0
}
"#,
            expect![[r#"
                me the_method() fn(&self)
            "#]],
        );
    }

    #[test]
    fn completes_method_call_when_receiver_is_a_macro_call() {
        check(
            r#"
struct S;
impl S { fn foo(&self) {} }
macro_rules! make_s { () => { S }; }
fn main() { make_s!().f$0; }
"#,
            expect![[r#"
                me foo() fn(&self)
            "#]],
        )
    }

    #[test]
    fn completes_after_macro_call_in_submodule() {
        check(
            r#"
macro_rules! empty {
    () => {};
}

mod foo {
    #[derive(Debug, Default)]
    struct Template2 {}

    impl Template2 {
        fn private(&self) {}
    }
    fn baz() {
        let goo: Template2 = Template2 {};
        empty!();
        goo.$0
    }
}
        "#,
            expect![[r#"
                me private() fn(&self)
            "#]],
        );
    }

    #[test]
    fn issue_8931() {
        check(
            r#"
//- minicore: fn
struct S;

struct Foo;
impl Foo {
    fn foo(&self) -> &[u8] { loop {} }
}

impl S {
    fn indented(&mut self, f: impl FnOnce(&mut Self)) {
    }

    fn f(&mut self, v: Foo) {
        self.indented(|this| v.$0)
    }
}
        "#,
            expect![[r#"
                me foo() fn(&self) -> &[u8]
            "#]],
        );
    }

    #[test]
    fn completes_bare_fields_and_methods_in_methods() {
        check(
            r#"
struct Foo { field: i32 }

impl Foo { fn foo(&self) { $0 } }"#,
            expect![[r#"
                lc self       &Foo
                sp Self
                st Foo
                fd self.field i32
                me self.foo() fn(&self)
            "#]],
        );
        check(
            r#"
struct Foo(i32);

impl Foo { fn foo(&mut self) { $0 } }"#,
            expect![[r#"
                lc self       &mut Foo
                sp Self
                st Foo
                fd self.0     i32
                me self.foo() fn(&mut self)
            "#]],
        );
    }
}
