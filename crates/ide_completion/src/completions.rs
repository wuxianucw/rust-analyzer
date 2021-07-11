//! This module defines an accumulator for completions which are going to be presented to user.

pub(crate) mod attribute;
pub(crate) mod dot;
pub(crate) mod flyimport;
pub(crate) mod fn_param;
pub(crate) mod keyword;
pub(crate) mod lifetime;
pub(crate) mod mod_;
pub(crate) mod pattern;
pub(crate) mod postfix;
pub(crate) mod qualified_path;
pub(crate) mod record;
pub(crate) mod snippet;
pub(crate) mod trait_impl;
pub(crate) mod unqualified_path;

use std::iter;

use hir::known;
use ide_db::SymbolKind;

use crate::{
    item::{Builder, CompletionKind},
    render::{
        const_::render_const,
        enum_variant::render_variant,
        function::{render_fn, render_method},
        macro_::render_macro,
        pattern::{render_struct_pat, render_variant_pat},
        render_field, render_resolution, render_tuple_field,
        type_alias::{render_type_alias, render_type_alias_with_eq},
        RenderContext,
    },
    CompletionContext, CompletionItem, CompletionItemKind,
};

/// Represents an in-progress set of completions being built.
#[derive(Debug, Default)]
pub struct Completions {
    buf: Vec<CompletionItem>,
}

impl From<Completions> for Vec<CompletionItem> {
    fn from(val: Completions) -> Self {
        val.buf
    }
}

impl Builder {
    /// Convenience method, which allows to add a freshly created completion into accumulator
    /// without binding it to the variable.
    pub(crate) fn add_to(self, acc: &mut Completions) {
        acc.add(self.build())
    }
}

impl Completions {
    fn add(&mut self, item: CompletionItem) {
        self.buf.push(item)
    }

    fn add_opt(&mut self, item: Option<CompletionItem>) {
        if let Some(item) = item {
            self.buf.push(item)
        }
    }

    pub(crate) fn add_all<I>(&mut self, items: I)
    where
        I: IntoIterator,
        I::Item: Into<CompletionItem>,
    {
        items.into_iter().for_each(|item| self.add(item.into()))
    }

    pub(crate) fn add_keyword(&mut self, ctx: &CompletionContext, keyword: &'static str) {
        let mut item = CompletionItem::new(CompletionKind::Keyword, ctx.source_range(), keyword);
        item.kind(CompletionItemKind::Keyword);
        item.add_to(self);
    }

    pub(crate) fn add_resolution(
        &mut self,
        ctx: &CompletionContext,
        local_name: hir::Name,
        resolution: &hir::ScopeDef,
    ) {
        self.add_opt(render_resolution(RenderContext::new(ctx), local_name, resolution));
    }

    pub(crate) fn add_macro(
        &mut self,
        ctx: &CompletionContext,
        name: Option<hir::Name>,
        macro_: hir::MacroDef,
    ) {
        let name = match name {
            Some(it) => it,
            None => return,
        };
        self.add_opt(render_macro(RenderContext::new(ctx), None, name, macro_));
    }

    pub(crate) fn add_function(
        &mut self,
        ctx: &CompletionContext,
        func: hir::Function,
        local_name: Option<hir::Name>,
    ) {
        self.add_opt(render_fn(RenderContext::new(ctx), None, local_name, func));
    }

    pub(crate) fn add_method(
        &mut self,
        ctx: &CompletionContext,
        func: hir::Function,
        receiver: Option<hir::Name>,
        local_name: Option<hir::Name>,
    ) {
        self.add_opt(render_method(RenderContext::new(ctx), None, receiver, local_name, func));
    }

    pub(crate) fn add_const(&mut self, ctx: &CompletionContext, constant: hir::Const) {
        self.add_opt(render_const(RenderContext::new(ctx), constant));
    }

    pub(crate) fn add_type_alias(&mut self, ctx: &CompletionContext, type_alias: hir::TypeAlias) {
        self.add_opt(render_type_alias(RenderContext::new(ctx), type_alias));
    }

    pub(crate) fn add_type_alias_with_eq(
        &mut self,
        ctx: &CompletionContext,
        type_alias: hir::TypeAlias,
    ) {
        self.add_opt(render_type_alias_with_eq(RenderContext::new(ctx), type_alias));
    }

    pub(crate) fn add_qualified_enum_variant(
        &mut self,
        ctx: &CompletionContext,
        variant: hir::Variant,
        path: hir::ModPath,
    ) {
        let item = render_variant(RenderContext::new(ctx), None, None, variant, Some(path));
        self.add(item);
    }

    pub(crate) fn add_enum_variant(
        &mut self,
        ctx: &CompletionContext,
        variant: hir::Variant,
        local_name: Option<hir::Name>,
    ) {
        let item = render_variant(RenderContext::new(ctx), None, local_name, variant, None);
        self.add(item);
    }

    pub(crate) fn add_field(
        &mut self,
        ctx: &CompletionContext,
        receiver: Option<hir::Name>,
        field: hir::Field,
        ty: &hir::Type,
    ) {
        let item = render_field(RenderContext::new(ctx), receiver, field, ty);
        self.add(item);
    }

    pub(crate) fn add_tuple_field(
        &mut self,
        ctx: &CompletionContext,
        receiver: Option<hir::Name>,
        field: usize,
        ty: &hir::Type,
    ) {
        let item = render_tuple_field(RenderContext::new(ctx), receiver, field, ty);
        self.add(item);
    }

    pub(crate) fn add_static_lifetime(&mut self, ctx: &CompletionContext) {
        let mut item =
            CompletionItem::new(CompletionKind::Reference, ctx.source_range(), "'static");
        item.kind(CompletionItemKind::SymbolKind(SymbolKind::LifetimeParam));
        self.add(item.build());
    }

    pub(crate) fn add_variant_pat(
        &mut self,
        ctx: &CompletionContext,
        variant: hir::Variant,
        local_name: Option<hir::Name>,
    ) {
        self.add_opt(render_variant_pat(RenderContext::new(ctx), variant, local_name, None));
    }

    pub(crate) fn add_qualified_variant_pat(
        &mut self,
        ctx: &CompletionContext,
        variant: hir::Variant,
        path: hir::ModPath,
    ) {
        self.add_opt(render_variant_pat(RenderContext::new(ctx), variant, None, Some(path)));
    }

    pub(crate) fn add_struct_pat(
        &mut self,
        ctx: &CompletionContext,
        strukt: hir::Struct,
        local_name: Option<hir::Name>,
    ) {
        self.add_opt(render_struct_pat(RenderContext::new(ctx), strukt, local_name));
    }
}

/// Calls the callback for each variant of the provided enum with the path to the variant.
/// Skips variants that are visible with single segment paths.
fn enum_variants_with_paths(
    acc: &mut Completions,
    ctx: &CompletionContext,
    enum_: hir::Enum,
    cb: impl Fn(&mut Completions, &CompletionContext, hir::Variant, hir::ModPath),
) {
    let variants = enum_.variants(ctx.db);

    let module = if let Some(module) = ctx.scope.module() {
        // Compute path from the completion site if available.
        module
    } else {
        // Otherwise fall back to the enum's definition site.
        enum_.module(ctx.db)
    };

    if let Some(impl_) = ctx.impl_def.as_ref().and_then(|impl_| ctx.sema.to_def(impl_)) {
        if impl_.self_ty(ctx.db).as_adt() == Some(hir::Adt::Enum(enum_)) {
            for &variant in &variants {
                let self_path = hir::ModPath::from_segments(
                    hir::PathKind::Plain,
                    iter::once(known::SELF_TYPE).chain(iter::once(variant.name(ctx.db))),
                );
                cb(acc, ctx, variant, self_path);
            }
        }
    }

    for variant in variants {
        if let Some(path) = module.find_use_path(ctx.db, hir::ModuleDef::from(variant)) {
            // Variants with trivial paths are already added by the existing completion logic,
            // so we should avoid adding these twice
            if path.segments().len() > 1 {
                cb(acc, ctx, variant, path);
            }
        }
    }
}
