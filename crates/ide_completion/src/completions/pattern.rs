//! Completes constants and paths in patterns.

use crate::{
    context::{PatternContext, PatternRefutability},
    CompletionContext, Completions,
};

/// Completes constants and paths in patterns.
pub(crate) fn complete_pattern(acc: &mut Completions, ctx: &CompletionContext) {
    let refutable = match ctx.pattern_ctx {
        Some(PatternContext { refutability, .. }) => refutability == PatternRefutability::Refutable,
        None => return,
    };

    if refutable {
        if let Some(hir::Adt::Enum(e)) =
            ctx.expected_type.as_ref().and_then(|ty| ty.strip_references().as_adt())
        {
            super::enum_variants_with_paths(acc, ctx, e, |acc, ctx, variant, path| {
                acc.add_qualified_variant_pat(ctx, variant, path.clone());
                acc.add_qualified_enum_variant(ctx, variant, path);
            });
        }
    }

    // FIXME: ideally, we should look at the type we are matching against and
    // suggest variants + auto-imports
    ctx.process_all_names(&mut |name, res| {
        let add_resolution = match &res {
            hir::ScopeDef::ModuleDef(def) => match def {
                hir::ModuleDef::Adt(hir::Adt::Struct(strukt)) => {
                    acc.add_struct_pat(ctx, *strukt, Some(name.clone()));
                    true
                }
                hir::ModuleDef::Variant(variant) if refutable => {
                    acc.add_variant_pat(ctx, *variant, Some(name.clone()));
                    true
                }
                hir::ModuleDef::Adt(hir::Adt::Enum(..))
                | hir::ModuleDef::Variant(..)
                | hir::ModuleDef::Const(..)
                | hir::ModuleDef::Module(..) => refutable,
                _ => false,
            },
            hir::ScopeDef::MacroDef(mac) => mac.is_fn_like(),
            hir::ScopeDef::ImplSelfType(impl_) => match impl_.self_ty(ctx.db).as_adt() {
                Some(hir::Adt::Struct(strukt)) => {
                    acc.add_struct_pat(ctx, strukt, Some(name.clone()));
                    true
                }
                Some(hir::Adt::Enum(_)) => refutable,
                _ => true,
            },
            _ => false,
        };
        if add_resolution {
            acc.add_resolution(ctx, name, &res);
        }
    });
}
