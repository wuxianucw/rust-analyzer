//! Renderer for function calls.

use hir::{AsAssocItem, HasSource, HirDisplay};
use ide_db::SymbolKind;
use itertools::Itertools;
use syntax::ast::Fn;

use crate::{
    item::{CompletionItem, CompletionItemKind, CompletionKind, CompletionRelevance, ImportEdit},
    render::{
        builder_ext::Params, compute_exact_name_match, compute_ref_match, compute_type_match,
        RenderContext,
    },
};

pub(crate) fn render_fn<'a>(
    ctx: RenderContext<'a>,
    import_to_add: Option<ImportEdit>,
    local_name: Option<hir::Name>,
    fn_: hir::Function,
) -> Option<CompletionItem> {
    let _p = profile::span("render_fn");
    Some(FunctionRender::new(ctx, None, local_name, fn_, false)?.render(import_to_add))
}

pub(crate) fn render_method<'a>(
    ctx: RenderContext<'a>,
    import_to_add: Option<ImportEdit>,
    receiver: Option<hir::Name>,
    local_name: Option<hir::Name>,
    fn_: hir::Function,
) -> Option<CompletionItem> {
    let _p = profile::span("render_method");
    Some(FunctionRender::new(ctx, receiver, local_name, fn_, true)?.render(import_to_add))
}

#[derive(Debug)]
struct FunctionRender<'a> {
    ctx: RenderContext<'a>,
    name: String,
    receiver: Option<hir::Name>,
    func: hir::Function,
    ast_node: Fn,
    is_method: bool,
}

impl<'a> FunctionRender<'a> {
    fn new(
        ctx: RenderContext<'a>,
        receiver: Option<hir::Name>,
        local_name: Option<hir::Name>,
        fn_: hir::Function,
        is_method: bool,
    ) -> Option<FunctionRender<'a>> {
        let name = local_name.unwrap_or_else(|| fn_.name(ctx.db())).to_string();
        let ast_node = fn_.source(ctx.db())?.value;

        Some(FunctionRender { ctx, name, receiver, func: fn_, ast_node, is_method })
    }

    fn render(self, import_to_add: Option<ImportEdit>) -> CompletionItem {
        let params = self.params();
        let call = if let Some(receiver) = &self.receiver {
            format!("{}.{}", receiver, &self.name)
        } else {
            self.name.clone()
        };
        let mut item =
            CompletionItem::new(CompletionKind::Reference, self.ctx.source_range(), call.clone());
        item.kind(self.kind())
            .set_documentation(self.ctx.docs(self.func))
            .set_deprecated(
                self.ctx.is_deprecated(self.func) || self.ctx.is_deprecated_assoc_item(self.func),
            )
            .detail(self.detail())
            .add_call_parens(self.ctx.completion, call.clone(), params);

        if import_to_add.is_none() {
            let db = self.ctx.db();
            if let Some(actm) = self.func.as_assoc_item(db) {
                if let Some(trt) = actm.containing_trait_or_trait_impl(db) {
                    item.trait_name(trt.name(db).to_string());
                }
            }
        }

        item.add_import(import_to_add).lookup_by(self.name);

        let ret_type = self.func.ret_type(self.ctx.db());
        item.set_relevance(CompletionRelevance {
            type_match: compute_type_match(self.ctx.completion, &ret_type),
            exact_name_match: compute_exact_name_match(self.ctx.completion, &call),
            ..CompletionRelevance::default()
        });

        if let Some(ref_match) = compute_ref_match(self.ctx.completion, &ret_type) {
            // FIXME
            // For now we don't properly calculate the edits for ref match
            // completions on methods, so we've disabled them. See #8058.
            if !self.is_method {
                item.ref_match(ref_match);
            }
        }

        item.build()
    }

    fn detail(&self) -> String {
        let ret_ty = self.func.ret_type(self.ctx.db());
        let ret = if ret_ty.is_unit() {
            // Omit the return type if it is the unit type
            String::new()
        } else {
            format!(" {}", self.ty_display())
        };

        format!("fn({}){}", self.params_display(), ret)
    }

    fn params_display(&self) -> String {
        if let Some(self_param) = self.func.self_param(self.ctx.db()) {
            let params = self
                .func
                .assoc_fn_params(self.ctx.db())
                .into_iter()
                .skip(1) // skip the self param because we are manually handling that
                .map(|p| p.ty().display(self.ctx.db()).to_string());

            std::iter::once(self_param.display(self.ctx.db()).to_owned()).chain(params).join(", ")
        } else {
            let params = self
                .func
                .assoc_fn_params(self.ctx.db())
                .into_iter()
                .map(|p| p.ty().display(self.ctx.db()).to_string())
                .join(", ");
            params
        }
    }

    fn ty_display(&self) -> String {
        let ret_ty = self.func.ret_type(self.ctx.db());

        format!("-> {}", ret_ty.display(self.ctx.db()))
    }

    fn add_arg(&self, arg: &str, ty: &hir::Type) -> String {
        if let Some(derefed_ty) = ty.remove_ref() {
            for (name, local) in self.ctx.completion.locals.iter() {
                if name == arg && local.ty(self.ctx.db()) == derefed_ty {
                    let mutability = if ty.is_mutable_reference() { "&mut " } else { "&" };
                    return format!("{}{}", mutability, arg);
                }
            }
        }
        arg.to_string()
    }

    fn params(&self) -> Params {
        let ast_params = match self.ast_node.param_list() {
            Some(it) => it,
            None => return Params::Named(Vec::new()),
        };

        let mut params_pats = Vec::new();
        let params_ty = if self.ctx.completion.has_dot_receiver() || self.receiver.is_some() {
            self.func.method_params(self.ctx.db()).unwrap_or_default()
        } else {
            if let Some(s) = ast_params.self_param() {
                cov_mark::hit!(parens_for_method_call_as_assoc_fn);
                params_pats.push(Some(s.to_string()));
            }
            self.func.assoc_fn_params(self.ctx.db())
        };
        params_pats
            .extend(ast_params.params().into_iter().map(|it| it.pat().map(|it| it.to_string())));

        let params = params_pats
            .into_iter()
            .zip(params_ty)
            .flat_map(|(pat, param_ty)| {
                let pat = pat?;
                let name = pat;
                let arg = name.trim_start_matches("mut ").trim_start_matches('_');
                Some(self.add_arg(arg, param_ty.ty()))
            })
            .collect();
        Params::Named(params)
    }

    fn kind(&self) -> CompletionItemKind {
        if self.func.self_param(self.ctx.db()).is_some() {
            CompletionItemKind::Method
        } else {
            SymbolKind::Function.into()
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        tests::{check_edit, check_edit_with_config, TEST_CONFIG},
        CompletionConfig,
    };

    #[test]
    fn inserts_parens_for_function_calls() {
        cov_mark::check!(inserts_parens_for_function_calls);
        check_edit(
            "no_args",
            r#"
fn no_args() {}
fn main() { no_$0 }
"#,
            r#"
fn no_args() {}
fn main() { no_args()$0 }
"#,
        );

        check_edit(
            "with_args",
            r#"
fn with_args(x: i32, y: String) {}
fn main() { with_$0 }
"#,
            r#"
fn with_args(x: i32, y: String) {}
fn main() { with_args(${1:x}, ${2:y})$0 }
"#,
        );

        check_edit(
            "foo",
            r#"
struct S;
impl S {
    fn foo(&self) {}
}
fn bar(s: &S) { s.f$0 }
"#,
            r#"
struct S;
impl S {
    fn foo(&self) {}
}
fn bar(s: &S) { s.foo()$0 }
"#,
        );

        check_edit(
            "foo",
            r#"
struct S {}
impl S {
    fn foo(&self, x: i32) {}
}
fn bar(s: &S) {
    s.f$0
}
"#,
            r#"
struct S {}
impl S {
    fn foo(&self, x: i32) {}
}
fn bar(s: &S) {
    s.foo(${1:x})$0
}
"#,
        );

        check_edit(
            "foo",
            r#"
struct S {}
impl S {
    fn foo(&self, x: i32) {
        $0
    }
}
"#,
            r#"
struct S {}
impl S {
    fn foo(&self, x: i32) {
        self.foo(${1:x})$0
    }
}
"#,
        );
    }

    #[test]
    fn parens_for_method_call_as_assoc_fn() {
        cov_mark::check!(parens_for_method_call_as_assoc_fn);
        check_edit(
            "foo",
            r#"
struct S;
impl S {
    fn foo(&self) {}
}
fn main() { S::f$0 }
"#,
            r#"
struct S;
impl S {
    fn foo(&self) {}
}
fn main() { S::foo(${1:&self})$0 }
"#,
        );
    }

    #[test]
    fn suppress_arg_snippets() {
        cov_mark::check!(suppress_arg_snippets);
        check_edit_with_config(
            CompletionConfig { add_call_argument_snippets: false, ..TEST_CONFIG },
            "with_args",
            r#"
fn with_args(x: i32, y: String) {}
fn main() { with_$0 }
"#,
            r#"
fn with_args(x: i32, y: String) {}
fn main() { with_args($0) }
"#,
        );
    }

    #[test]
    fn strips_underscores_from_args() {
        check_edit(
            "foo",
            r#"
fn foo(_foo: i32, ___bar: bool, ho_ge_: String) {}
fn main() { f$0 }
"#,
            r#"
fn foo(_foo: i32, ___bar: bool, ho_ge_: String) {}
fn main() { foo(${1:foo}, ${2:bar}, ${3:ho_ge_})$0 }
"#,
        );
    }

    #[test]
    fn insert_ref_when_matching_local_in_scope() {
        check_edit(
            "ref_arg",
            r#"
struct Foo {}
fn ref_arg(x: &Foo) {}
fn main() {
    let x = Foo {};
    ref_ar$0
}
"#,
            r#"
struct Foo {}
fn ref_arg(x: &Foo) {}
fn main() {
    let x = Foo {};
    ref_arg(${1:&x})$0
}
"#,
        );
    }

    #[test]
    fn insert_mut_ref_when_matching_local_in_scope() {
        check_edit(
            "ref_arg",
            r#"
struct Foo {}
fn ref_arg(x: &mut Foo) {}
fn main() {
    let x = Foo {};
    ref_ar$0
}
"#,
            r#"
struct Foo {}
fn ref_arg(x: &mut Foo) {}
fn main() {
    let x = Foo {};
    ref_arg(${1:&mut x})$0
}
"#,
        );
    }

    #[test]
    fn insert_ref_when_matching_local_in_scope_for_method() {
        check_edit(
            "apply_foo",
            r#"
struct Foo {}
struct Bar {}
impl Bar {
    fn apply_foo(&self, x: &Foo) {}
}

fn main() {
    let x = Foo {};
    let y = Bar {};
    y.$0
}
"#,
            r#"
struct Foo {}
struct Bar {}
impl Bar {
    fn apply_foo(&self, x: &Foo) {}
}

fn main() {
    let x = Foo {};
    let y = Bar {};
    y.apply_foo(${1:&x})$0
}
"#,
        );
    }

    #[test]
    fn trim_mut_keyword_in_func_completion() {
        check_edit(
            "take_mutably",
            r#"
fn take_mutably(mut x: &i32) {}

fn main() {
    take_m$0
}
"#,
            r#"
fn take_mutably(mut x: &i32) {}

fn main() {
    take_mutably(${1:x})$0
}
"#,
        );
    }
}
