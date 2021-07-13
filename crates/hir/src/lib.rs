//! HIR (previously known as descriptors) provides a high-level object oriented
//! access to Rust code.
//!
//! The principal difference between HIR and syntax trees is that HIR is bound
//! to a particular crate instance. That is, it has cfg flags and features
//! applied. So, the relation between syntax and HIR is many-to-one.
//!
//! HIR is the public API of the all of the compiler logic above syntax trees.
//! It is written in "OO" style. Each type is self contained (as in, it knows it's
//! parents and full context). It should be "clean code".
//!
//! `hir_*` crates are the implementation of the compiler logic.
//! They are written in "ECS" style, with relatively little abstractions.
//! Many types are not self-contained, and explicitly use local indexes, arenas, etc.
//!
//! `hir` is what insulates the "we don't know how to actually write an incremental compiler"
//! from the ide with completions, hovers, etc. It is a (soft, internal) boundary:
//! <https://www.tedinski.com/2018/02/06/system-boundaries.html>.

#![recursion_limit = "512"]

mod semantics;
mod source_analyzer;

mod from_id;
mod attrs;
mod has_source;

pub mod diagnostics;
pub mod db;

mod display;

use std::{iter, sync::Arc};

use arrayvec::ArrayVec;
use base_db::{CrateDisplayName, CrateId, Edition, FileId};
use either::Either;
use hir_def::{
    adt::{ReprKind, VariantData},
    body::{BodyDiagnostic, SyntheticSyntax},
    expr::{BindingAnnotation, LabelId, Pat, PatId},
    item_tree::ItemTreeNode,
    lang_item::LangItemTarget,
    nameres,
    per_ns::PerNs,
    resolver::{HasResolver, Resolver},
    src::HasSource as _,
    AdtId, AssocContainerId, AssocItemId, AssocItemLoc, AttrDefId, ConstId, ConstParamId,
    DefWithBodyId, EnumId, FunctionId, GenericDefId, HasModule, ImplId, LifetimeParamId,
    LocalEnumVariantId, LocalFieldId, Lookup, ModuleId, StaticId, StructId, TraitId, TypeAliasId,
    TypeParamId, UnionId,
};
use hir_expand::{name::name, MacroCallKind, MacroDefId, MacroDefKind};
use hir_ty::{
    autoderef,
    consteval::ConstExt,
    could_unify,
    diagnostics::BodyValidationDiagnostic,
    method_resolution::{self, TyFingerprint},
    primitive::UintTy,
    subst_prefix,
    traits::FnTrait,
    AliasEq, AliasTy, BoundVar, CallableDefId, CallableSig, Canonical, CanonicalVarKinds, Cast,
    DebruijnIndex, InEnvironment, Interner, QuantifiedWhereClause, Scalar, Solution, Substitution,
    TraitEnvironment, TraitRefExt, Ty, TyBuilder, TyDefId, TyExt, TyKind, TyVariableKind,
    WhereClause,
};
use itertools::Itertools;
use nameres::diagnostics::DefDiagnosticKind;
use once_cell::unsync::Lazy;
use rustc_hash::FxHashSet;
use stdx::{format_to, impl_from};
use syntax::{
    ast::{self, AttrsOwner, NameOwner},
    AstNode, AstPtr, SmolStr, SyntaxKind, SyntaxNodePtr,
};
use tt::{Ident, Leaf, Literal, TokenTree};

use crate::db::{DefDatabase, HirDatabase};

pub use crate::{
    attrs::{HasAttrs, Namespace},
    diagnostics::{
        AnyDiagnostic, BreakOutsideOfLoop, InactiveCode, IncorrectCase, MacroError,
        MismatchedArgCount, MissingFields, MissingMatchArms, MissingOkOrSomeInTailExpr,
        MissingUnsafe, NoSuchField, RemoveThisSemicolon, ReplaceFilterMapNextWithFindMap,
        UnimplementedBuiltinMacro, UnresolvedExternCrate, UnresolvedImport, UnresolvedMacroCall,
        UnresolvedModule, UnresolvedProcMacro,
    },
    has_source::HasSource,
    semantics::{PathResolution, Semantics, SemanticsScope},
};

// Be careful with these re-exports.
//
// `hir` is the boundary between the compiler and the IDE. It should try hard to
// isolate the compiler from the ide, to allow the two to be refactored
// independently. Re-exporting something from the compiler is the sure way to
// breach the boundary.
//
// Generally, a refactoring which *removes* a name from this list is a good
// idea!
pub use {
    cfg::{CfgAtom, CfgExpr, CfgOptions},
    hir_def::{
        adt::StructKind,
        attr::{Attr, Attrs, AttrsWithOwner, Documentation},
        find_path::PrefixKind,
        import_map,
        item_scope::ItemInNs, // FIXME: don't re-export ItemInNs, as it uses raw ids.
        nameres::ModuleSource,
        path::{ModPath, PathKind},
        type_ref::{Mutability, TypeRef},
        visibility::Visibility,
    },
    hir_expand::{
        name::{known, Name},
        ExpandResult, HirFileId, InFile, MacroFile, Origin,
    },
    hir_ty::display::HirDisplay,
};

// These are negative re-exports: pub using these names is forbidden, they
// should remain private to hir internals.
#[allow(unused)]
use {
    hir_def::path::Path,
    hir_expand::{hygiene::Hygiene, name::AsName},
};

/// hir::Crate describes a single crate. It's the main interface with which
/// a crate's dependencies interact. Mostly, it should be just a proxy for the
/// root module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Crate {
    pub(crate) id: CrateId,
}

#[derive(Debug)]
pub struct CrateDependency {
    pub krate: Crate,
    pub name: Name,
}

impl Crate {
    pub fn dependencies(self, db: &dyn HirDatabase) -> Vec<CrateDependency> {
        db.crate_graph()[self.id]
            .dependencies
            .iter()
            .map(|dep| {
                let krate = Crate { id: dep.crate_id };
                let name = dep.as_name();
                CrateDependency { krate, name }
            })
            .collect()
    }

    pub fn reverse_dependencies(self, db: &dyn HirDatabase) -> Vec<Crate> {
        let crate_graph = db.crate_graph();
        crate_graph
            .iter()
            .filter(|&krate| {
                crate_graph[krate].dependencies.iter().any(|it| it.crate_id == self.id)
            })
            .map(|id| Crate { id })
            .collect()
    }

    pub fn transitive_reverse_dependencies(self, db: &dyn HirDatabase) -> Vec<Crate> {
        db.crate_graph().transitive_rev_deps(self.id).into_iter().map(|id| Crate { id }).collect()
    }

    pub fn root_module(self, db: &dyn HirDatabase) -> Module {
        let def_map = db.crate_def_map(self.id);
        Module { id: def_map.module_id(def_map.root()) }
    }

    pub fn root_file(self, db: &dyn HirDatabase) -> FileId {
        db.crate_graph()[self.id].root_file_id
    }

    pub fn edition(self, db: &dyn HirDatabase) -> Edition {
        db.crate_graph()[self.id].edition
    }

    pub fn display_name(self, db: &dyn HirDatabase) -> Option<CrateDisplayName> {
        db.crate_graph()[self.id].display_name.clone()
    }

    pub fn query_external_importables(
        self,
        db: &dyn DefDatabase,
        query: import_map::Query,
    ) -> impl Iterator<Item = Either<ModuleDef, MacroDef>> {
        let _p = profile::span("query_external_importables");
        import_map::search_dependencies(db, self.into(), query).into_iter().map(|item| match item {
            ItemInNs::Types(mod_id) | ItemInNs::Values(mod_id) => Either::Left(mod_id.into()),
            ItemInNs::Macros(mac_id) => Either::Right(mac_id.into()),
        })
    }

    pub fn all(db: &dyn HirDatabase) -> Vec<Crate> {
        db.crate_graph().iter().map(|id| Crate { id }).collect()
    }

    /// Try to get the root URL of the documentation of a crate.
    pub fn get_html_root_url(self: &Crate, db: &dyn HirDatabase) -> Option<String> {
        // Look for #![doc(html_root_url = "...")]
        let attrs = db.attrs(AttrDefId::ModuleId(self.root_module(db).into()));
        let doc_attr_q = attrs.by_key("doc");

        if !doc_attr_q.exists() {
            return None;
        }

        let doc_url = doc_attr_q.tt_values().map(|tt| {
            let name = tt.token_trees.iter()
                .skip_while(|tt| !matches!(tt, TokenTree::Leaf(Leaf::Ident(Ident{text: ref ident, ..})) if ident == "html_root_url"))
                .nth(2);

            match name {
                Some(TokenTree::Leaf(Leaf::Literal(Literal{ref text, ..}))) => Some(text),
                _ => None
            }
        }).flatten().next();

        doc_url.map(|s| s.trim_matches('"').trim_end_matches('/').to_owned() + "/")
    }

    pub fn cfg(&self, db: &dyn HirDatabase) -> CfgOptions {
        db.crate_graph()[self.id].cfg_options.clone()
    }

    pub fn potential_cfg(&self, db: &dyn HirDatabase) -> CfgOptions {
        db.crate_graph()[self.id].potential_cfg_options.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Module {
    pub(crate) id: ModuleId,
}

/// The defs which can be visible in the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleDef {
    Module(Module),
    Function(Function),
    Adt(Adt),
    // Can't be directly declared, but can be imported.
    Variant(Variant),
    Const(Const),
    Static(Static),
    Trait(Trait),
    TypeAlias(TypeAlias),
    BuiltinType(BuiltinType),
}
impl_from!(
    Module,
    Function,
    Adt(Struct, Enum, Union),
    Variant,
    Const,
    Static,
    Trait,
    TypeAlias,
    BuiltinType
    for ModuleDef
);

impl From<VariantDef> for ModuleDef {
    fn from(var: VariantDef) -> Self {
        match var {
            VariantDef::Struct(t) => Adt::from(t).into(),
            VariantDef::Union(t) => Adt::from(t).into(),
            VariantDef::Variant(t) => t.into(),
        }
    }
}

impl ModuleDef {
    pub fn module(self, db: &dyn HirDatabase) -> Option<Module> {
        match self {
            ModuleDef::Module(it) => it.parent(db),
            ModuleDef::Function(it) => Some(it.module(db)),
            ModuleDef::Adt(it) => Some(it.module(db)),
            ModuleDef::Variant(it) => Some(it.module(db)),
            ModuleDef::Const(it) => Some(it.module(db)),
            ModuleDef::Static(it) => Some(it.module(db)),
            ModuleDef::Trait(it) => Some(it.module(db)),
            ModuleDef::TypeAlias(it) => Some(it.module(db)),
            ModuleDef::BuiltinType(_) => None,
        }
    }

    pub fn canonical_path(&self, db: &dyn HirDatabase) -> Option<String> {
        let mut segments = vec![self.name(db)?.to_string()];
        for m in self.module(db)?.path_to_root(db) {
            segments.extend(m.name(db).map(|it| it.to_string()))
        }
        segments.reverse();
        Some(segments.join("::"))
    }

    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        match self {
            ModuleDef::Adt(it) => Some(it.name(db)),
            ModuleDef::Trait(it) => Some(it.name(db)),
            ModuleDef::Function(it) => Some(it.name(db)),
            ModuleDef::Variant(it) => Some(it.name(db)),
            ModuleDef::TypeAlias(it) => Some(it.name(db)),
            ModuleDef::Module(it) => it.name(db),
            ModuleDef::Const(it) => it.name(db),
            ModuleDef::Static(it) => it.name(db),
            ModuleDef::BuiltinType(it) => Some(it.name()),
        }
    }

    pub fn diagnostics(self, db: &dyn HirDatabase) -> Vec<AnyDiagnostic> {
        let id = match self {
            ModuleDef::Adt(it) => match it {
                Adt::Struct(it) => it.id.into(),
                Adt::Enum(it) => it.id.into(),
                Adt::Union(it) => it.id.into(),
            },
            ModuleDef::Trait(it) => it.id.into(),
            ModuleDef::Function(it) => it.id.into(),
            ModuleDef::TypeAlias(it) => it.id.into(),
            ModuleDef::Module(it) => it.id.into(),
            ModuleDef::Const(it) => it.id.into(),
            ModuleDef::Static(it) => it.id.into(),
            _ => return Vec::new(),
        };

        let module = match self.module(db) {
            Some(it) => it,
            None => return Vec::new(),
        };

        let mut acc = Vec::new();
        for diag in hir_ty::diagnostics::validate_module_item(db, module.id.krate(), id) {
            acc.push(diag.into())
        }
        acc
    }
}

impl Module {
    /// Name of this module.
    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        let def_map = self.id.def_map(db.upcast());
        let parent = def_map[self.id.local_id].parent?;
        def_map[parent].children.iter().find_map(|(name, module_id)| {
            if *module_id == self.id.local_id {
                Some(name.clone())
            } else {
                None
            }
        })
    }

    /// Returns the crate this module is part of.
    pub fn krate(self) -> Crate {
        Crate { id: self.id.krate() }
    }

    /// Topmost parent of this module. Every module has a `crate_root`, but some
    /// might be missing `krate`. This can happen if a module's file is not included
    /// in the module tree of any target in `Cargo.toml`.
    pub fn crate_root(self, db: &dyn HirDatabase) -> Module {
        let def_map = db.crate_def_map(self.id.krate());
        Module { id: def_map.module_id(def_map.root()) }
    }

    /// Iterates over all child modules.
    pub fn children(self, db: &dyn HirDatabase) -> impl Iterator<Item = Module> {
        let def_map = self.id.def_map(db.upcast());
        let children = def_map[self.id.local_id]
            .children
            .iter()
            .map(|(_, module_id)| Module { id: def_map.module_id(*module_id) })
            .collect::<Vec<_>>();
        children.into_iter()
    }

    /// Finds a parent module.
    pub fn parent(self, db: &dyn HirDatabase) -> Option<Module> {
        // FIXME: handle block expressions as modules (their parent is in a different DefMap)
        let def_map = self.id.def_map(db.upcast());
        let parent_id = def_map[self.id.local_id].parent?;
        Some(Module { id: def_map.module_id(parent_id) })
    }

    pub fn path_to_root(self, db: &dyn HirDatabase) -> Vec<Module> {
        let mut res = vec![self];
        let mut curr = self;
        while let Some(next) = curr.parent(db) {
            res.push(next);
            curr = next
        }
        res
    }

    /// Returns a `ModuleScope`: a set of items, visible in this module.
    pub fn scope(
        self,
        db: &dyn HirDatabase,
        visible_from: Option<Module>,
    ) -> Vec<(Name, ScopeDef)> {
        self.id.def_map(db.upcast())[self.id.local_id]
            .scope
            .entries()
            .filter_map(|(name, def)| {
                if let Some(m) = visible_from {
                    let filtered =
                        def.filter_visibility(|vis| vis.is_visible_from(db.upcast(), m.id));
                    if filtered.is_none() && !def.is_none() {
                        None
                    } else {
                        Some((name, filtered))
                    }
                } else {
                    Some((name, def))
                }
            })
            .flat_map(|(name, def)| {
                ScopeDef::all_items(def).into_iter().map(move |item| (name.clone(), item))
            })
            .collect()
    }

    pub fn visibility(self, db: &dyn HirDatabase) -> Visibility {
        let def_map = self.id.def_map(db.upcast());
        let module_data = &def_map[self.id.local_id];
        module_data.visibility
    }

    pub fn visibility_of(self, db: &dyn HirDatabase, def: &ModuleDef) -> Option<Visibility> {
        let def_map = self.id.def_map(db.upcast());
        let module_data = &def_map[self.id.local_id];
        module_data.scope.visibility_of((*def).into())
    }

    pub fn diagnostics(self, db: &dyn HirDatabase, acc: &mut Vec<AnyDiagnostic>) {
        let _p = profile::span("Module::diagnostics").detail(|| {
            format!("{:?}", self.name(db).map_or("<unknown>".into(), |name| name.to_string()))
        });
        let def_map = self.id.def_map(db.upcast());
        for diag in def_map.diagnostics() {
            if diag.in_module != self.id.local_id {
                // FIXME: This is accidentally quadratic.
                continue;
            }
            match &diag.kind {
                DefDiagnosticKind::UnresolvedModule { ast: declaration, candidate } => {
                    let decl = declaration.to_node(db.upcast());
                    acc.push(
                        UnresolvedModule {
                            decl: InFile::new(declaration.file_id, AstPtr::new(&decl)),
                            candidate: candidate.clone(),
                        }
                        .into(),
                    )
                }
                DefDiagnosticKind::UnresolvedExternCrate { ast } => {
                    let item = ast.to_node(db.upcast());
                    acc.push(
                        UnresolvedExternCrate {
                            decl: InFile::new(ast.file_id, AstPtr::new(&item)),
                        }
                        .into(),
                    );
                }

                DefDiagnosticKind::UnresolvedImport { id, index } => {
                    let file_id = id.file_id();
                    let item_tree = id.item_tree(db.upcast());
                    let import = &item_tree[id.value];

                    let use_tree = import.use_tree_to_ast(db.upcast(), file_id, *index);
                    acc.push(
                        UnresolvedImport { decl: InFile::new(file_id, AstPtr::new(&use_tree)) }
                            .into(),
                    );
                }

                DefDiagnosticKind::UnconfiguredCode { ast, cfg, opts } => {
                    let item = ast.to_node(db.upcast());
                    acc.push(
                        InactiveCode {
                            node: ast.with_value(AstPtr::new(&item).into()),
                            cfg: cfg.clone(),
                            opts: opts.clone(),
                        }
                        .into(),
                    );
                }

                DefDiagnosticKind::UnresolvedProcMacro { ast } => {
                    let mut precise_location = None;
                    let (node, name) = match ast {
                        MacroCallKind::FnLike { ast_id, .. } => {
                            let node = ast_id.to_node(db.upcast());
                            (ast_id.with_value(SyntaxNodePtr::from(AstPtr::new(&node))), None)
                        }
                        MacroCallKind::Derive { ast_id, derive_name, .. } => {
                            let node = ast_id.to_node(db.upcast());

                            // Compute the precise location of the macro name's token in the derive
                            // list.
                            // FIXME: This does not handle paths to the macro, but neither does the
                            // rest of r-a.
                            let derive_attrs =
                                node.attrs().filter_map(|attr| match attr.as_simple_call() {
                                    Some((name, args)) if name == "derive" => Some(args),
                                    _ => None,
                                });
                            'outer: for attr in derive_attrs {
                                let tokens =
                                    attr.syntax().children_with_tokens().filter_map(|elem| {
                                        match elem {
                                            syntax::NodeOrToken::Node(_) => None,
                                            syntax::NodeOrToken::Token(tok) => Some(tok),
                                        }
                                    });
                                for token in tokens {
                                    if token.kind() == SyntaxKind::IDENT
                                        && token.text() == derive_name.as_str()
                                    {
                                        precise_location = Some(token.text_range());
                                        break 'outer;
                                    }
                                }
                            }

                            (
                                ast_id.with_value(SyntaxNodePtr::from(AstPtr::new(&node))),
                                Some(derive_name.clone()),
                            )
                        }
                        MacroCallKind::Attr { ast_id, invoc_attr_index, attr_name, .. } => {
                            let node = ast_id.to_node(db.upcast());
                            let attr =
                                node.attrs().nth((*invoc_attr_index) as usize).unwrap_or_else(
                                    || panic!("cannot find attribute #{}", invoc_attr_index),
                                );
                            (
                                ast_id.with_value(SyntaxNodePtr::from(AstPtr::new(&attr))),
                                Some(attr_name.clone()),
                            )
                        }
                    };
                    acc.push(
                        UnresolvedProcMacro { node, precise_location, macro_name: name }.into(),
                    );
                }

                DefDiagnosticKind::UnresolvedMacroCall { ast, path } => {
                    let node = ast.to_node(db.upcast());
                    acc.push(
                        UnresolvedMacroCall {
                            macro_call: InFile::new(ast.file_id, AstPtr::new(&node)),
                            path: path.clone(),
                        }
                        .into(),
                    );
                }

                DefDiagnosticKind::MacroError { ast, message } => {
                    let node = match ast {
                        MacroCallKind::FnLike { ast_id, .. } => {
                            let node = ast_id.to_node(db.upcast());
                            ast_id.with_value(SyntaxNodePtr::from(AstPtr::new(&node)))
                        }
                        MacroCallKind::Derive { ast_id, .. }
                        | MacroCallKind::Attr { ast_id, .. } => {
                            // FIXME: point to the attribute instead, this creates very large diagnostics
                            let node = ast_id.to_node(db.upcast());
                            ast_id.with_value(SyntaxNodePtr::from(AstPtr::new(&node)))
                        }
                    };
                    acc.push(MacroError { node, message: message.clone() }.into());
                }

                DefDiagnosticKind::UnimplementedBuiltinMacro { ast } => {
                    let node = ast.to_node(db.upcast());
                    // Must have a name, otherwise we wouldn't emit it.
                    let name = node.name().expect("unimplemented builtin macro with no name");
                    acc.push(
                        UnimplementedBuiltinMacro {
                            node: ast.with_value(SyntaxNodePtr::from(AstPtr::new(&name))),
                        }
                        .into(),
                    );
                }
            }
        }
        for decl in self.declarations(db) {
            match decl {
                ModuleDef::Function(f) => f.diagnostics(db, acc),
                ModuleDef::Module(m) => {
                    // Only add diagnostics from inline modules
                    if def_map[m.id.local_id].origin.is_inline() {
                        m.diagnostics(db, acc)
                    }
                }
                _ => acc.extend(decl.diagnostics(db)),
            }
        }

        for impl_def in self.impl_defs(db) {
            for item in impl_def.items(db) {
                if let AssocItem::Function(f) = item {
                    f.diagnostics(db, acc);
                }
            }
        }
    }

    pub fn declarations(self, db: &dyn HirDatabase) -> Vec<ModuleDef> {
        let def_map = self.id.def_map(db.upcast());
        def_map[self.id.local_id].scope.declarations().map(ModuleDef::from).collect()
    }

    pub fn impl_defs(self, db: &dyn HirDatabase) -> Vec<Impl> {
        let def_map = self.id.def_map(db.upcast());
        def_map[self.id.local_id].scope.impls().map(Impl::from).collect()
    }

    /// Finds a path that can be used to refer to the given item from within
    /// this module, if possible.
    pub fn find_use_path(self, db: &dyn DefDatabase, item: impl Into<ItemInNs>) -> Option<ModPath> {
        hir_def::find_path::find_path(db, item.into(), self.into())
    }

    /// Finds a path that can be used to refer to the given item from within
    /// this module, if possible. This is used for returning import paths for use-statements.
    pub fn find_use_path_prefixed(
        self,
        db: &dyn DefDatabase,
        item: impl Into<ItemInNs>,
        prefix_kind: PrefixKind,
    ) -> Option<ModPath> {
        hir_def::find_path::find_path_prefixed(db, item.into(), self.into(), prefix_kind)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Field {
    pub(crate) parent: VariantDef,
    pub(crate) id: LocalFieldId,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FieldSource {
    Named(ast::RecordField),
    Pos(ast::TupleField),
}

impl Field {
    pub fn name(&self, db: &dyn HirDatabase) -> Name {
        self.parent.variant_data(db).fields()[self.id].name.clone()
    }

    /// Returns the type as in the signature of the struct (i.e., with
    /// placeholder types for type parameters). Only use this in the context of
    /// the field definition.
    pub fn ty(&self, db: &dyn HirDatabase) -> Type {
        let var_id = self.parent.into();
        let generic_def_id: GenericDefId = match self.parent {
            VariantDef::Struct(it) => it.id.into(),
            VariantDef::Union(it) => it.id.into(),
            VariantDef::Variant(it) => it.parent.id.into(),
        };
        let substs = TyBuilder::type_params_subst(db, generic_def_id);
        let ty = db.field_types(var_id)[self.id].clone().substitute(&Interner, &substs);
        Type::new(db, self.parent.module(db).id.krate(), var_id, ty)
    }

    pub fn parent_def(&self, _db: &dyn HirDatabase) -> VariantDef {
        self.parent
    }
}

impl HasVisibility for Field {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        let variant_data = self.parent.variant_data(db);
        let visibility = &variant_data.fields()[self.id].visibility;
        let parent_id: hir_def::VariantId = self.parent.into();
        visibility.resolve(db.upcast(), &parent_id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Struct {
    pub(crate) id: StructId,
}

impl Struct {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).container }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.struct_data(self.id).name.clone()
    }

    pub fn fields(self, db: &dyn HirDatabase) -> Vec<Field> {
        db.struct_data(self.id)
            .variant_data
            .fields()
            .iter()
            .map(|(id, _)| Field { parent: self.into(), id })
            .collect()
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        Type::from_def(db, self.id.lookup(db.upcast()).container.krate(), self.id)
    }

    pub fn repr(self, db: &dyn HirDatabase) -> Option<ReprKind> {
        db.struct_data(self.id).repr.clone()
    }

    pub fn kind(self, db: &dyn HirDatabase) -> StructKind {
        self.variant_data(db).kind()
    }

    fn variant_data(self, db: &dyn HirDatabase) -> Arc<VariantData> {
        db.struct_data(self.id).variant_data.clone()
    }
}

impl HasVisibility for Struct {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.struct_data(self.id).visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Union {
    pub(crate) id: UnionId,
}

impl Union {
    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.union_data(self.id).name.clone()
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).container }
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        Type::from_def(db, self.id.lookup(db.upcast()).container.krate(), self.id)
    }

    pub fn fields(self, db: &dyn HirDatabase) -> Vec<Field> {
        db.union_data(self.id)
            .variant_data
            .fields()
            .iter()
            .map(|(id, _)| Field { parent: self.into(), id })
            .collect()
    }

    fn variant_data(self, db: &dyn HirDatabase) -> Arc<VariantData> {
        db.union_data(self.id).variant_data.clone()
    }
}

impl HasVisibility for Union {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.union_data(self.id).visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Enum {
    pub(crate) id: EnumId,
}

impl Enum {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).container }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.enum_data(self.id).name.clone()
    }

    pub fn variants(self, db: &dyn HirDatabase) -> Vec<Variant> {
        db.enum_data(self.id).variants.iter().map(|(id, _)| Variant { parent: self, id }).collect()
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        Type::from_def(db, self.id.lookup(db.upcast()).container.krate(), self.id)
    }
}

impl HasVisibility for Enum {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.enum_data(self.id).visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Variant {
    pub(crate) parent: Enum,
    pub(crate) id: LocalEnumVariantId,
}

impl Variant {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.parent.module(db)
    }

    pub fn parent_enum(self, _db: &dyn HirDatabase) -> Enum {
        self.parent
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.enum_data(self.parent.id).variants[self.id].name.clone()
    }

    pub fn fields(self, db: &dyn HirDatabase) -> Vec<Field> {
        self.variant_data(db)
            .fields()
            .iter()
            .map(|(id, _)| Field { parent: self.into(), id })
            .collect()
    }

    pub fn kind(self, db: &dyn HirDatabase) -> StructKind {
        self.variant_data(db).kind()
    }

    pub(crate) fn variant_data(self, db: &dyn HirDatabase) -> Arc<VariantData> {
        db.enum_data(self.parent.id).variants[self.id].variant_data.clone()
    }
}

/// A Data Type
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Adt {
    Struct(Struct),
    Union(Union),
    Enum(Enum),
}
impl_from!(Struct, Union, Enum for Adt);

impl Adt {
    pub fn has_non_default_type_params(self, db: &dyn HirDatabase) -> bool {
        let subst = db.generic_defaults(self.into());
        subst.iter().any(|ty| ty.skip_binders().is_unknown())
    }

    /// Turns this ADT into a type. Any type parameters of the ADT will be
    /// turned into unknown types, which is good for e.g. finding the most
    /// general set of completions, but will not look very nice when printed.
    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        let id = AdtId::from(self);
        Type::from_def(db, id.module(db.upcast()).krate(), id)
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        match self {
            Adt::Struct(s) => s.module(db),
            Adt::Union(s) => s.module(db),
            Adt::Enum(e) => e.module(db),
        }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        match self {
            Adt::Struct(s) => s.name(db),
            Adt::Union(u) => u.name(db),
            Adt::Enum(e) => e.name(db),
        }
    }
}

impl HasVisibility for Adt {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        match self {
            Adt::Struct(it) => it.visibility(db),
            Adt::Union(it) => it.visibility(db),
            Adt::Enum(it) => it.visibility(db),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VariantDef {
    Struct(Struct),
    Union(Union),
    Variant(Variant),
}
impl_from!(Struct, Union, Variant for VariantDef);

impl VariantDef {
    pub fn fields(self, db: &dyn HirDatabase) -> Vec<Field> {
        match self {
            VariantDef::Struct(it) => it.fields(db),
            VariantDef::Union(it) => it.fields(db),
            VariantDef::Variant(it) => it.fields(db),
        }
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        match self {
            VariantDef::Struct(it) => it.module(db),
            VariantDef::Union(it) => it.module(db),
            VariantDef::Variant(it) => it.module(db),
        }
    }

    pub fn name(&self, db: &dyn HirDatabase) -> Name {
        match self {
            VariantDef::Struct(s) => s.name(db),
            VariantDef::Union(u) => u.name(db),
            VariantDef::Variant(e) => e.name(db),
        }
    }

    pub(crate) fn variant_data(self, db: &dyn HirDatabase) -> Arc<VariantData> {
        match self {
            VariantDef::Struct(it) => it.variant_data(db),
            VariantDef::Union(it) => it.variant_data(db),
            VariantDef::Variant(it) => it.variant_data(db),
        }
    }
}

/// The defs which have a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DefWithBody {
    Function(Function),
    Static(Static),
    Const(Const),
}
impl_from!(Function, Const, Static for DefWithBody);

impl DefWithBody {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        match self {
            DefWithBody::Const(c) => c.module(db),
            DefWithBody::Function(f) => f.module(db),
            DefWithBody::Static(s) => s.module(db),
        }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        match self {
            DefWithBody::Function(f) => Some(f.name(db)),
            DefWithBody::Static(s) => s.name(db),
            DefWithBody::Const(c) => c.name(db),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Function {
    pub(crate) id: FunctionId,
}

impl Function {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.lookup(db.upcast()).module(db.upcast()).into()
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.function_data(self.id).name.clone()
    }

    /// Get this function's return type
    pub fn ret_type(self, db: &dyn HirDatabase) -> Type {
        let resolver = self.id.resolver(db.upcast());
        let krate = self.id.lookup(db.upcast()).container.module(db.upcast()).krate();
        let ret_type = &db.function_data(self.id).ret_type;
        let ctx = hir_ty::TyLoweringContext::new(db, &resolver);
        let ty = ctx.lower_ty(ret_type);
        Type::new_with_resolver_inner(db, krate, &resolver, ty)
    }

    pub fn self_param(self, db: &dyn HirDatabase) -> Option<SelfParam> {
        if !db.function_data(self.id).has_self_param() {
            return None;
        }
        Some(SelfParam { func: self.id })
    }

    pub fn assoc_fn_params(self, db: &dyn HirDatabase) -> Vec<Param> {
        let resolver = self.id.resolver(db.upcast());
        let krate = self.id.lookup(db.upcast()).container.module(db.upcast()).krate();
        let ctx = hir_ty::TyLoweringContext::new(db, &resolver);
        let environment = db.trait_environment(self.id.into());
        db.function_data(self.id)
            .params
            .iter()
            .enumerate()
            .map(|(idx, type_ref)| {
                let ty = Type { krate, env: environment.clone(), ty: ctx.lower_ty(type_ref) };
                Param { func: self, ty, idx }
            })
            .collect()
    }

    pub fn method_params(self, db: &dyn HirDatabase) -> Option<Vec<Param>> {
        if self.self_param(db).is_none() {
            return None;
        }
        let mut res = self.assoc_fn_params(db);
        res.remove(0);
        Some(res)
    }

    pub fn is_unsafe(self, db: &dyn HirDatabase) -> bool {
        db.function_data(self.id).is_unsafe()
    }

    pub fn is_async(self, db: &dyn HirDatabase) -> bool {
        db.function_data(self.id).is_async()
    }

    pub fn diagnostics(self, db: &dyn HirDatabase, acc: &mut Vec<AnyDiagnostic>) {
        let krate = self.module(db).id.krate();

        let source_map = db.body_with_source_map(self.id.into()).1;
        for diag in source_map.diagnostics() {
            match diag {
                BodyDiagnostic::InactiveCode { node, cfg, opts } => acc.push(
                    InactiveCode { node: node.clone(), cfg: cfg.clone(), opts: opts.clone() }
                        .into(),
                ),
                BodyDiagnostic::MacroError { node, message } => acc.push(
                    MacroError {
                        node: node.clone().map(|it| it.into()),
                        message: message.to_string(),
                    }
                    .into(),
                ),
                BodyDiagnostic::UnresolvedProcMacro { node } => acc.push(
                    UnresolvedProcMacro {
                        node: node.clone().map(|it| it.into()),
                        precise_location: None,
                        macro_name: None,
                    }
                    .into(),
                ),
                BodyDiagnostic::UnresolvedMacroCall { node, path } => acc.push(
                    UnresolvedMacroCall { macro_call: node.clone(), path: path.clone() }.into(),
                ),
            }
        }

        let infer = db.infer(self.id.into());
        let source_map = Lazy::new(|| db.body_with_source_map(self.id.into()).1);
        for d in &infer.diagnostics {
            match d {
                hir_ty::InferenceDiagnostic::NoSuchField { expr } => {
                    let field = source_map.field_syntax(*expr);
                    acc.push(NoSuchField { field }.into())
                }
                hir_ty::InferenceDiagnostic::BreakOutsideOfLoop { expr } => {
                    let expr = source_map
                        .expr_syntax(*expr)
                        .expect("break outside of loop in synthetic syntax");
                    acc.push(BreakOutsideOfLoop { expr }.into())
                }
            }
        }

        for expr in hir_ty::diagnostics::missing_unsafe(db, self.id.into()) {
            match source_map.expr_syntax(expr) {
                Ok(expr) => acc.push(MissingUnsafe { expr }.into()),
                Err(SyntheticSyntax) => {
                    // FIXME: Here and eslwhere in this file, the `expr` was
                    // desugared, report or assert that this doesn't happen.
                }
            }
        }

        for diagnostic in BodyValidationDiagnostic::collect(db, self.id.into()) {
            match diagnostic {
                BodyValidationDiagnostic::RecordMissingFields {
                    record,
                    variant,
                    missed_fields,
                } => {
                    let variant_data = variant.variant_data(db.upcast());
                    let missed_fields = missed_fields
                        .into_iter()
                        .map(|idx| variant_data.fields()[idx].name.clone())
                        .collect();

                    match record {
                        Either::Left(record_expr) => match source_map.expr_syntax(record_expr) {
                            Ok(source_ptr) => {
                                let root = source_ptr.file_syntax(db.upcast());
                                if let ast::Expr::RecordExpr(record_expr) =
                                    &source_ptr.value.to_node(&root)
                                {
                                    if let Some(_) = record_expr.record_expr_field_list() {
                                        acc.push(
                                            MissingFields {
                                                file: source_ptr.file_id,
                                                field_list_parent: Either::Left(AstPtr::new(
                                                    record_expr,
                                                )),
                                                field_list_parent_path: record_expr
                                                    .path()
                                                    .map(|path| AstPtr::new(&path)),
                                                missed_fields,
                                            }
                                            .into(),
                                        )
                                    }
                                }
                            }
                            Err(SyntheticSyntax) => (),
                        },
                        Either::Right(record_pat) => match source_map.pat_syntax(record_pat) {
                            Ok(source_ptr) => {
                                if let Some(expr) = source_ptr.value.as_ref().left() {
                                    let root = source_ptr.file_syntax(db.upcast());
                                    if let ast::Pat::RecordPat(record_pat) = expr.to_node(&root) {
                                        if let Some(_) = record_pat.record_pat_field_list() {
                                            acc.push(
                                                MissingFields {
                                                    file: source_ptr.file_id,
                                                    field_list_parent: Either::Right(AstPtr::new(
                                                        &record_pat,
                                                    )),
                                                    field_list_parent_path: record_pat
                                                        .path()
                                                        .map(|path| AstPtr::new(&path)),
                                                    missed_fields,
                                                }
                                                .into(),
                                            )
                                        }
                                    }
                                }
                            }
                            Err(SyntheticSyntax) => (),
                        },
                    }
                }
                BodyValidationDiagnostic::ReplaceFilterMapNextWithFindMap { method_call_expr } => {
                    if let Ok(next_source_ptr) = source_map.expr_syntax(method_call_expr) {
                        acc.push(
                            ReplaceFilterMapNextWithFindMap {
                                file: next_source_ptr.file_id,
                                next_expr: next_source_ptr.value,
                            }
                            .into(),
                        );
                    }
                }
                BodyValidationDiagnostic::MismatchedArgCount { call_expr, expected, found } => {
                    match source_map.expr_syntax(call_expr) {
                        Ok(source_ptr) => acc.push(
                            MismatchedArgCount { call_expr: source_ptr, expected, found }.into(),
                        ),
                        Err(SyntheticSyntax) => (),
                    }
                }
                BodyValidationDiagnostic::RemoveThisSemicolon { expr } => {
                    match source_map.expr_syntax(expr) {
                        Ok(expr) => acc.push(RemoveThisSemicolon { expr }.into()),
                        Err(SyntheticSyntax) => (),
                    }
                }
                BodyValidationDiagnostic::MissingOkOrSomeInTailExpr { expr, required } => {
                    match source_map.expr_syntax(expr) {
                        Ok(expr) => acc.push(MissingOkOrSomeInTailExpr { expr, required }.into()),
                        Err(SyntheticSyntax) => (),
                    }
                }
                BodyValidationDiagnostic::MissingMatchArms { match_expr } => {
                    match source_map.expr_syntax(match_expr) {
                        Ok(source_ptr) => {
                            let root = source_ptr.file_syntax(db.upcast());
                            if let ast::Expr::MatchExpr(match_expr) =
                                &source_ptr.value.to_node(&root)
                            {
                                if let (Some(match_expr), Some(arms)) =
                                    (match_expr.expr(), match_expr.match_arm_list())
                                {
                                    acc.push(
                                        MissingMatchArms {
                                            file: source_ptr.file_id,
                                            match_expr: AstPtr::new(&match_expr),
                                            arms: AstPtr::new(&arms),
                                        }
                                        .into(),
                                    )
                                }
                            }
                        }
                        Err(SyntheticSyntax) => (),
                    }
                }
            }
        }

        for diag in hir_ty::diagnostics::validate_module_item(db, krate, self.id.into()) {
            acc.push(diag.into())
        }
    }

    /// Whether this function declaration has a definition.
    ///
    /// This is false in the case of required (not provided) trait methods.
    pub fn has_body(self, db: &dyn HirDatabase) -> bool {
        db.function_data(self.id).has_body()
    }

    /// A textual representation of the HIR of this function for debugging purposes.
    pub fn debug_hir(self, db: &dyn HirDatabase) -> String {
        let body = db.body(self.id.into());

        let mut result = String::new();
        format_to!(result, "HIR expressions in the body of `{}`:\n", self.name(db));
        for (id, expr) in body.exprs.iter() {
            format_to!(result, "{:?}: {:?}\n", id, expr);
        }

        result
    }
}

// Note: logically, this belongs to `hir_ty`, but we are not using it there yet.
pub enum Access {
    Shared,
    Exclusive,
    Owned,
}

impl From<hir_ty::Mutability> for Access {
    fn from(mutability: hir_ty::Mutability) -> Access {
        match mutability {
            hir_ty::Mutability::Not => Access::Shared,
            hir_ty::Mutability::Mut => Access::Exclusive,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Param {
    func: Function,
    /// The index in parameter list, including self parameter.
    idx: usize,
    ty: Type,
}

impl Param {
    pub fn ty(&self) -> &Type {
        &self.ty
    }

    pub fn as_local(&self, db: &dyn HirDatabase) -> Local {
        let parent = DefWithBodyId::FunctionId(self.func.into());
        let body = db.body(parent);
        Local { parent, pat_id: body.params[self.idx] }
    }

    pub fn pattern_source(&self, db: &dyn HirDatabase) -> Option<ast::Pat> {
        self.source(db).and_then(|p| p.value.pat())
    }

    pub fn source(&self, db: &dyn HirDatabase) -> Option<InFile<ast::Param>> {
        let InFile { file_id, value } = self.func.source(db)?;
        let params = value.param_list()?;
        if params.self_param().is_some() {
            params.params().nth(self.idx.checked_sub(1)?)
        } else {
            params.params().nth(self.idx)
        }
        .map(|value| InFile { file_id, value })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SelfParam {
    func: FunctionId,
}

impl SelfParam {
    pub fn access(self, db: &dyn HirDatabase) -> Access {
        let func_data = db.function_data(self.func);
        func_data
            .params
            .first()
            .map(|param| match &**param {
                TypeRef::Reference(.., mutability) => match mutability {
                    hir_def::type_ref::Mutability::Shared => Access::Shared,
                    hir_def::type_ref::Mutability::Mut => Access::Exclusive,
                },
                _ => Access::Owned,
            })
            .unwrap_or(Access::Owned)
    }

    pub fn display(self, db: &dyn HirDatabase) -> &'static str {
        match self.access(db) {
            Access::Shared => "&self",
            Access::Exclusive => "&mut self",
            Access::Owned => "self",
        }
    }

    pub fn source(&self, db: &dyn HirDatabase) -> Option<InFile<ast::SelfParam>> {
        let InFile { file_id, value } = Function::from(self.func).source(db)?;
        value
            .param_list()
            .and_then(|params| params.self_param())
            .map(|value| InFile { file_id, value })
    }
}

impl HasVisibility for Function {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        let function_data = db.function_data(self.id);
        let visibility = &function_data.visibility;
        visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Const {
    pub(crate) id: ConstId,
}

impl Const {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).module(db.upcast()) }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        db.const_data(self.id).name.clone()
    }

    pub fn type_ref(self, db: &dyn HirDatabase) -> TypeRef {
        db.const_data(self.id).type_ref.as_ref().clone()
    }
}

impl HasVisibility for Const {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        let function_data = db.const_data(self.id);
        let visibility = &function_data.visibility;
        visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Static {
    pub(crate) id: StaticId,
}

impl Static {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).module(db.upcast()) }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        db.static_data(self.id).name.clone()
    }

    pub fn is_mut(self, db: &dyn HirDatabase) -> bool {
        db.static_data(self.id).mutable
    }
}

impl HasVisibility for Static {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.static_data(self.id).visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Trait {
    pub(crate) id: TraitId,
}

impl Trait {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).container }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.trait_data(self.id).name.clone()
    }

    pub fn items(self, db: &dyn HirDatabase) -> Vec<AssocItem> {
        db.trait_data(self.id).items.iter().map(|(_name, it)| (*it).into()).collect()
    }

    pub fn is_auto(self, db: &dyn HirDatabase) -> bool {
        db.trait_data(self.id).is_auto
    }

    pub fn is_unsafe(&self, db: &dyn HirDatabase) -> bool {
        db.trait_data(self.id).is_unsafe
    }
}

impl HasVisibility for Trait {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        db.trait_data(self.id).visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeAlias {
    pub(crate) id: TypeAliasId,
}

impl TypeAlias {
    pub fn has_non_default_type_params(self, db: &dyn HirDatabase) -> bool {
        let subst = db.generic_defaults(self.id.into());
        subst.iter().any(|ty| ty.skip_binders().is_unknown())
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        Module { id: self.id.lookup(db.upcast()).module(db.upcast()) }
    }

    pub fn type_ref(self, db: &dyn HirDatabase) -> Option<TypeRef> {
        db.type_alias_data(self.id).type_ref.as_deref().cloned()
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        Type::from_def(db, self.id.lookup(db.upcast()).module(db.upcast()).krate(), self.id)
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        db.type_alias_data(self.id).name.clone()
    }
}

impl HasVisibility for TypeAlias {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        let function_data = db.type_alias_data(self.id);
        let visibility = &function_data.visibility;
        visibility.resolve(db.upcast(), &self.id.resolver(db.upcast()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BuiltinType {
    pub(crate) inner: hir_def::builtin_type::BuiltinType,
}

impl BuiltinType {
    pub fn ty(self, db: &dyn HirDatabase, module: Module) -> Type {
        let resolver = module.id.resolver(db.upcast());
        Type::new_with_resolver(db, &resolver, TyBuilder::builtin(self.inner))
            .expect("crate not present in resolver")
    }

    pub fn name(self) -> Name {
        self.inner.as_name()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MacroKind {
    /// `macro_rules!` or Macros 2.0 macro.
    Declarative,
    /// A built-in or custom derive.
    Derive,
    /// A built-in function-like macro.
    BuiltIn,
    /// A procedural attribute macro.
    Attr,
    /// A function-like procedural macro.
    ProcMacro,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MacroDef {
    pub(crate) id: MacroDefId,
}

impl MacroDef {
    /// FIXME: right now, this just returns the root module of the crate that
    /// defines this macro. The reasons for this is that macros are expanded
    /// early, in `hir_expand`, where modules simply do not exist yet.
    pub fn module(self, db: &dyn HirDatabase) -> Option<Module> {
        let krate = self.id.krate;
        let def_map = db.crate_def_map(krate);
        let module_id = def_map.root();
        Some(Module { id: def_map.module_id(module_id) })
    }

    /// XXX: this parses the file
    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        match self.source(db)?.value {
            Either::Left(it) => it.name().map(|it| it.as_name()),
            Either::Right(it) => it.name().map(|it| it.as_name()),
        }
    }

    pub fn kind(&self) -> MacroKind {
        match self.id.kind {
            MacroDefKind::Declarative(_) => MacroKind::Declarative,
            MacroDefKind::BuiltIn(_, _) | MacroDefKind::BuiltInEager(_, _) => MacroKind::BuiltIn,
            MacroDefKind::BuiltInDerive(_, _) => MacroKind::Derive,
            MacroDefKind::BuiltInAttr(_, _) => MacroKind::Attr,
            MacroDefKind::ProcMacro(_, base_db::ProcMacroKind::CustomDerive, _) => {
                MacroKind::Derive
            }
            MacroDefKind::ProcMacro(_, base_db::ProcMacroKind::Attr, _) => MacroKind::Attr,
            MacroDefKind::ProcMacro(_, base_db::ProcMacroKind::FuncLike, _) => MacroKind::ProcMacro,
        }
    }

    pub fn is_fn_like(&self) -> bool {
        match self.kind() {
            MacroKind::Declarative | MacroKind::BuiltIn | MacroKind::ProcMacro => true,
            MacroKind::Attr | MacroKind::Derive => false,
        }
    }
}

/// Invariant: `inner.as_assoc_item(db).is_some()`
/// We do not actively enforce this invariant.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum AssocItem {
    Function(Function),
    Const(Const),
    TypeAlias(TypeAlias),
}
#[derive(Debug)]
pub enum AssocItemContainer {
    Trait(Trait),
    Impl(Impl),
}
pub trait AsAssocItem {
    fn as_assoc_item(self, db: &dyn HirDatabase) -> Option<AssocItem>;
}

impl AsAssocItem for Function {
    fn as_assoc_item(self, db: &dyn HirDatabase) -> Option<AssocItem> {
        as_assoc_item(db, AssocItem::Function, self.id)
    }
}
impl AsAssocItem for Const {
    fn as_assoc_item(self, db: &dyn HirDatabase) -> Option<AssocItem> {
        as_assoc_item(db, AssocItem::Const, self.id)
    }
}
impl AsAssocItem for TypeAlias {
    fn as_assoc_item(self, db: &dyn HirDatabase) -> Option<AssocItem> {
        as_assoc_item(db, AssocItem::TypeAlias, self.id)
    }
}
impl AsAssocItem for ModuleDef {
    fn as_assoc_item(self, db: &dyn HirDatabase) -> Option<AssocItem> {
        match self {
            ModuleDef::Function(it) => it.as_assoc_item(db),
            ModuleDef::Const(it) => it.as_assoc_item(db),
            ModuleDef::TypeAlias(it) => it.as_assoc_item(db),
            _ => None,
        }
    }
}
fn as_assoc_item<ID, DEF, CTOR, AST>(db: &dyn HirDatabase, ctor: CTOR, id: ID) -> Option<AssocItem>
where
    ID: Lookup<Data = AssocItemLoc<AST>>,
    DEF: From<ID>,
    CTOR: FnOnce(DEF) -> AssocItem,
    AST: ItemTreeNode,
{
    match id.lookup(db.upcast()).container {
        AssocContainerId::TraitId(_) | AssocContainerId::ImplId(_) => Some(ctor(DEF::from(id))),
        AssocContainerId::ModuleId(_) => None,
    }
}

impl AssocItem {
    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        match self {
            AssocItem::Function(it) => Some(it.name(db)),
            AssocItem::Const(it) => it.name(db),
            AssocItem::TypeAlias(it) => Some(it.name(db)),
        }
    }
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        match self {
            AssocItem::Function(f) => f.module(db),
            AssocItem::Const(c) => c.module(db),
            AssocItem::TypeAlias(t) => t.module(db),
        }
    }
    pub fn container(self, db: &dyn HirDatabase) -> AssocItemContainer {
        let container = match self {
            AssocItem::Function(it) => it.id.lookup(db.upcast()).container,
            AssocItem::Const(it) => it.id.lookup(db.upcast()).container,
            AssocItem::TypeAlias(it) => it.id.lookup(db.upcast()).container,
        };
        match container {
            AssocContainerId::TraitId(id) => AssocItemContainer::Trait(id.into()),
            AssocContainerId::ImplId(id) => AssocItemContainer::Impl(id.into()),
            AssocContainerId::ModuleId(_) => panic!("invalid AssocItem"),
        }
    }

    pub fn containing_trait(self, db: &dyn HirDatabase) -> Option<Trait> {
        match self.container(db) {
            AssocItemContainer::Trait(t) => Some(t),
            _ => None,
        }
    }

    pub fn containing_trait_impl(self, db: &dyn HirDatabase) -> Option<Trait> {
        match self.container(db) {
            AssocItemContainer::Impl(i) => i.trait_(db),
            _ => None,
        }
    }

    pub fn containing_trait_or_trait_impl(self, db: &dyn HirDatabase) -> Option<Trait> {
        match self.container(db) {
            AssocItemContainer::Trait(t) => Some(t),
            AssocItemContainer::Impl(i) => i.trait_(db),
        }
    }
}

impl HasVisibility for AssocItem {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility {
        match self {
            AssocItem::Function(f) => f.visibility(db),
            AssocItem::Const(c) => c.visibility(db),
            AssocItem::TypeAlias(t) => t.visibility(db),
        }
    }
}

impl From<AssocItem> for ModuleDef {
    fn from(assoc: AssocItem) -> Self {
        match assoc {
            AssocItem::Function(it) => ModuleDef::Function(it),
            AssocItem::Const(it) => ModuleDef::Const(it),
            AssocItem::TypeAlias(it) => ModuleDef::TypeAlias(it),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum GenericDef {
    Function(Function),
    Adt(Adt),
    Trait(Trait),
    TypeAlias(TypeAlias),
    Impl(Impl),
    // enum variants cannot have generics themselves, but their parent enums
    // can, and this makes some code easier to write
    Variant(Variant),
    // consts can have type parameters from their parents (i.e. associated consts of traits)
    Const(Const),
}
impl_from!(
    Function,
    Adt(Struct, Enum, Union),
    Trait,
    TypeAlias,
    Impl,
    Variant,
    Const
    for GenericDef
);

impl GenericDef {
    pub fn params(self, db: &dyn HirDatabase) -> Vec<GenericParam> {
        let generics = db.generic_params(self.into());
        let ty_params = generics
            .types
            .iter()
            .map(|(local_id, _)| TypeParam { id: TypeParamId { parent: self.into(), local_id } })
            .map(GenericParam::TypeParam);
        let lt_params = generics
            .lifetimes
            .iter()
            .map(|(local_id, _)| LifetimeParam {
                id: LifetimeParamId { parent: self.into(), local_id },
            })
            .map(GenericParam::LifetimeParam);
        let const_params = generics
            .consts
            .iter()
            .map(|(local_id, _)| ConstParam { id: ConstParamId { parent: self.into(), local_id } })
            .map(GenericParam::ConstParam);
        ty_params.chain(lt_params).chain(const_params).collect()
    }

    pub fn type_params(self, db: &dyn HirDatabase) -> Vec<TypeParam> {
        let generics = db.generic_params(self.into());
        generics
            .types
            .iter()
            .map(|(local_id, _)| TypeParam { id: TypeParamId { parent: self.into(), local_id } })
            .collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Local {
    pub(crate) parent: DefWithBodyId,
    pub(crate) pat_id: PatId,
}

impl Local {
    pub fn is_param(self, db: &dyn HirDatabase) -> bool {
        let src = self.source(db);
        match src.value {
            Either::Left(bind_pat) => {
                bind_pat.syntax().ancestors().any(|it| ast::Param::can_cast(it.kind()))
            }
            Either::Right(_self_param) => true,
        }
    }

    pub fn as_self_param(self, db: &dyn HirDatabase) -> Option<SelfParam> {
        match self.parent {
            DefWithBodyId::FunctionId(func) if self.is_self(db) => Some(SelfParam { func }),
            _ => None,
        }
    }

    // FIXME: why is this an option? It shouldn't be?
    pub fn name(self, db: &dyn HirDatabase) -> Option<Name> {
        let body = db.body(self.parent);
        match &body[self.pat_id] {
            Pat::Bind { name, .. } => Some(name.clone()),
            _ => None,
        }
    }

    pub fn is_self(self, db: &dyn HirDatabase) -> bool {
        self.name(db) == Some(name![self])
    }

    pub fn is_mut(self, db: &dyn HirDatabase) -> bool {
        let body = db.body(self.parent);
        matches!(&body[self.pat_id], Pat::Bind { mode: BindingAnnotation::Mutable, .. })
    }

    pub fn parent(self, _db: &dyn HirDatabase) -> DefWithBody {
        self.parent.into()
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.parent(db).module(db)
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        let def = self.parent;
        let infer = db.infer(def);
        let ty = infer[self.pat_id].clone();
        let krate = def.module(db.upcast()).krate();
        Type::new(db, krate, def, ty)
    }

    pub fn source(self, db: &dyn HirDatabase) -> InFile<Either<ast::IdentPat, ast::SelfParam>> {
        let (_body, source_map) = db.body_with_source_map(self.parent);
        let src = source_map.pat_syntax(self.pat_id).unwrap(); // Hmm...
        let root = src.file_syntax(db.upcast());
        src.map(|ast| {
            ast.map_left(|it| it.cast().unwrap().to_node(&root)).map_right(|it| it.to_node(&root))
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Label {
    pub(crate) parent: DefWithBodyId,
    pub(crate) label_id: LabelId,
}

impl Label {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.parent(db).module(db)
    }

    pub fn parent(self, _db: &dyn HirDatabase) -> DefWithBody {
        self.parent.into()
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        let body = db.body(self.parent);
        body[self.label_id].name.clone()
    }

    pub fn source(self, db: &dyn HirDatabase) -> InFile<ast::Label> {
        let (_body, source_map) = db.body_with_source_map(self.parent);
        let src = source_map.label_syntax(self.label_id);
        let root = src.file_syntax(db.upcast());
        src.map(|ast| ast.to_node(&root))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GenericParam {
    TypeParam(TypeParam),
    LifetimeParam(LifetimeParam),
    ConstParam(ConstParam),
}
impl_from!(TypeParam, LifetimeParam, ConstParam for GenericParam);

impl GenericParam {
    pub fn module(self, db: &dyn HirDatabase) -> Module {
        match self {
            GenericParam::TypeParam(it) => it.module(db),
            GenericParam::LifetimeParam(it) => it.module(db),
            GenericParam::ConstParam(it) => it.module(db),
        }
    }

    pub fn name(self, db: &dyn HirDatabase) -> Name {
        match self {
            GenericParam::TypeParam(it) => it.name(db),
            GenericParam::LifetimeParam(it) => it.name(db),
            GenericParam::ConstParam(it) => it.name(db),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypeParam {
    pub(crate) id: TypeParamId,
}

impl TypeParam {
    pub fn name(self, db: &dyn HirDatabase) -> Name {
        let params = db.generic_params(self.id.parent);
        params.types[self.id.local_id].name.clone().unwrap_or_else(Name::missing)
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.parent.module(db.upcast()).into()
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        let resolver = self.id.parent.resolver(db.upcast());
        let krate = self.id.parent.module(db.upcast()).krate();
        let ty = TyKind::Placeholder(hir_ty::to_placeholder_idx(db, self.id)).intern(&Interner);
        Type::new_with_resolver_inner(db, krate, &resolver, ty)
    }

    pub fn trait_bounds(self, db: &dyn HirDatabase) -> Vec<Trait> {
        db.generic_predicates_for_param(self.id)
            .iter()
            .filter_map(|pred| match &pred.skip_binders().skip_binders() {
                hir_ty::WhereClause::Implemented(trait_ref) => {
                    Some(Trait::from(trait_ref.hir_trait_id()))
                }
                _ => None,
            })
            .collect()
    }

    pub fn default(self, db: &dyn HirDatabase) -> Option<Type> {
        let params = db.generic_defaults(self.id.parent);
        let local_idx = hir_ty::param_idx(db, self.id)?;
        let resolver = self.id.parent.resolver(db.upcast());
        let krate = self.id.parent.module(db.upcast()).krate();
        let ty = params.get(local_idx)?.clone();
        let subst = TyBuilder::type_params_subst(db, self.id.parent);
        let ty = ty.substitute(&Interner, &subst_prefix(&subst, local_idx));
        Some(Type::new_with_resolver_inner(db, krate, &resolver, ty))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LifetimeParam {
    pub(crate) id: LifetimeParamId,
}

impl LifetimeParam {
    pub fn name(self, db: &dyn HirDatabase) -> Name {
        let params = db.generic_params(self.id.parent);
        params.lifetimes[self.id.local_id].name.clone()
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.parent.module(db.upcast()).into()
    }

    pub fn parent(self, _db: &dyn HirDatabase) -> GenericDef {
        self.id.parent.into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConstParam {
    pub(crate) id: ConstParamId,
}

impl ConstParam {
    pub fn name(self, db: &dyn HirDatabase) -> Name {
        let params = db.generic_params(self.id.parent);
        params.consts[self.id.local_id].name.clone()
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.parent.module(db.upcast()).into()
    }

    pub fn parent(self, _db: &dyn HirDatabase) -> GenericDef {
        self.id.parent.into()
    }

    pub fn ty(self, db: &dyn HirDatabase) -> Type {
        let def = self.id.parent;
        let krate = def.module(db.upcast()).krate();
        Type::new(db, krate, def, db.const_param_ty(self.id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Impl {
    pub(crate) id: ImplId,
}

impl Impl {
    pub fn all_in_crate(db: &dyn HirDatabase, krate: Crate) -> Vec<Impl> {
        let inherent = db.inherent_impls_in_crate(krate.id);
        let trait_ = db.trait_impls_in_crate(krate.id);

        inherent.all_impls().chain(trait_.all_impls()).map(Self::from).collect()
    }

    pub fn all_for_type(db: &dyn HirDatabase, Type { krate, ty, .. }: Type) -> Vec<Impl> {
        let def_crates = match method_resolution::def_crates(db, &ty, krate) {
            Some(def_crates) => def_crates,
            None => return Vec::new(),
        };

        let filter = |impl_def: &Impl| {
            let self_ty = impl_def.self_ty(db);
            let rref = self_ty.remove_ref();
            ty.equals_ctor(rref.as_ref().map_or(&self_ty.ty, |it| &it.ty))
        };

        let fp = TyFingerprint::for_inherent_impl(&ty);
        let fp = if let Some(fp) = fp {
            fp
        } else {
            return Vec::new();
        };

        let mut all = Vec::new();
        def_crates.iter().for_each(|&id| {
            all.extend(
                db.inherent_impls_in_crate(id)
                    .for_self_ty(&ty)
                    .iter()
                    .cloned()
                    .map(Self::from)
                    .filter(filter),
            )
        });
        for id in def_crates
            .iter()
            .flat_map(|&id| Crate { id }.transitive_reverse_dependencies(db))
            .map(|Crate { id }| id)
            .chain(def_crates.iter().copied())
            .unique()
        {
            all.extend(
                db.trait_impls_in_crate(id)
                    .for_self_ty_without_blanket_impls(fp)
                    .map(Self::from)
                    .filter(filter),
            );
        }
        all
    }

    pub fn all_for_trait(db: &dyn HirDatabase, trait_: Trait) -> Vec<Impl> {
        let krate = trait_.module(db).krate();
        let mut all = Vec::new();
        for Crate { id } in krate.transitive_reverse_dependencies(db).into_iter() {
            let impls = db.trait_impls_in_crate(id);
            all.extend(impls.for_trait(trait_.id).map(Self::from))
        }
        all
    }

    // FIXME: the return type is wrong. This should be a hir version of
    // `TraitRef` (to account for parameters and qualifiers)
    pub fn trait_(self, db: &dyn HirDatabase) -> Option<Trait> {
        let trait_ref = db.impl_trait(self.id)?.skip_binders().clone();
        let id = hir_ty::from_chalk_trait_id(trait_ref.trait_id);
        Some(Trait { id })
    }

    pub fn self_ty(self, db: &dyn HirDatabase) -> Type {
        let impl_data = db.impl_data(self.id);
        let resolver = self.id.resolver(db.upcast());
        let krate = self.id.lookup(db.upcast()).container.krate();
        let ctx = hir_ty::TyLoweringContext::new(db, &resolver);
        let ty = ctx.lower_ty(&impl_data.self_ty);
        Type::new_with_resolver_inner(db, krate, &resolver, ty)
    }

    pub fn items(self, db: &dyn HirDatabase) -> Vec<AssocItem> {
        db.impl_data(self.id).items.iter().map(|it| (*it).into()).collect()
    }

    pub fn is_negative(self, db: &dyn HirDatabase) -> bool {
        db.impl_data(self.id).is_negative
    }

    pub fn module(self, db: &dyn HirDatabase) -> Module {
        self.id.lookup(db.upcast()).container.into()
    }

    pub fn is_builtin_derive(self, db: &dyn HirDatabase) -> Option<InFile<ast::Attr>> {
        let src = self.source(db)?;
        let item = src.file_id.is_builtin_derive(db.upcast())?;
        let hygenic = hir_expand::hygiene::Hygiene::new(db.upcast(), item.file_id);

        // FIXME: handle `cfg_attr`
        let attr = item
            .value
            .attrs()
            .filter_map(|it| {
                let path = ModPath::from_src(db.upcast(), it.path()?, &hygenic)?;
                if path.as_ident()?.to_string() == "derive" {
                    Some(it)
                } else {
                    None
                }
            })
            .last()?;

        Some(item.with_value(attr))
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Type {
    krate: CrateId,
    env: Arc<TraitEnvironment>,
    ty: Ty,
}

impl Type {
    pub(crate) fn new_with_resolver(
        db: &dyn HirDatabase,
        resolver: &Resolver,
        ty: Ty,
    ) -> Option<Type> {
        let krate = resolver.krate()?;
        Some(Type::new_with_resolver_inner(db, krate, resolver, ty))
    }
    pub(crate) fn new_with_resolver_inner(
        db: &dyn HirDatabase,
        krate: CrateId,
        resolver: &Resolver,
        ty: Ty,
    ) -> Type {
        let environment = resolver
            .generic_def()
            .map_or_else(|| Arc::new(TraitEnvironment::empty(krate)), |d| db.trait_environment(d));
        Type { krate, env: environment, ty }
    }

    fn new(db: &dyn HirDatabase, krate: CrateId, lexical_env: impl HasResolver, ty: Ty) -> Type {
        let resolver = lexical_env.resolver(db.upcast());
        let environment = resolver
            .generic_def()
            .map_or_else(|| Arc::new(TraitEnvironment::empty(krate)), |d| db.trait_environment(d));
        Type { krate, env: environment, ty }
    }

    fn from_def(
        db: &dyn HirDatabase,
        krate: CrateId,
        def: impl HasResolver + Into<TyDefId>,
    ) -> Type {
        let ty = TyBuilder::def_ty(db, def.into()).fill_with_unknown().build();
        Type::new(db, krate, def, ty)
    }

    pub fn is_unit(&self) -> bool {
        matches!(self.ty.kind(&Interner), TyKind::Tuple(0, ..))
    }

    pub fn is_bool(&self) -> bool {
        matches!(self.ty.kind(&Interner), TyKind::Scalar(Scalar::Bool))
    }

    pub fn is_never(&self) -> bool {
        matches!(self.ty.kind(&Interner), TyKind::Never)
    }

    pub fn is_mutable_reference(&self) -> bool {
        matches!(self.ty.kind(&Interner), TyKind::Ref(hir_ty::Mutability::Mut, ..))
    }

    pub fn is_usize(&self) -> bool {
        matches!(self.ty.kind(&Interner), TyKind::Scalar(Scalar::Uint(UintTy::Usize)))
    }

    pub fn remove_ref(&self) -> Option<Type> {
        match &self.ty.kind(&Interner) {
            TyKind::Ref(.., ty) => Some(self.derived(ty.clone())),
            _ => None,
        }
    }

    pub fn strip_references(&self) -> Type {
        self.derived(self.ty.strip_references().clone())
    }

    pub fn is_unknown(&self) -> bool {
        self.ty.is_unknown()
    }

    /// Checks that particular type `ty` implements `std::future::Future`.
    /// This function is used in `.await` syntax completion.
    pub fn impls_future(&self, db: &dyn HirDatabase) -> bool {
        // No special case for the type of async block, since Chalk can figure it out.

        let krate = self.krate;

        let std_future_trait =
            db.lang_item(krate, "future_trait".into()).and_then(|it| it.as_trait());
        let std_future_trait = match std_future_trait {
            Some(it) => it,
            None => return false,
        };

        let canonical_ty =
            Canonical { value: self.ty.clone(), binders: CanonicalVarKinds::empty(&Interner) };
        method_resolution::implements_trait(
            &canonical_ty,
            db,
            self.env.clone(),
            krate,
            std_future_trait,
        )
    }

    /// Checks that particular type `ty` implements `std::ops::FnOnce`.
    ///
    /// This function can be used to check if a particular type is callable, since FnOnce is a
    /// supertrait of Fn and FnMut, so all callable types implements at least FnOnce.
    pub fn impls_fnonce(&self, db: &dyn HirDatabase) -> bool {
        let krate = self.krate;

        let fnonce_trait = match FnTrait::FnOnce.get_id(db, krate) {
            Some(it) => it,
            None => return false,
        };

        let canonical_ty =
            Canonical { value: self.ty.clone(), binders: CanonicalVarKinds::empty(&Interner) };
        method_resolution::implements_trait_unique(
            &canonical_ty,
            db,
            self.env.clone(),
            krate,
            fnonce_trait,
        )
    }

    pub fn impls_trait(&self, db: &dyn HirDatabase, trait_: Trait, args: &[Type]) -> bool {
        let trait_ref = TyBuilder::trait_ref(db, trait_.id)
            .push(self.ty.clone())
            .fill(args.iter().map(|t| t.ty.clone()))
            .build();

        let goal = Canonical {
            value: hir_ty::InEnvironment::new(&self.env.env, trait_ref.cast(&Interner)),
            binders: CanonicalVarKinds::empty(&Interner),
        };

        db.trait_solve(self.krate, goal).is_some()
    }

    pub fn normalize_trait_assoc_type(
        &self,
        db: &dyn HirDatabase,
        args: &[Type],
        alias: TypeAlias,
    ) -> Option<Type> {
        let projection = TyBuilder::assoc_type_projection(db, alias.id)
            .push(self.ty.clone())
            .fill(args.iter().map(|t| t.ty.clone()))
            .build();
        let goal = hir_ty::make_canonical(
            InEnvironment::new(
                &self.env.env,
                AliasEq {
                    alias: AliasTy::Projection(projection),
                    ty: TyKind::BoundVar(BoundVar::new(DebruijnIndex::INNERMOST, 0))
                        .intern(&Interner),
                }
                .cast(&Interner),
            ),
            [TyVariableKind::General].iter().copied(),
        );

        match db.trait_solve(self.krate, goal)? {
            Solution::Unique(s) => s
                .value
                .subst
                .as_slice(&Interner)
                .first()
                .map(|ty| self.derived(ty.assert_ty_ref(&Interner).clone())),
            Solution::Ambig(_) => None,
        }
    }

    pub fn is_copy(&self, db: &dyn HirDatabase) -> bool {
        let lang_item = db.lang_item(self.krate, SmolStr::new("copy"));
        let copy_trait = match lang_item {
            Some(LangItemTarget::TraitId(it)) => it,
            _ => return false,
        };
        self.impls_trait(db, copy_trait.into(), &[])
    }

    pub fn as_callable(&self, db: &dyn HirDatabase) -> Option<Callable> {
        let def = self.ty.callable_def(db);

        let sig = self.ty.callable_sig(db)?;
        Some(Callable { ty: self.clone(), sig, def, is_bound_method: false })
    }

    pub fn is_closure(&self) -> bool {
        matches!(&self.ty.kind(&Interner), TyKind::Closure { .. })
    }

    pub fn is_fn(&self) -> bool {
        matches!(&self.ty.kind(&Interner), TyKind::FnDef(..) | TyKind::Function { .. })
    }

    pub fn is_packed(&self, db: &dyn HirDatabase) -> bool {
        let adt_id = match *self.ty.kind(&Interner) {
            TyKind::Adt(hir_ty::AdtId(adt_id), ..) => adt_id,
            _ => return false,
        };

        let adt = adt_id.into();
        match adt {
            Adt::Struct(s) => matches!(s.repr(db), Some(ReprKind::Packed)),
            _ => false,
        }
    }

    pub fn is_raw_ptr(&self) -> bool {
        matches!(&self.ty.kind(&Interner), TyKind::Raw(..))
    }

    pub fn contains_unknown(&self) -> bool {
        return go(&self.ty);

        fn go(ty: &Ty) -> bool {
            match ty.kind(&Interner) {
                TyKind::Error => true,

                TyKind::Adt(_, substs)
                | TyKind::AssociatedType(_, substs)
                | TyKind::Tuple(_, substs)
                | TyKind::OpaqueType(_, substs)
                | TyKind::FnDef(_, substs)
                | TyKind::Closure(_, substs) => {
                    substs.iter(&Interner).filter_map(|a| a.ty(&Interner)).any(go)
                }

                TyKind::Array(_ty, len) if len.is_unknown() => true,
                TyKind::Array(ty, _)
                | TyKind::Slice(ty)
                | TyKind::Raw(_, ty)
                | TyKind::Ref(_, _, ty) => go(ty),

                TyKind::Scalar(_)
                | TyKind::Str
                | TyKind::Never
                | TyKind::Placeholder(_)
                | TyKind::BoundVar(_)
                | TyKind::InferenceVar(_, _)
                | TyKind::Dyn(_)
                | TyKind::Function(_)
                | TyKind::Alias(_)
                | TyKind::Foreign(_)
                | TyKind::Generator(..)
                | TyKind::GeneratorWitness(..) => false,
            }
        }
    }

    pub fn fields(&self, db: &dyn HirDatabase) -> Vec<(Field, Type)> {
        let (variant_id, substs) = match *self.ty.kind(&Interner) {
            TyKind::Adt(hir_ty::AdtId(AdtId::StructId(s)), ref substs) => (s.into(), substs),
            TyKind::Adt(hir_ty::AdtId(AdtId::UnionId(u)), ref substs) => (u.into(), substs),
            _ => return Vec::new(),
        };

        db.field_types(variant_id)
            .iter()
            .map(|(local_id, ty)| {
                let def = Field { parent: variant_id.into(), id: local_id };
                let ty = ty.clone().substitute(&Interner, substs);
                (def, self.derived(ty))
            })
            .collect()
    }

    pub fn tuple_fields(&self, _db: &dyn HirDatabase) -> Vec<Type> {
        if let TyKind::Tuple(_, substs) = &self.ty.kind(&Interner) {
            substs
                .iter(&Interner)
                .map(|ty| self.derived(ty.assert_ty_ref(&Interner).clone()))
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn autoderef<'a>(&'a self, db: &'a dyn HirDatabase) -> impl Iterator<Item = Type> + 'a {
        // There should be no inference vars in types passed here
        // FIXME check that?
        let canonical =
            Canonical { value: self.ty.clone(), binders: CanonicalVarKinds::empty(&Interner) };
        let environment = self.env.env.clone();
        let ty = InEnvironment { goal: canonical, environment };
        autoderef(db, Some(self.krate), ty)
            .map(|canonical| canonical.value)
            .map(move |ty| self.derived(ty))
    }

    // This would be nicer if it just returned an iterator, but that runs into
    // lifetime problems, because we need to borrow temp `CrateImplDefs`.
    pub fn iterate_assoc_items<T>(
        self,
        db: &dyn HirDatabase,
        krate: Crate,
        mut callback: impl FnMut(AssocItem) -> Option<T>,
    ) -> Option<T> {
        for krate in method_resolution::def_crates(db, &self.ty, krate.id)? {
            let impls = db.inherent_impls_in_crate(krate);

            for impl_def in impls.for_self_ty(&self.ty) {
                for &item in db.impl_data(*impl_def).items.iter() {
                    if let Some(result) = callback(item.into()) {
                        return Some(result);
                    }
                }
            }
        }
        None
    }

    pub fn type_arguments(&self) -> impl Iterator<Item = Type> + '_ {
        self.ty
            .strip_references()
            .as_adt()
            .into_iter()
            .flat_map(|(_, substs)| substs.iter(&Interner))
            .filter_map(|arg| arg.ty(&Interner).cloned())
            .map(move |ty| self.derived(ty))
    }

    pub fn iterate_method_candidates<T>(
        &self,
        db: &dyn HirDatabase,
        krate: Crate,
        traits_in_scope: &FxHashSet<TraitId>,
        name: Option<&Name>,
        mut callback: impl FnMut(&Ty, Function) -> Option<T>,
    ) -> Option<T> {
        let _p = profile::span("iterate_method_candidates");
        // There should be no inference vars in types passed here
        // FIXME check that?
        // FIXME replace Unknown by bound vars here
        let canonical =
            Canonical { value: self.ty.clone(), binders: CanonicalVarKinds::empty(&Interner) };

        let env = self.env.clone();
        let krate = krate.id;

        method_resolution::iterate_method_candidates(
            &canonical,
            db,
            env,
            krate,
            traits_in_scope,
            None,
            name,
            method_resolution::LookupMode::MethodCall,
            |ty, it| match it {
                AssocItemId::FunctionId(f) => callback(ty, f.into()),
                _ => None,
            },
        )
    }

    pub fn iterate_path_candidates<T>(
        &self,
        db: &dyn HirDatabase,
        krate: Crate,
        traits_in_scope: &FxHashSet<TraitId>,
        name: Option<&Name>,
        mut callback: impl FnMut(&Ty, AssocItem) -> Option<T>,
    ) -> Option<T> {
        let _p = profile::span("iterate_path_candidates");
        let canonical = hir_ty::replace_errors_with_variables(&self.ty);

        let env = self.env.clone();
        let krate = krate.id;

        method_resolution::iterate_method_candidates(
            &canonical,
            db,
            env,
            krate,
            traits_in_scope,
            None,
            name,
            method_resolution::LookupMode::Path,
            |ty, it| callback(ty, it.into()),
        )
    }

    pub fn as_adt(&self) -> Option<Adt> {
        let (adt, _subst) = self.ty.as_adt()?;
        Some(adt.into())
    }

    pub fn as_builtin(&self) -> Option<BuiltinType> {
        self.ty.as_builtin().map(|inner| BuiltinType { inner })
    }

    pub fn as_dyn_trait(&self) -> Option<Trait> {
        self.ty.dyn_trait().map(Into::into)
    }

    /// If a type can be represented as `dyn Trait`, returns all traits accessible via this type,
    /// or an empty iterator otherwise.
    pub fn applicable_inherent_traits<'a>(
        &'a self,
        db: &'a dyn HirDatabase,
    ) -> impl Iterator<Item = Trait> + 'a {
        let _p = profile::span("applicable_inherent_traits");
        self.autoderef(db)
            .filter_map(|derefed_type| derefed_type.ty.dyn_trait())
            .flat_map(move |dyn_trait_id| hir_ty::all_super_traits(db.upcast(), dyn_trait_id))
            .map(Trait::from)
    }

    pub fn as_impl_traits(&self, db: &dyn HirDatabase) -> Option<Vec<Trait>> {
        self.ty.impl_trait_bounds(db).map(|it| {
            it.into_iter()
                .filter_map(|pred| match pred.skip_binders() {
                    hir_ty::WhereClause::Implemented(trait_ref) => {
                        Some(Trait::from(trait_ref.hir_trait_id()))
                    }
                    _ => None,
                })
                .collect()
        })
    }

    pub fn as_associated_type_parent_trait(&self, db: &dyn HirDatabase) -> Option<Trait> {
        self.ty.associated_type_parent_trait(db).map(Into::into)
    }

    fn derived(&self, ty: Ty) -> Type {
        Type { krate: self.krate, env: self.env.clone(), ty }
    }

    pub fn walk(&self, db: &dyn HirDatabase, mut cb: impl FnMut(Type)) {
        // TypeWalk::walk for a Ty at first visits parameters and only after that the Ty itself.
        // We need a different order here.

        fn walk_substs(
            db: &dyn HirDatabase,
            type_: &Type,
            substs: &Substitution,
            cb: &mut impl FnMut(Type),
        ) {
            for ty in substs.iter(&Interner).filter_map(|a| a.ty(&Interner)) {
                walk_type(db, &type_.derived(ty.clone()), cb);
            }
        }

        fn walk_bounds(
            db: &dyn HirDatabase,
            type_: &Type,
            bounds: &[QuantifiedWhereClause],
            cb: &mut impl FnMut(Type),
        ) {
            for pred in bounds {
                if let WhereClause::Implemented(trait_ref) = pred.skip_binders() {
                    cb(type_.clone());
                    // skip the self type. it's likely the type we just got the bounds from
                    for ty in trait_ref
                        .substitution
                        .iter(&Interner)
                        .skip(1)
                        .filter_map(|a| a.ty(&Interner))
                    {
                        walk_type(db, &type_.derived(ty.clone()), cb);
                    }
                }
            }
        }

        fn walk_type(db: &dyn HirDatabase, type_: &Type, cb: &mut impl FnMut(Type)) {
            let ty = type_.ty.strip_references();
            match ty.kind(&Interner) {
                TyKind::Adt(_, substs) => {
                    cb(type_.derived(ty.clone()));
                    walk_substs(db, type_, substs, cb);
                }
                TyKind::AssociatedType(_, substs) => {
                    if ty.associated_type_parent_trait(db).is_some() {
                        cb(type_.derived(ty.clone()));
                    }
                    walk_substs(db, type_, substs, cb);
                }
                TyKind::OpaqueType(_, subst) => {
                    if let Some(bounds) = ty.impl_trait_bounds(db) {
                        walk_bounds(db, &type_.derived(ty.clone()), &bounds, cb);
                    }

                    walk_substs(db, type_, subst, cb);
                }
                TyKind::Alias(AliasTy::Opaque(opaque_ty)) => {
                    if let Some(bounds) = ty.impl_trait_bounds(db) {
                        walk_bounds(db, &type_.derived(ty.clone()), &bounds, cb);
                    }

                    walk_substs(db, type_, &opaque_ty.substitution, cb);
                }
                TyKind::Placeholder(_) => {
                    if let Some(bounds) = ty.impl_trait_bounds(db) {
                        walk_bounds(db, &type_.derived(ty.clone()), &bounds, cb);
                    }
                }
                TyKind::Dyn(bounds) => {
                    walk_bounds(
                        db,
                        &type_.derived(ty.clone()),
                        bounds.bounds.skip_binders().interned(),
                        cb,
                    );
                }

                TyKind::Ref(_, _, ty)
                | TyKind::Raw(_, ty)
                | TyKind::Array(ty, _)
                | TyKind::Slice(ty) => {
                    walk_type(db, &type_.derived(ty.clone()), cb);
                }

                TyKind::FnDef(_, substs)
                | TyKind::Tuple(_, substs)
                | TyKind::Closure(.., substs) => {
                    walk_substs(db, type_, substs, cb);
                }
                TyKind::Function(hir_ty::FnPointer { substitution, .. }) => {
                    walk_substs(db, type_, &substitution.0, cb);
                }

                _ => {}
            }
        }

        walk_type(db, self, &mut cb);
    }

    pub fn could_unify_with(&self, db: &dyn HirDatabase, other: &Type) -> bool {
        let tys = hir_ty::replace_errors_with_variables(&(self.ty.clone(), other.ty.clone()));
        could_unify(db, self.env.clone(), &tys)
    }
}

// FIXME: closures
#[derive(Debug)]
pub struct Callable {
    ty: Type,
    sig: CallableSig,
    def: Option<CallableDefId>,
    pub(crate) is_bound_method: bool,
}

pub enum CallableKind {
    Function(Function),
    TupleStruct(Struct),
    TupleEnumVariant(Variant),
    Closure,
}

impl Callable {
    pub fn kind(&self) -> CallableKind {
        match self.def {
            Some(CallableDefId::FunctionId(it)) => CallableKind::Function(it.into()),
            Some(CallableDefId::StructId(it)) => CallableKind::TupleStruct(it.into()),
            Some(CallableDefId::EnumVariantId(it)) => CallableKind::TupleEnumVariant(it.into()),
            None => CallableKind::Closure,
        }
    }
    pub fn receiver_param(&self, db: &dyn HirDatabase) -> Option<ast::SelfParam> {
        let func = match self.def {
            Some(CallableDefId::FunctionId(it)) if self.is_bound_method => it,
            _ => return None,
        };
        let src = func.lookup(db.upcast()).source(db.upcast());
        let param_list = src.value.param_list()?;
        param_list.self_param()
    }
    pub fn n_params(&self) -> usize {
        self.sig.params().len() - if self.is_bound_method { 1 } else { 0 }
    }
    pub fn params(
        &self,
        db: &dyn HirDatabase,
    ) -> Vec<(Option<Either<ast::SelfParam, ast::Pat>>, Type)> {
        let types = self
            .sig
            .params()
            .iter()
            .skip(if self.is_bound_method { 1 } else { 0 })
            .map(|ty| self.ty.derived(ty.clone()));
        let patterns = match self.def {
            Some(CallableDefId::FunctionId(func)) => {
                let src = func.lookup(db.upcast()).source(db.upcast());
                src.value.param_list().map(|param_list| {
                    param_list
                        .self_param()
                        .map(|it| Some(Either::Left(it)))
                        .filter(|_| !self.is_bound_method)
                        .into_iter()
                        .chain(param_list.params().map(|it| it.pat().map(Either::Right)))
                })
            }
            _ => None,
        };
        patterns.into_iter().flatten().chain(iter::repeat(None)).zip(types).collect()
    }
    pub fn return_type(&self) -> Type {
        self.ty.derived(self.sig.ret().clone())
    }
}

/// For IDE only
#[derive(Debug, PartialEq, Eq, Hash)]
pub enum ScopeDef {
    ModuleDef(ModuleDef),
    MacroDef(MacroDef),
    GenericParam(GenericParam),
    ImplSelfType(Impl),
    AdtSelfType(Adt),
    Local(Local),
    Label(Label),
    Unknown,
}

impl ScopeDef {
    pub fn all_items(def: PerNs) -> ArrayVec<Self, 3> {
        let mut items = ArrayVec::new();

        match (def.take_types(), def.take_values()) {
            (Some(m1), None) => items.push(ScopeDef::ModuleDef(m1.into())),
            (None, Some(m2)) => items.push(ScopeDef::ModuleDef(m2.into())),
            (Some(m1), Some(m2)) => {
                // Some items, like unit structs and enum variants, are
                // returned as both a type and a value. Here we want
                // to de-duplicate them.
                if m1 != m2 {
                    items.push(ScopeDef::ModuleDef(m1.into()));
                    items.push(ScopeDef::ModuleDef(m2.into()));
                } else {
                    items.push(ScopeDef::ModuleDef(m1.into()));
                }
            }
            (None, None) => {}
        };

        if let Some(macro_def_id) = def.take_macros() {
            items.push(ScopeDef::MacroDef(macro_def_id.into()));
        }

        if items.is_empty() {
            items.push(ScopeDef::Unknown);
        }

        items
    }
}

impl From<ItemInNs> for ScopeDef {
    fn from(item: ItemInNs) -> Self {
        match item {
            ItemInNs::Types(id) => ScopeDef::ModuleDef(id.into()),
            ItemInNs::Values(id) => ScopeDef::ModuleDef(id.into()),
            ItemInNs::Macros(id) => ScopeDef::MacroDef(id.into()),
        }
    }
}

pub trait HasVisibility {
    fn visibility(&self, db: &dyn HirDatabase) -> Visibility;
    fn is_visible_from(&self, db: &dyn HirDatabase, module: Module) -> bool {
        let vis = self.visibility(db);
        vis.is_visible_from(db.upcast(), module.id)
    }
}
