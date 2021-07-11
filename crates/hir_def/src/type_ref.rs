//! HIR for references to types. Paths in these are not yet resolved. They can
//! be directly created from an ast::TypeRef, without further queries.

use hir_expand::{name::Name, AstId, InFile};
use std::convert::TryInto;
use syntax::ast;

use crate::{body::LowerCtx, intern::Interned, path::Path};

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Mutability {
    Shared,
    Mut,
}

impl Mutability {
    pub fn from_mutable(mutable: bool) -> Mutability {
        if mutable {
            Mutability::Mut
        } else {
            Mutability::Shared
        }
    }

    pub fn as_keyword_for_ref(self) -> &'static str {
        match self {
            Mutability::Shared => "",
            Mutability::Mut => "mut ",
        }
    }

    pub fn as_keyword_for_ptr(self) -> &'static str {
        match self {
            Mutability::Shared => "const ",
            Mutability::Mut => "mut ",
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum Rawness {
    RawPtr,
    Ref,
}

impl Rawness {
    pub fn from_raw(is_raw: bool) -> Rawness {
        if is_raw {
            Rawness::RawPtr
        } else {
            Rawness::Ref
        }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TraitRef {
    pub path: Path,
}

impl TraitRef {
    /// Converts an `ast::PathType` to a `hir::TraitRef`.
    pub(crate) fn from_ast(ctx: &LowerCtx, node: ast::Type) -> Option<Self> {
        // FIXME: Use `Path::from_src`
        match node {
            ast::Type::PathType(path) => {
                path.path().and_then(|it| ctx.lower_path(it)).map(|path| TraitRef { path })
            }
            _ => None,
        }
    }
}

/// Compare ty::Ty
///
/// Note: Most users of `TypeRef` that end up in the salsa database intern it using
/// `Interned<TypeRef>` to save space. But notably, nested `TypeRef`s are not interned, since that
/// does not seem to save any noticeable amount of memory.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum TypeRef {
    Never,
    Placeholder,
    Tuple(Vec<TypeRef>),
    Path(Path),
    RawPtr(Box<TypeRef>, Mutability),
    Reference(Box<TypeRef>, Option<LifetimeRef>, Mutability),
    // FIXME: for full const generics, the latter element (length) here is going to have to be an
    // expression that is further lowered later in hir_ty.
    Array(Box<TypeRef>, ConstScalar),
    Slice(Box<TypeRef>),
    /// A fn pointer. Last element of the vector is the return type.
    Fn(Vec<TypeRef>, bool /*varargs*/),
    // For
    ImplTrait(Vec<Interned<TypeBound>>),
    DynTrait(Vec<Interned<TypeBound>>),
    Macro(AstId<ast::MacroCall>),
    Error,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct LifetimeRef {
    pub name: Name,
}

impl LifetimeRef {
    pub(crate) fn new_name(name: Name) -> Self {
        LifetimeRef { name }
    }

    pub(crate) fn new(lifetime: &ast::Lifetime) -> Self {
        LifetimeRef { name: Name::new_lifetime(lifetime) }
    }

    pub fn missing() -> LifetimeRef {
        LifetimeRef { name: Name::missing() }
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum TypeBound {
    Path(Path),
    ForLifetime(Box<[Name]>, Path),
    Lifetime(LifetimeRef),
    Error,
}

impl TypeRef {
    /// Converts an `ast::TypeRef` to a `hir::TypeRef`.
    pub fn from_ast(ctx: &LowerCtx, node: ast::Type) -> Self {
        match node {
            ast::Type::ParenType(inner) => TypeRef::from_ast_opt(ctx, inner.ty()),
            ast::Type::TupleType(inner) => {
                TypeRef::Tuple(inner.fields().map(|it| TypeRef::from_ast(ctx, it)).collect())
            }
            ast::Type::NeverType(..) => TypeRef::Never,
            ast::Type::PathType(inner) => {
                // FIXME: Use `Path::from_src`
                inner
                    .path()
                    .and_then(|it| ctx.lower_path(it))
                    .map(TypeRef::Path)
                    .unwrap_or(TypeRef::Error)
            }
            ast::Type::PtrType(inner) => {
                let inner_ty = TypeRef::from_ast_opt(ctx, inner.ty());
                let mutability = Mutability::from_mutable(inner.mut_token().is_some());
                TypeRef::RawPtr(Box::new(inner_ty), mutability)
            }
            ast::Type::ArrayType(inner) => {
                // FIXME: This is a hack. We should probably reuse the machinery of
                // `hir_def::body::lower` to lower this into an `Expr` and then evaluate it at the
                // `hir_ty` level, which would allow knowing the type of:
                // let v: [u8; 2 + 2] = [0u8; 4];
                let len = inner
                    .expr()
                    .map(ConstScalar::usize_from_literal_expr)
                    .unwrap_or(ConstScalar::Unknown);

                TypeRef::Array(Box::new(TypeRef::from_ast_opt(ctx, inner.ty())), len)
            }
            ast::Type::SliceType(inner) => {
                TypeRef::Slice(Box::new(TypeRef::from_ast_opt(ctx, inner.ty())))
            }
            ast::Type::RefType(inner) => {
                let inner_ty = TypeRef::from_ast_opt(ctx, inner.ty());
                let lifetime = inner.lifetime().map(|lt| LifetimeRef::new(&lt));
                let mutability = Mutability::from_mutable(inner.mut_token().is_some());
                TypeRef::Reference(Box::new(inner_ty), lifetime, mutability)
            }
            ast::Type::InferType(_inner) => TypeRef::Placeholder,
            ast::Type::FnPtrType(inner) => {
                let ret_ty = inner
                    .ret_type()
                    .and_then(|rt| rt.ty())
                    .map(|it| TypeRef::from_ast(ctx, it))
                    .unwrap_or_else(|| TypeRef::Tuple(Vec::new()));
                let mut is_varargs = false;
                let mut params = if let Some(pl) = inner.param_list() {
                    if let Some(param) = pl.params().last() {
                        is_varargs = param.dotdotdot_token().is_some();
                    }

                    pl.params().map(|p| p.ty()).map(|it| TypeRef::from_ast_opt(ctx, it)).collect()
                } else {
                    Vec::new()
                };
                params.push(ret_ty);
                TypeRef::Fn(params, is_varargs)
            }
            // for types are close enough for our purposes to the inner type for now...
            ast::Type::ForType(inner) => TypeRef::from_ast_opt(ctx, inner.ty()),
            ast::Type::ImplTraitType(inner) => {
                TypeRef::ImplTrait(type_bounds_from_ast(ctx, inner.type_bound_list()))
            }
            ast::Type::DynTraitType(inner) => {
                TypeRef::DynTrait(type_bounds_from_ast(ctx, inner.type_bound_list()))
            }
            ast::Type::MacroType(mt) => match mt.macro_call() {
                Some(mc) => ctx
                    .ast_id(&mc)
                    .map(|mc| TypeRef::Macro(InFile::new(ctx.file_id(), mc)))
                    .unwrap_or(TypeRef::Error),
                None => TypeRef::Error,
            },
        }
    }

    pub(crate) fn from_ast_opt(ctx: &LowerCtx, node: Option<ast::Type>) -> Self {
        if let Some(node) = node {
            TypeRef::from_ast(ctx, node)
        } else {
            TypeRef::Error
        }
    }

    pub(crate) fn unit() -> TypeRef {
        TypeRef::Tuple(Vec::new())
    }

    pub fn walk(&self, f: &mut impl FnMut(&TypeRef)) {
        go(self, f);

        fn go(type_ref: &TypeRef, f: &mut impl FnMut(&TypeRef)) {
            f(type_ref);
            match type_ref {
                TypeRef::Fn(types, _) | TypeRef::Tuple(types) => {
                    types.iter().for_each(|t| go(t, f))
                }
                TypeRef::RawPtr(type_ref, _)
                | TypeRef::Reference(type_ref, ..)
                | TypeRef::Array(type_ref, _)
                | TypeRef::Slice(type_ref) => go(type_ref, f),
                TypeRef::ImplTrait(bounds) | TypeRef::DynTrait(bounds) => {
                    for bound in bounds {
                        match bound.as_ref() {
                            TypeBound::Path(path) | TypeBound::ForLifetime(_, path) => {
                                go_path(path, f)
                            }
                            TypeBound::Lifetime(_) | TypeBound::Error => (),
                        }
                    }
                }
                TypeRef::Path(path) => go_path(path, f),
                TypeRef::Never | TypeRef::Placeholder | TypeRef::Macro(_) | TypeRef::Error => {}
            };
        }

        fn go_path(path: &Path, f: &mut impl FnMut(&TypeRef)) {
            if let Some(type_ref) = path.type_anchor() {
                go(type_ref, f);
            }
            for segment in path.segments().iter() {
                if let Some(args_and_bindings) = segment.args_and_bindings {
                    for arg in &args_and_bindings.args {
                        match arg {
                            crate::path::GenericArg::Type(type_ref) => {
                                go(type_ref, f);
                            }
                            crate::path::GenericArg::Lifetime(_) => {}
                        }
                    }
                    for binding in &args_and_bindings.bindings {
                        if let Some(type_ref) = &binding.type_ref {
                            go(type_ref, f);
                        }
                        for bound in &binding.bounds {
                            match bound.as_ref() {
                                TypeBound::Path(path) | TypeBound::ForLifetime(_, path) => {
                                    go_path(path, f)
                                }
                                TypeBound::Lifetime(_) | TypeBound::Error => (),
                            }
                        }
                    }
                }
            }
        }
    }
}

pub(crate) fn type_bounds_from_ast(
    lower_ctx: &LowerCtx,
    type_bounds_opt: Option<ast::TypeBoundList>,
) -> Vec<Interned<TypeBound>> {
    if let Some(type_bounds) = type_bounds_opt {
        type_bounds.bounds().map(|it| Interned::new(TypeBound::from_ast(lower_ctx, it))).collect()
    } else {
        vec![]
    }
}

impl TypeBound {
    pub(crate) fn from_ast(ctx: &LowerCtx, node: ast::TypeBound) -> Self {
        let lower_path_type = |path_type: ast::PathType| ctx.lower_path(path_type.path()?);

        match node.kind() {
            ast::TypeBoundKind::PathType(path_type) => {
                lower_path_type(path_type).map(TypeBound::Path).unwrap_or(TypeBound::Error)
            }
            ast::TypeBoundKind::ForType(for_type) => {
                let lt_refs = match for_type.generic_param_list() {
                    Some(gpl) => gpl
                        .lifetime_params()
                        .flat_map(|lp| lp.lifetime().map(|lt| Name::new_lifetime(&lt)))
                        .collect(),
                    None => Box::default(),
                };
                let path = for_type.ty().and_then(|ty| match ty {
                    ast::Type::PathType(path_type) => lower_path_type(path_type),
                    _ => None,
                });
                match path {
                    Some(p) => TypeBound::ForLifetime(lt_refs, p),
                    None => TypeBound::Error,
                }
            }
            ast::TypeBoundKind::Lifetime(lifetime) => {
                TypeBound::Lifetime(LifetimeRef::new(&lifetime))
            }
        }
    }

    pub fn as_path(&self) -> Option<&Path> {
        match self {
            TypeBound::Path(p) | TypeBound::ForLifetime(_, p) => Some(p),
            TypeBound::Lifetime(_) | TypeBound::Error => None,
        }
    }
}

/// A concrete constant value
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConstScalar {
    // for now, we only support the trivial case of constant evaluating the length of an array
    // Note that this is u64 because the target usize may be bigger than our usize
    Usize(u64),

    /// Case of an unknown value that rustc might know but we don't
    // FIXME: this is a hack to get around chalk not being able to represent unevaluatable
    // constants
    // https://github.com/rust-analyzer/rust-analyzer/pull/8813#issuecomment-840679177
    // https://rust-lang.zulipchat.com/#narrow/stream/144729-wg-traits/topic/Handling.20non.20evaluatable.20constants'.20equality/near/238386348
    Unknown,
}

impl std::fmt::Display for ConstScalar {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            ConstScalar::Usize(us) => write!(fmt, "{}", us),
            ConstScalar::Unknown => write!(fmt, "_"),
        }
    }
}

impl ConstScalar {
    /// Gets a target usize out of the ConstScalar
    pub fn as_usize(&self) -> Option<u64> {
        match self {
            &ConstScalar::Usize(us) => Some(us),
            _ => None,
        }
    }

    // FIXME: as per the comments on `TypeRef::Array`, this evaluation should not happen at this
    // parse stage.
    fn usize_from_literal_expr(expr: ast::Expr) -> ConstScalar {
        match expr {
            ast::Expr::Literal(lit) => {
                let lkind = lit.kind();
                match lkind {
                    ast::LiteralKind::IntNumber(num)
                        if num.suffix() == None || num.suffix() == Some("usize") =>
                    {
                        num.value().and_then(|v| v.try_into().ok())
                    }
                    _ => None,
                }
            }
            _ => None,
        }
        .map(ConstScalar::Usize)
        .unwrap_or(ConstScalar::Unknown)
    }
}
