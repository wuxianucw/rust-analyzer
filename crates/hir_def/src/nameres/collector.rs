//! The core of the module-level name resolution algorithm.
//!
//! `DefCollector::collect` contains the fixed-point iteration loop which
//! resolves imports and expands macros.

use std::iter;

use base_db::{CrateId, Edition, FileId, ProcMacroId};
use cfg::{CfgExpr, CfgOptions};
use hir_expand::{
    ast_id_map::FileAstId,
    builtin_attr::find_builtin_attr,
    builtin_derive::find_builtin_derive,
    builtin_macro::find_builtin_macro,
    name::{name, AsName, Name},
    proc_macro::ProcMacroExpander,
    FragmentKind, HirFileId, MacroCallId, MacroCallKind, MacroDefId, MacroDefKind,
};
use hir_expand::{InFile, MacroCallLoc};
use itertools::Itertools;
use la_arena::Idx;
use rustc_hash::{FxHashMap, FxHashSet};
use syntax::ast;

use crate::{
    attr::{Attr, AttrId, AttrInput, Attrs},
    attr_macro_as_call_id, builtin_attr,
    db::DefDatabase,
    derive_macro_as_call_id,
    intern::Interned,
    item_scope::{ImportType, PerNsGlobImports},
    item_tree::{
        self, Fields, FileItemTreeId, ImportKind, ItemTree, ItemTreeId, MacroCall, MacroDef,
        MacroRules, Mod, ModItem, ModKind,
    },
    macro_call_as_call_id,
    nameres::{
        diagnostics::DefDiagnostic,
        mod_resolution::ModDir,
        path_resolution::ReachedFixedPoint,
        proc_macro::{ProcMacroDef, ProcMacroKind},
        BuiltinShadowMode, DefMap, ModuleData, ModuleOrigin, ResolveMode,
    },
    path::{ImportAlias, ModPath, PathKind},
    per_ns::PerNs,
    visibility::{RawVisibility, Visibility},
    AdtId, AstId, AstIdWithPath, ConstLoc, EnumLoc, EnumVariantId, FunctionLoc, ImplLoc, Intern,
    LocalModuleId, ModuleDefId, StaticLoc, StructLoc, TraitLoc, TypeAliasLoc, UnionLoc,
    UnresolvedMacro,
};

const GLOB_RECURSION_LIMIT: usize = 100;
const EXPANSION_DEPTH_LIMIT: usize = 128;
const FIXED_POINT_LIMIT: usize = 8192;

pub(super) fn collect_defs(
    db: &dyn DefDatabase,
    mut def_map: DefMap,
    block: Option<AstId<ast::BlockExpr>>,
) -> DefMap {
    let crate_graph = db.crate_graph();

    if block.is_none() {
        // populate external prelude
        for dep in &crate_graph[def_map.krate].dependencies {
            log::debug!("crate dep {:?} -> {:?}", dep.name, dep.crate_id);
            let dep_def_map = db.crate_def_map(dep.crate_id);
            def_map
                .extern_prelude
                .insert(dep.as_name(), dep_def_map.module_id(dep_def_map.root).into());
        }
    }

    let cfg_options = &crate_graph[def_map.krate].cfg_options;
    let proc_macros = &crate_graph[def_map.krate].proc_macro;
    let proc_macros = proc_macros
        .iter()
        .enumerate()
        .map(|(idx, it)| {
            // FIXME: a hacky way to create a Name from string.
            let name = tt::Ident { text: it.name.clone(), id: tt::TokenId::unspecified() };
            (name.as_name(), ProcMacroExpander::new(def_map.krate, ProcMacroId(idx as u32)))
        })
        .collect();

    let mut collector = DefCollector {
        db,
        def_map,
        glob_imports: FxHashMap::default(),
        unresolved_imports: Vec::new(),
        resolved_imports: Vec::new(),

        unresolved_macros: Vec::new(),
        mod_dirs: FxHashMap::default(),
        cfg_options,
        proc_macros,
        exports_proc_macros: false,
        from_glob_import: Default::default(),
        skip_attrs: Default::default(),
        derive_helpers_in_scope: Default::default(),
        registered_attrs: Default::default(),
        registered_tools: Default::default(),
    };
    match block {
        Some(block) => {
            collector.seed_with_inner(block);
        }
        None => {
            collector.seed_with_top_level();
        }
    }
    collector.collect();
    let mut def_map = collector.finish();
    def_map.shrink_to_fit();
    def_map
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum PartialResolvedImport {
    /// None of any namespaces is resolved
    Unresolved,
    /// One of namespaces is resolved
    Indeterminate(PerNs),
    /// All namespaces are resolved, OR it is came from other crate
    Resolved(PerNs),
}

impl PartialResolvedImport {
    fn namespaces(&self) -> PerNs {
        match self {
            PartialResolvedImport::Unresolved => PerNs::none(),
            PartialResolvedImport::Indeterminate(ns) => *ns,
            PartialResolvedImport::Resolved(ns) => *ns,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ImportSource {
    Import { id: ItemTreeId<item_tree::Import>, use_tree: Idx<ast::UseTree> },
    ExternCrate(ItemTreeId<item_tree::ExternCrate>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Import {
    path: Interned<ModPath>,
    alias: Option<ImportAlias>,
    visibility: RawVisibility,
    kind: ImportKind,
    is_prelude: bool,
    is_extern_crate: bool,
    is_macro_use: bool,
    source: ImportSource,
}

impl Import {
    fn from_use(
        db: &dyn DefDatabase,
        krate: CrateId,
        tree: &ItemTree,
        id: ItemTreeId<item_tree::Import>,
    ) -> Vec<Self> {
        let it = &tree[id.value];
        let attrs = &tree.attrs(db, krate, ModItem::from(id.value).into());
        let visibility = &tree[it.visibility];
        let is_prelude = attrs.by_key("prelude_import").exists();

        let mut res = Vec::new();
        it.use_tree.expand(|idx, path, kind, alias| {
            res.push(Self {
                path: Interned::new(path), // FIXME this makes little sense
                alias,
                visibility: visibility.clone(),
                kind,
                is_prelude,
                is_extern_crate: false,
                is_macro_use: false,
                source: ImportSource::Import { id, use_tree: idx },
            });
        });
        res
    }

    fn from_extern_crate(
        db: &dyn DefDatabase,
        krate: CrateId,
        tree: &ItemTree,
        id: ItemTreeId<item_tree::ExternCrate>,
    ) -> Self {
        let it = &tree[id.value];
        let attrs = &tree.attrs(db, krate, ModItem::from(id.value).into());
        let visibility = &tree[it.visibility];
        Self {
            path: Interned::new(ModPath::from_segments(
                PathKind::Plain,
                iter::once(it.name.clone()),
            )),
            alias: it.alias.clone(),
            visibility: visibility.clone(),
            kind: ImportKind::Plain,
            is_prelude: false,
            is_extern_crate: true,
            is_macro_use: attrs.by_key("macro_use").exists(),
            source: ImportSource::ExternCrate(id),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ImportDirective {
    module_id: LocalModuleId,
    import: Import,
    status: PartialResolvedImport,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MacroDirective {
    module_id: LocalModuleId,
    depth: usize,
    kind: MacroDirectiveKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MacroDirectiveKind {
    FnLike { ast_id: AstIdWithPath<ast::MacroCall>, fragment: FragmentKind },
    Derive { ast_id: AstIdWithPath<ast::Item>, derive_attr: AttrId },
    Attr { ast_id: AstIdWithPath<ast::Item>, attr: Attr, mod_item: ModItem },
}

struct DefData<'a> {
    id: ModuleDefId,
    name: &'a Name,
    visibility: &'a RawVisibility,
    has_constructor: bool,
}

/// Walks the tree of module recursively
struct DefCollector<'a> {
    db: &'a dyn DefDatabase,
    def_map: DefMap,
    glob_imports: FxHashMap<LocalModuleId, Vec<(LocalModuleId, Visibility)>>,
    unresolved_imports: Vec<ImportDirective>,
    resolved_imports: Vec<ImportDirective>,
    unresolved_macros: Vec<MacroDirective>,
    mod_dirs: FxHashMap<LocalModuleId, ModDir>,
    cfg_options: &'a CfgOptions,
    /// List of procedural macros defined by this crate. This is read from the dynamic library
    /// built by the build system, and is the list of proc. macros we can actually expand. It is
    /// empty when proc. macro support is disabled (in which case we still do name resolution for
    /// them).
    proc_macros: Vec<(Name, ProcMacroExpander)>,
    exports_proc_macros: bool,
    from_glob_import: PerNsGlobImports,
    /// If we fail to resolve an attribute on a `ModItem`, we fall back to ignoring the attribute.
    /// This map is used to skip all attributes up to and including the one that failed to resolve,
    /// in order to not expand them twice.
    ///
    /// This also stores the attributes to skip when we resolve derive helpers and non-macro
    /// non-builtin attributes in general.
    skip_attrs: FxHashMap<InFile<ModItem>, AttrId>,
    /// Tracks which custom derives are in scope for an item, to allow resolution of derive helper
    /// attributes.
    derive_helpers_in_scope: FxHashMap<AstId<ast::Item>, Vec<Name>>,
    /// Custom attributes registered with `#![register_attr]`.
    registered_attrs: Vec<String>,
    /// Custom tool modules registered with `#![register_tool]`.
    registered_tools: Vec<String>,
}

impl DefCollector<'_> {
    fn seed_with_top_level(&mut self) {
        let file_id = self.db.crate_graph()[self.def_map.krate].root_file_id;
        let item_tree = self.db.file_item_tree(file_id.into());
        let module_id = self.def_map.root;

        let attrs = item_tree.top_level_attrs(self.db, self.def_map.krate);
        if attrs.cfg().map_or(true, |cfg| self.cfg_options.check(&cfg) != Some(false)) {
            self.inject_prelude(&attrs);

            // Process other crate-level attributes.
            for attr in &*attrs {
                let attr_name = match attr.path.as_ident() {
                    Some(name) => name,
                    None => continue,
                };

                let registered_name = if *attr_name == hir_expand::name![register_attr]
                    || *attr_name == hir_expand::name![register_tool]
                {
                    match attr.input.as_deref() {
                        Some(AttrInput::TokenTree(subtree)) => match &*subtree.token_trees {
                            [tt::TokenTree::Leaf(tt::Leaf::Ident(name))] => name.as_name(),
                            _ => continue,
                        },
                        _ => continue,
                    }
                } else {
                    continue;
                };

                if *attr_name == hir_expand::name![register_attr] {
                    self.registered_attrs.push(registered_name.to_string());
                    cov_mark::hit!(register_attr);
                } else {
                    self.registered_tools.push(registered_name.to_string());
                    cov_mark::hit!(register_tool);
                }
            }

            ModCollector {
                def_collector: &mut *self,
                macro_depth: 0,
                module_id,
                file_id: file_id.into(),
                item_tree: &item_tree,
                mod_dir: ModDir::root(),
            }
            .collect(item_tree.top_level_items());
        }
    }

    fn seed_with_inner(&mut self, block: AstId<ast::BlockExpr>) {
        let item_tree = self.db.file_item_tree(block.file_id);
        let module_id = self.def_map.root;
        if item_tree
            .top_level_attrs(self.db, self.def_map.krate)
            .cfg()
            .map_or(true, |cfg| self.cfg_options.check(&cfg) != Some(false))
        {
            ModCollector {
                def_collector: &mut *self,
                macro_depth: 0,
                module_id,
                file_id: block.file_id,
                item_tree: &item_tree,
                mod_dir: ModDir::root(),
            }
            .collect(item_tree.inner_items_of_block(block.value));
        }
    }

    fn collect(&mut self) {
        // main name resolution fixed-point loop.
        let mut i = 0;
        'outer: loop {
            loop {
                self.db.unwind_if_cancelled();
                loop {
                    if self.resolve_imports() == ReachedFixedPoint::Yes {
                        break;
                    }
                }
                if self.resolve_macros() == ReachedFixedPoint::Yes {
                    break;
                }

                i += 1;
                if i == FIXED_POINT_LIMIT {
                    log::error!("name resolution is stuck");
                    break 'outer;
                }
            }

            if self.reseed_with_unresolved_attribute() == ReachedFixedPoint::Yes {
                break;
            }
        }

        // Resolve all indeterminate resolved imports again
        // As some of the macros will expand newly import shadowing partial resolved imports
        // FIXME: We maybe could skip this, if we handle the indeterminate imports in `resolve_imports`
        // correctly
        let partial_resolved = self.resolved_imports.iter().filter_map(|directive| {
            if let PartialResolvedImport::Indeterminate(_) = directive.status {
                let mut directive = directive.clone();
                directive.status = PartialResolvedImport::Unresolved;
                Some(directive)
            } else {
                None
            }
        });
        self.unresolved_imports.extend(partial_resolved);
        self.resolve_imports();

        let unresolved_imports = std::mem::replace(&mut self.unresolved_imports, Vec::new());
        // show unresolved imports in completion, etc
        for directive in &unresolved_imports {
            self.record_resolved_import(directive)
        }
        self.unresolved_imports = unresolved_imports;

        // FIXME: This condition should instead check if this is a `proc-macro` type crate.
        if self.exports_proc_macros {
            // A crate exporting procedural macros is not allowed to export anything else.
            //
            // Additionally, while the proc macro entry points must be `pub`, they are not publicly
            // exported in type/value namespace. This function reduces the visibility of all items
            // in the crate root that aren't proc macros.
            let root = self.def_map.root;
            let module_id = self.def_map.module_id(root);
            let root = &mut self.def_map.modules[root];
            root.scope.censor_non_proc_macros(module_id);
        }
    }

    /// When the fixed-point loop reaches a stable state, we might still have some unresolved
    /// attributes (or unexpanded attribute proc macros) left over. This takes one of them, and
    /// feeds the item it's applied to back into name resolution.
    ///
    /// This effectively ignores the fact that the macro is there and just treats the items as
    /// normal code.
    ///
    /// This improves UX when proc macros are turned off or don't work, and replicates the behavior
    /// before we supported proc. attribute macros.
    fn reseed_with_unresolved_attribute(&mut self) -> ReachedFixedPoint {
        cov_mark::hit!(unresolved_attribute_fallback);

        let mut unresolved_macros = std::mem::replace(&mut self.unresolved_macros, Vec::new());
        let pos = unresolved_macros.iter().position(|directive| {
            if let MacroDirectiveKind::Attr { ast_id, mod_item, attr } = &directive.kind {
                self.skip_attrs.insert(ast_id.ast_id.with_value(*mod_item), attr.id);

                let file_id = ast_id.ast_id.file_id;
                let item_tree = self.db.file_item_tree(file_id);
                let mod_dir = self.mod_dirs[&directive.module_id].clone();
                ModCollector {
                    def_collector: &mut *self,
                    macro_depth: directive.depth,
                    module_id: directive.module_id,
                    file_id,
                    item_tree: &item_tree,
                    mod_dir,
                }
                .collect(&[*mod_item]);
                true
            } else {
                false
            }
        });

        if let Some(pos) = pos {
            unresolved_macros.remove(pos);
        }

        // The collection above might add new unresolved macros (eg. derives), so merge the lists.
        self.unresolved_macros.extend(unresolved_macros);

        if pos.is_some() {
            // Continue name resolution with the new data.
            ReachedFixedPoint::No
        } else {
            ReachedFixedPoint::Yes
        }
    }

    fn inject_prelude(&mut self, crate_attrs: &Attrs) {
        // See compiler/rustc_builtin_macros/src/standard_library_imports.rs

        if crate_attrs.by_key("no_core").exists() {
            // libcore does not get a prelude.
            return;
        }

        let krate = if crate_attrs.by_key("no_std").exists() {
            name![core]
        } else {
            let std = name![std];
            if self.def_map.extern_prelude().any(|(name, _)| *name == std) {
                std
            } else {
                // If `std` does not exist for some reason, fall back to core. This mostly helps
                // keep r-a's own tests minimal.
                name![core]
            }
        };

        let edition = match self.def_map.edition {
            Edition::Edition2015 => name![rust_2015],
            Edition::Edition2018 => name![rust_2018],
            Edition::Edition2021 => name![rust_2021],
        };

        let path_kind = if self.def_map.edition == Edition::Edition2015 {
            PathKind::Plain
        } else {
            PathKind::Abs
        };
        let path = ModPath::from_segments(
            path_kind.clone(),
            [krate.clone(), name![prelude], edition].iter().cloned(),
        );
        // Fall back to the older `std::prelude::v1` for compatibility with Rust <1.52.0
        // FIXME remove this fallback
        let fallback_path =
            ModPath::from_segments(path_kind, [krate, name![prelude], name![v1]].iter().cloned());

        for path in &[path, fallback_path] {
            let (per_ns, _) = self.def_map.resolve_path(
                self.db,
                self.def_map.root,
                path,
                BuiltinShadowMode::Other,
            );

            match &per_ns.types {
                Some((ModuleDefId::ModuleId(m), _)) => {
                    self.def_map.prelude = Some(*m);
                    return;
                }
                _ => {
                    log::debug!(
                        "could not resolve prelude path `{}` to module (resolved to {:?})",
                        path,
                        per_ns.types
                    );
                }
            }
        }
    }

    /// Adds a definition of procedural macro `name` to the root module.
    ///
    /// # Notes on procedural macro resolution
    ///
    /// Procedural macro functionality is provided by the build system: It has to build the proc
    /// macro and pass the resulting dynamic library to rust-analyzer.
    ///
    /// When procedural macro support is enabled, the list of proc macros exported by a crate is
    /// known before we resolve names in the crate. This list is stored in `self.proc_macros` and is
    /// derived from the dynamic library.
    ///
    /// However, we *also* would like to be able to at least *resolve* macros on our own, without
    /// help by the build system. So, when the macro isn't found in `self.proc_macros`, we instead
    /// use a dummy expander that always errors. This comes with the drawback of macros potentially
    /// going out of sync with what the build system sees (since we resolve using VFS state, but
    /// Cargo builds only on-disk files). We could and probably should add diagnostics for that.
    fn export_proc_macro(&mut self, def: ProcMacroDef, ast_id: AstId<ast::Fn>) {
        let kind = def.kind.to_basedb_kind();
        self.exports_proc_macros = true;
        let macro_def = match self.proc_macros.iter().find(|(n, _)| n == &def.name) {
            Some((_, expander)) => MacroDefId {
                krate: self.def_map.krate,
                kind: MacroDefKind::ProcMacro(*expander, kind, ast_id),
                local_inner: false,
            },
            None => MacroDefId {
                krate: self.def_map.krate,
                kind: MacroDefKind::ProcMacro(
                    ProcMacroExpander::dummy(self.def_map.krate),
                    kind,
                    ast_id,
                ),
                local_inner: false,
            },
        };

        self.define_proc_macro(def.name.clone(), macro_def);
        self.def_map.exported_proc_macros.insert(macro_def, def);
    }

    /// Define a macro with `macro_rules`.
    ///
    /// It will define the macro in legacy textual scope, and if it has `#[macro_export]`,
    /// then it is also defined in the root module scope.
    /// You can `use` or invoke it by `crate::macro_name` anywhere, before or after the definition.
    ///
    /// It is surprising that the macro will never be in the current module scope.
    /// These code fails with "unresolved import/macro",
    /// ```rust,compile_fail
    /// mod m { macro_rules! foo { () => {} } }
    /// use m::foo as bar;
    /// ```
    ///
    /// ```rust,compile_fail
    /// macro_rules! foo { () => {} }
    /// self::foo!();
    /// crate::foo!();
    /// ```
    ///
    /// Well, this code compiles, because the plain path `foo` in `use` is searched
    /// in the legacy textual scope only.
    /// ```rust
    /// macro_rules! foo { () => {} }
    /// use foo as bar;
    /// ```
    fn define_macro_rules(
        &mut self,
        module_id: LocalModuleId,
        name: Name,
        macro_: MacroDefId,
        export: bool,
    ) {
        // Textual scoping
        self.define_legacy_macro(module_id, name.clone(), macro_);

        // Module scoping
        // In Rust, `#[macro_export]` macros are unconditionally visible at the
        // crate root, even if the parent modules is **not** visible.
        if export {
            self.update(
                self.def_map.root,
                &[(Some(name), PerNs::macros(macro_, Visibility::Public))],
                Visibility::Public,
                ImportType::Named,
            );
        }
    }

    /// Define a legacy textual scoped macro in module
    ///
    /// We use a map `legacy_macros` to store all legacy textual scoped macros visible per module.
    /// It will clone all macros from parent legacy scope, whose definition is prior to
    /// the definition of current module.
    /// And also, `macro_use` on a module will import all legacy macros visible inside to
    /// current legacy scope, with possible shadowing.
    fn define_legacy_macro(&mut self, module_id: LocalModuleId, name: Name, mac: MacroDefId) {
        // Always shadowing
        self.def_map.modules[module_id].scope.define_legacy_macro(name, mac);
    }

    /// Define a macro 2.0 macro
    ///
    /// The scoped of macro 2.0 macro is equal to normal function
    fn define_macro_def(
        &mut self,
        module_id: LocalModuleId,
        name: Name,
        macro_: MacroDefId,
        vis: &RawVisibility,
    ) {
        let vis =
            self.def_map.resolve_visibility(self.db, module_id, vis).unwrap_or(Visibility::Public);
        self.update(module_id, &[(Some(name), PerNs::macros(macro_, vis))], vis, ImportType::Named);
    }

    /// Define a proc macro
    ///
    /// A proc macro is similar to normal macro scope, but it would not visible in legacy textual scoped.
    /// And unconditionally exported.
    fn define_proc_macro(&mut self, name: Name, macro_: MacroDefId) {
        self.update(
            self.def_map.root,
            &[(Some(name), PerNs::macros(macro_, Visibility::Public))],
            Visibility::Public,
            ImportType::Named,
        );
    }

    /// Import macros from `#[macro_use] extern crate`.
    fn import_macros_from_extern_crate(
        &mut self,
        current_module_id: LocalModuleId,
        extern_crate: &item_tree::ExternCrate,
    ) {
        log::debug!(
            "importing macros from extern crate: {:?} ({:?})",
            extern_crate,
            self.def_map.edition,
        );

        let res = self.def_map.resolve_name_in_extern_prelude(self.db, &extern_crate.name);

        if let Some(ModuleDefId::ModuleId(m)) = res.take_types() {
            if m == self.def_map.module_id(current_module_id) {
                cov_mark::hit!(ignore_macro_use_extern_crate_self);
                return;
            }

            cov_mark::hit!(macro_rules_from_other_crates_are_visible_with_macro_use);
            self.import_all_macros_exported(current_module_id, m.krate);
        }
    }

    /// Import all exported macros from another crate
    ///
    /// Exported macros are just all macros in the root module scope.
    /// Note that it contains not only all `#[macro_export]` macros, but also all aliases
    /// created by `use` in the root module, ignoring the visibility of `use`.
    fn import_all_macros_exported(&mut self, current_module_id: LocalModuleId, krate: CrateId) {
        let def_map = self.db.crate_def_map(krate);
        for (name, def) in def_map[def_map.root].scope.macros() {
            // `macro_use` only bring things into legacy scope.
            self.define_legacy_macro(current_module_id, name.clone(), def);
        }
    }

    /// Tries to resolve every currently unresolved import.
    fn resolve_imports(&mut self) -> ReachedFixedPoint {
        let mut res = ReachedFixedPoint::Yes;
        let imports = std::mem::replace(&mut self.unresolved_imports, Vec::new());
        let imports = imports
            .into_iter()
            .filter_map(|mut directive| {
                directive.status = self.resolve_import(directive.module_id, &directive.import);
                match directive.status {
                    PartialResolvedImport::Indeterminate(_) => {
                        self.record_resolved_import(&directive);
                        // FIXME: For avoid performance regression,
                        // we consider an imported resolved if it is indeterminate (i.e not all namespace resolved)
                        self.resolved_imports.push(directive);
                        res = ReachedFixedPoint::No;
                        None
                    }
                    PartialResolvedImport::Resolved(_) => {
                        self.record_resolved_import(&directive);
                        self.resolved_imports.push(directive);
                        res = ReachedFixedPoint::No;
                        None
                    }
                    PartialResolvedImport::Unresolved => Some(directive),
                }
            })
            .collect();
        self.unresolved_imports = imports;
        res
    }

    fn resolve_import(&self, module_id: LocalModuleId, import: &Import) -> PartialResolvedImport {
        log::debug!("resolving import: {:?} ({:?})", import, self.def_map.edition);
        if import.is_extern_crate {
            let res = self.def_map.resolve_name_in_extern_prelude(
                self.db,
                import
                    .path
                    .as_ident()
                    .expect("extern crate should have been desugared to one-element path"),
            );
            if res.is_none() {
                PartialResolvedImport::Unresolved
            } else {
                PartialResolvedImport::Resolved(res)
            }
        } else {
            let res = self.def_map.resolve_path_fp_with_macro(
                self.db,
                ResolveMode::Import,
                module_id,
                &import.path,
                BuiltinShadowMode::Module,
            );

            let def = res.resolved_def;
            if res.reached_fixedpoint == ReachedFixedPoint::No || def.is_none() {
                return PartialResolvedImport::Unresolved;
            }

            if let Some(krate) = res.krate {
                if krate != self.def_map.krate {
                    return PartialResolvedImport::Resolved(def);
                }
            }

            // Check whether all namespace is resolved
            if def.take_types().is_some()
                && def.take_values().is_some()
                && def.take_macros().is_some()
            {
                PartialResolvedImport::Resolved(def)
            } else {
                PartialResolvedImport::Indeterminate(def)
            }
        }
    }

    fn record_resolved_import(&mut self, directive: &ImportDirective) {
        let module_id = directive.module_id;
        let import = &directive.import;
        let mut def = directive.status.namespaces();
        let vis = self
            .def_map
            .resolve_visibility(self.db, module_id, &directive.import.visibility)
            .unwrap_or(Visibility::Public);

        match import.kind {
            ImportKind::Plain | ImportKind::TypeOnly => {
                let name = match &import.alias {
                    Some(ImportAlias::Alias(name)) => Some(name.clone()),
                    Some(ImportAlias::Underscore) => None,
                    None => match import.path.segments().last() {
                        Some(last_segment) => Some(last_segment.clone()),
                        None => {
                            cov_mark::hit!(bogus_paths);
                            return;
                        }
                    },
                };

                if import.kind == ImportKind::TypeOnly {
                    def.values = None;
                    def.macros = None;
                }

                log::debug!("resolved import {:?} ({:?}) to {:?}", name, import, def);

                // extern crates in the crate root are special-cased to insert entries into the extern prelude: rust-lang/rust#54658
                if import.is_extern_crate && module_id == self.def_map.root {
                    if let (Some(def), Some(name)) = (def.take_types(), name.as_ref()) {
                        self.def_map.extern_prelude.insert(name.clone(), def);
                    }
                }

                self.update(module_id, &[(name, def)], vis, ImportType::Named);
            }
            ImportKind::Glob => {
                log::debug!("glob import: {:?}", import);
                match def.take_types() {
                    Some(ModuleDefId::ModuleId(m)) => {
                        if import.is_prelude {
                            // Note: This dodgily overrides the injected prelude. The rustc
                            // implementation seems to work the same though.
                            cov_mark::hit!(std_prelude);
                            self.def_map.prelude = Some(m);
                        } else if m.krate != self.def_map.krate {
                            cov_mark::hit!(glob_across_crates);
                            // glob import from other crate => we can just import everything once
                            let item_map = m.def_map(self.db);
                            let scope = &item_map[m.local_id].scope;

                            // Module scoped macros is included
                            let items = scope
                                .resolutions()
                                // only keep visible names...
                                .map(|(n, res)| {
                                    (n, res.filter_visibility(|v| v.is_visible_from_other_crate()))
                                })
                                .filter(|(_, res)| !res.is_none())
                                .collect::<Vec<_>>();

                            self.update(module_id, &items, vis, ImportType::Glob);
                        } else {
                            // glob import from same crate => we do an initial
                            // import, and then need to propagate any further
                            // additions
                            let def_map;
                            let scope = if m.block == self.def_map.block_id() {
                                &self.def_map[m.local_id].scope
                            } else {
                                def_map = m.def_map(self.db);
                                &def_map[m.local_id].scope
                            };

                            // Module scoped macros is included
                            let items = scope
                                .resolutions()
                                // only keep visible names...
                                .map(|(n, res)| {
                                    (
                                        n,
                                        res.filter_visibility(|v| {
                                            v.is_visible_from_def_map(
                                                self.db,
                                                &self.def_map,
                                                module_id,
                                            )
                                        }),
                                    )
                                })
                                .filter(|(_, res)| !res.is_none())
                                .collect::<Vec<_>>();

                            self.update(module_id, &items, vis, ImportType::Glob);
                            // record the glob import in case we add further items
                            let glob = self.glob_imports.entry(m.local_id).or_default();
                            if !glob.iter().any(|(mid, _)| *mid == module_id) {
                                glob.push((module_id, vis));
                            }
                        }
                    }
                    Some(ModuleDefId::AdtId(AdtId::EnumId(e))) => {
                        cov_mark::hit!(glob_enum);
                        // glob import from enum => just import all the variants

                        // XXX: urgh, so this works by accident! Here, we look at
                        // the enum data, and, in theory, this might require us to
                        // look back at the crate_def_map, creating a cycle. For
                        // example, `enum E { crate::some_macro!(); }`. Luckily, the
                        // only kind of macro that is allowed inside enum is a
                        // `cfg_macro`, and we don't need to run name resolution for
                        // it, but this is sheer luck!
                        let enum_data = self.db.enum_data(e);
                        let resolutions = enum_data
                            .variants
                            .iter()
                            .map(|(local_id, variant_data)| {
                                let name = variant_data.name.clone();
                                let variant = EnumVariantId { parent: e, local_id };
                                let res = PerNs::both(variant.into(), variant.into(), vis);
                                (Some(name), res)
                            })
                            .collect::<Vec<_>>();
                        self.update(module_id, &resolutions, vis, ImportType::Glob);
                    }
                    Some(d) => {
                        log::debug!("glob import {:?} from non-module/enum {:?}", import, d);
                    }
                    None => {
                        log::debug!("glob import {:?} didn't resolve as type", import);
                    }
                }
            }
        }
    }

    fn update(
        &mut self,
        module_id: LocalModuleId,
        resolutions: &[(Option<Name>, PerNs)],
        vis: Visibility,
        import_type: ImportType,
    ) {
        self.db.unwind_if_cancelled();
        self.update_recursive(module_id, resolutions, vis, import_type, 0)
    }

    fn update_recursive(
        &mut self,
        module_id: LocalModuleId,
        resolutions: &[(Option<Name>, PerNs)],
        // All resolutions are imported with this visibility; the visibilities in
        // the `PerNs` values are ignored and overwritten
        vis: Visibility,
        import_type: ImportType,
        depth: usize,
    ) {
        if depth > GLOB_RECURSION_LIMIT {
            // prevent stack overflows (but this shouldn't be possible)
            panic!("infinite recursion in glob imports!");
        }
        let mut changed = false;

        for (name, res) in resolutions {
            match name {
                Some(name) => {
                    let scope = &mut self.def_map.modules[module_id].scope;
                    changed |= scope.push_res_with_import(
                        &mut self.from_glob_import,
                        (module_id, name.clone()),
                        res.with_visibility(vis),
                        import_type,
                    );
                }
                None => {
                    let tr = match res.take_types() {
                        Some(ModuleDefId::TraitId(tr)) => tr,
                        Some(other) => {
                            log::debug!("non-trait `_` import of {:?}", other);
                            continue;
                        }
                        None => continue,
                    };
                    let old_vis = self.def_map.modules[module_id].scope.unnamed_trait_vis(tr);
                    let should_update = match old_vis {
                        None => true,
                        Some(old_vis) => {
                            let max_vis = old_vis.max(vis, &self.def_map).unwrap_or_else(|| {
                                panic!("`Tr as _` imports with unrelated visibilities {:?} and {:?} (trait {:?})", old_vis, vis, tr);
                            });

                            if max_vis == old_vis {
                                false
                            } else {
                                cov_mark::hit!(upgrade_underscore_visibility);
                                true
                            }
                        }
                    };

                    if should_update {
                        changed = true;
                        self.def_map.modules[module_id].scope.push_unnamed_trait(tr, vis);
                    }
                }
            }
        }

        if !changed {
            return;
        }
        let glob_imports = self
            .glob_imports
            .get(&module_id)
            .into_iter()
            .flat_map(|v| v.iter())
            .filter(|(glob_importing_module, _)| {
                // we know all resolutions have the same visibility (`vis`), so we
                // just need to check that once
                vis.is_visible_from_def_map(self.db, &self.def_map, *glob_importing_module)
            })
            .cloned()
            .collect::<Vec<_>>();

        for (glob_importing_module, glob_import_vis) in glob_imports {
            self.update_recursive(
                glob_importing_module,
                resolutions,
                glob_import_vis,
                ImportType::Glob,
                depth + 1,
            );
        }
    }

    fn resolve_macros(&mut self) -> ReachedFixedPoint {
        let mut macros = std::mem::replace(&mut self.unresolved_macros, Vec::new());
        let mut resolved = Vec::new();
        let mut res = ReachedFixedPoint::Yes;
        macros.retain(|directive| {
            let resolver = |path| {
                let resolved_res = self.def_map.resolve_path_fp_with_macro(
                    self.db,
                    ResolveMode::Other,
                    directive.module_id,
                    &path,
                    BuiltinShadowMode::Module,
                );
                resolved_res.resolved_def.take_macros()
            };

            match &directive.kind {
                MacroDirectiveKind::FnLike { ast_id, fragment } => {
                    match macro_call_as_call_id(
                        ast_id,
                        *fragment,
                        self.db,
                        self.def_map.krate,
                        &resolver,
                        &mut |_err| (),
                    ) {
                        Ok(Ok(call_id)) => {
                            resolved.push((directive.module_id, call_id, directive.depth));
                            res = ReachedFixedPoint::No;
                            return false;
                        }
                        Err(UnresolvedMacro { .. }) | Ok(Err(_)) => {}
                    }
                }
                MacroDirectiveKind::Derive { ast_id, derive_attr } => {
                    match derive_macro_as_call_id(
                        ast_id,
                        *derive_attr,
                        self.db,
                        self.def_map.krate,
                        &resolver,
                    ) {
                        Ok(call_id) => {
                            resolved.push((directive.module_id, call_id, directive.depth));
                            res = ReachedFixedPoint::No;
                            return false;
                        }
                        Err(UnresolvedMacro { .. }) => (),
                    }
                }
                MacroDirectiveKind::Attr { ast_id, mod_item, attr } => {
                    if let Some(ident) = ast_id.path.as_ident() {
                        if let Some(helpers) = self.derive_helpers_in_scope.get(&ast_id.ast_id) {
                            if helpers.contains(ident) {
                                cov_mark::hit!(resolved_derive_helper);

                                // Resolved to derive helper. Collect the item's attributes again,
                                // starting after the derive helper.
                                let file_id = ast_id.ast_id.file_id;
                                let item_tree = self.db.file_item_tree(file_id);
                                let mod_dir = self.mod_dirs[&directive.module_id].clone();
                                self.skip_attrs.insert(InFile::new(file_id, *mod_item), attr.id);
                                ModCollector {
                                    def_collector: &mut *self,
                                    macro_depth: directive.depth,
                                    module_id: directive.module_id,
                                    file_id,
                                    item_tree: &item_tree,
                                    mod_dir,
                                }
                                .collect(&[*mod_item]);

                                // Remove the original directive since we resolved it.
                                res = ReachedFixedPoint::No;
                                return false;
                            }
                        }
                    }

                    if !self.db.enable_proc_attr_macros() {
                        return true;
                    }

                    // Not resolved to a derive helper, so try to resolve as a macro.
                    match attr_macro_as_call_id(
                        ast_id,
                        attr,
                        self.db,
                        self.def_map.krate,
                        &resolver,
                    ) {
                        Ok(call_id) => {
                            let loc: MacroCallLoc = self.db.lookup_intern_macro(call_id);
                            if let MacroDefKind::ProcMacro(exp, ..) = &loc.def.kind {
                                if exp.is_dummy() {
                                    // Proc macros that cannot be expanded are treated as not
                                    // resolved, in order to fall back later.
                                    self.def_map.diagnostics.push(
                                        DefDiagnostic::unresolved_proc_macro(
                                            directive.module_id,
                                            loc.kind,
                                        ),
                                    );

                                    let file_id = ast_id.ast_id.file_id;
                                    let item_tree = self.db.file_item_tree(file_id);
                                    let mod_dir = self.mod_dirs[&directive.module_id].clone();
                                    self.skip_attrs
                                        .insert(InFile::new(file_id, *mod_item), attr.id);
                                    ModCollector {
                                        def_collector: &mut *self,
                                        macro_depth: directive.depth,
                                        module_id: directive.module_id,
                                        file_id,
                                        item_tree: &item_tree,
                                        mod_dir,
                                    }
                                    .collect(&[*mod_item]);

                                    // Remove the macro directive.
                                    return false;
                                }
                            }

                            self.def_map.modules[directive.module_id]
                                .scope
                                .add_attr_macro_invoc(ast_id.ast_id, call_id);

                            resolved.push((directive.module_id, call_id, directive.depth));
                            res = ReachedFixedPoint::No;
                            return false;
                        }
                        Err(UnresolvedMacro { .. }) => (),
                    }
                }
            }

            true
        });
        // Attribute resolution can add unresolved macro invocations, so concatenate the lists.
        self.unresolved_macros.extend(macros);

        for (module_id, macro_call_id, depth) in resolved {
            self.collect_macro_expansion(module_id, macro_call_id, depth);
        }

        res
    }

    fn collect_macro_expansion(
        &mut self,
        module_id: LocalModuleId,
        macro_call_id: MacroCallId,
        depth: usize,
    ) {
        if depth > EXPANSION_DEPTH_LIMIT {
            cov_mark::hit!(macro_expansion_overflow);
            log::warn!("macro expansion is too deep");
            return;
        }
        let file_id = macro_call_id.as_file();

        // First, fetch the raw expansion result for purposes of error reporting. This goes through
        // `macro_expand_error` to avoid depending on the full expansion result (to improve
        // incrementality).
        let loc: MacroCallLoc = self.db.lookup_intern_macro(macro_call_id);
        let err = self.db.macro_expand_error(macro_call_id);
        if let Some(err) = err {
            let diag = match err {
                hir_expand::ExpandError::UnresolvedProcMacro => {
                    // Missing proc macros are non-fatal, so they are handled specially.
                    DefDiagnostic::unresolved_proc_macro(module_id, loc.kind.clone())
                }
                _ => DefDiagnostic::macro_error(module_id, loc.kind.clone(), err.to_string()),
            };

            self.def_map.diagnostics.push(diag);
        }

        // If we've just resolved a derive, record its helper attributes.
        if let MacroCallKind::Derive { ast_id, .. } = &loc.kind {
            if loc.def.krate != self.def_map.krate {
                let def_map = self.db.crate_def_map(loc.def.krate);
                if let Some(def) = def_map.exported_proc_macros.get(&loc.def) {
                    if let ProcMacroKind::CustomDerive { helpers } = &def.kind {
                        self.derive_helpers_in_scope
                            .entry(*ast_id)
                            .or_default()
                            .extend(helpers.iter().cloned());
                    }
                }
            }
        }

        // Then, fetch and process the item tree. This will reuse the expansion result from above.
        let item_tree = self.db.file_item_tree(file_id);
        let mod_dir = self.mod_dirs[&module_id].clone();
        ModCollector {
            def_collector: &mut *self,
            macro_depth: depth,
            file_id,
            module_id,
            item_tree: &item_tree,
            mod_dir,
        }
        .collect(item_tree.top_level_items());
    }

    fn finish(mut self) -> DefMap {
        // Emit diagnostics for all remaining unexpanded macros.

        for directive in &self.unresolved_macros {
            match &directive.kind {
                MacroDirectiveKind::FnLike { ast_id, fragment } => match macro_call_as_call_id(
                    ast_id,
                    *fragment,
                    self.db,
                    self.def_map.krate,
                    |path| {
                        let resolved_res = self.def_map.resolve_path_fp_with_macro(
                            self.db,
                            ResolveMode::Other,
                            directive.module_id,
                            &path,
                            BuiltinShadowMode::Module,
                        );
                        resolved_res.resolved_def.take_macros()
                    },
                    &mut |_| (),
                ) {
                    Ok(_) => (),
                    Err(UnresolvedMacro { path }) => {
                        self.def_map.diagnostics.push(DefDiagnostic::unresolved_macro_call(
                            directive.module_id,
                            ast_id.ast_id,
                            path,
                        ));
                    }
                },
                MacroDirectiveKind::Derive { .. } | MacroDirectiveKind::Attr { .. } => {
                    // FIXME: we might want to diagnose this too
                }
            }
        }

        // Emit diagnostics for all remaining unresolved imports.

        // We'd like to avoid emitting a diagnostics avalanche when some `extern crate` doesn't
        // resolve. We first emit diagnostics for unresolved extern crates and collect the missing
        // crate names. Then we emit diagnostics for unresolved imports, but only if the import
        // doesn't start with an unresolved crate's name. Due to renaming and reexports, this is a
        // heuristic, but it works in practice.
        let mut diagnosed_extern_crates = FxHashSet::default();
        for directive in &self.unresolved_imports {
            if let ImportSource::ExternCrate(krate) = directive.import.source {
                let item_tree = krate.item_tree(self.db);
                let extern_crate = &item_tree[krate.value];

                diagnosed_extern_crates.insert(extern_crate.name.clone());

                self.def_map.diagnostics.push(DefDiagnostic::unresolved_extern_crate(
                    directive.module_id,
                    InFile::new(krate.file_id(), extern_crate.ast_id),
                ));
            }
        }

        for directive in &self.unresolved_imports {
            if let ImportSource::Import { id: import, use_tree } = &directive.import.source {
                match (directive.import.path.segments().first(), &directive.import.path.kind) {
                    (Some(krate), PathKind::Plain | PathKind::Abs) => {
                        if diagnosed_extern_crates.contains(krate) {
                            continue;
                        }
                    }
                    _ => {}
                }

                self.def_map.diagnostics.push(DefDiagnostic::unresolved_import(
                    directive.module_id,
                    *import,
                    *use_tree,
                ));
            }
        }

        self.def_map
    }
}

/// Walks a single module, populating defs, imports and macros
struct ModCollector<'a, 'b> {
    def_collector: &'a mut DefCollector<'b>,
    macro_depth: usize,
    module_id: LocalModuleId,
    file_id: HirFileId,
    item_tree: &'a ItemTree,
    mod_dir: ModDir,
}

impl ModCollector<'_, '_> {
    fn collect(&mut self, items: &[ModItem]) {
        let krate = self.def_collector.def_map.krate;

        // Note: don't assert that inserted value is fresh: it's simply not true
        // for macros.
        self.def_collector.mod_dirs.insert(self.module_id, self.mod_dir.clone());

        // Prelude module is always considered to be `#[macro_use]`.
        if let Some(prelude_module) = self.def_collector.def_map.prelude {
            if prelude_module.krate != krate {
                cov_mark::hit!(prelude_is_macro_use);
                self.def_collector.import_all_macros_exported(self.module_id, prelude_module.krate);
            }
        }

        // This should be processed eagerly instead of deferred to resolving.
        // `#[macro_use] extern crate` is hoisted to imports macros before collecting
        // any other items.
        for item in items {
            let attrs = self.item_tree.attrs(self.def_collector.db, krate, (*item).into());
            if attrs.cfg().map_or(true, |cfg| self.is_cfg_enabled(&cfg)) {
                if let ModItem::ExternCrate(id) = item {
                    let import = self.item_tree[*id].clone();
                    let attrs = self.item_tree.attrs(
                        self.def_collector.db,
                        krate,
                        ModItem::from(*id).into(),
                    );
                    if attrs.by_key("macro_use").exists() {
                        self.def_collector.import_macros_from_extern_crate(self.module_id, &import);
                    }
                }
            }
        }

        for &item in items {
            let attrs = self.item_tree.attrs(self.def_collector.db, krate, item.into());
            if let Some(cfg) = attrs.cfg() {
                if !self.is_cfg_enabled(&cfg) {
                    self.emit_unconfigured_diagnostic(item, &cfg);
                    continue;
                }
            }

            if let Err(()) = self.resolve_attributes(&attrs, item) {
                // Do not process the item. It has at least one non-builtin attribute, so the
                // fixed-point algorithm is required to resolve the rest of them.
                continue;
            }

            let module = self.def_collector.def_map.module_id(self.module_id);

            let mut def = None;
            match item {
                ModItem::Mod(m) => self.collect_module(&self.item_tree[m], &attrs),
                ModItem::Import(import_id) => {
                    let module_id = self.module_id;
                    let imports = Import::from_use(
                        self.def_collector.db,
                        krate,
                        self.item_tree,
                        ItemTreeId::new(self.file_id, import_id),
                    );
                    self.def_collector.unresolved_imports.extend(imports.into_iter().map(
                        |import| ImportDirective {
                            module_id,
                            import,
                            status: PartialResolvedImport::Unresolved,
                        },
                    ));
                }
                ModItem::ExternCrate(import_id) => {
                    self.def_collector.unresolved_imports.push(ImportDirective {
                        module_id: self.module_id,
                        import: Import::from_extern_crate(
                            self.def_collector.db,
                            krate,
                            self.item_tree,
                            ItemTreeId::new(self.file_id, import_id),
                        ),
                        status: PartialResolvedImport::Unresolved,
                    })
                }
                ModItem::ExternBlock(block) => self.collect(&self.item_tree[block].children),
                ModItem::MacroCall(mac) => self.collect_macro_call(&self.item_tree[mac]),
                ModItem::MacroRules(id) => self.collect_macro_rules(id),
                ModItem::MacroDef(id) => self.collect_macro_def(id),
                ModItem::Impl(imp) => {
                    let module = self.def_collector.def_map.module_id(self.module_id);
                    let impl_id =
                        ImplLoc { container: module, id: ItemTreeId::new(self.file_id, imp) }
                            .intern(self.def_collector.db);
                    self.def_collector.def_map.modules[self.module_id].scope.define_impl(impl_id)
                }
                ModItem::Function(id) => {
                    let func = &self.item_tree[id];

                    let ast_id = InFile::new(self.file_id, func.ast_id);
                    self.collect_proc_macro_def(&func.name, ast_id, &attrs);

                    def = Some(DefData {
                        id: FunctionLoc {
                            container: module.into(),
                            id: ItemTreeId::new(self.file_id, id),
                        }
                        .intern(self.def_collector.db)
                        .into(),
                        name: &func.name,
                        visibility: &self.item_tree[func.visibility],
                        has_constructor: false,
                    });
                }
                ModItem::Struct(id) => {
                    let it = &self.item_tree[id];

                    def = Some(DefData {
                        id: StructLoc { container: module, id: ItemTreeId::new(self.file_id, id) }
                            .intern(self.def_collector.db)
                            .into(),
                        name: &it.name,
                        visibility: &self.item_tree[it.visibility],
                        has_constructor: !matches!(it.fields, Fields::Record(_)),
                    });
                }
                ModItem::Union(id) => {
                    let it = &self.item_tree[id];

                    def = Some(DefData {
                        id: UnionLoc { container: module, id: ItemTreeId::new(self.file_id, id) }
                            .intern(self.def_collector.db)
                            .into(),
                        name: &it.name,
                        visibility: &self.item_tree[it.visibility],
                        has_constructor: false,
                    });
                }
                ModItem::Enum(id) => {
                    let it = &self.item_tree[id];

                    def = Some(DefData {
                        id: EnumLoc { container: module, id: ItemTreeId::new(self.file_id, id) }
                            .intern(self.def_collector.db)
                            .into(),
                        name: &it.name,
                        visibility: &self.item_tree[it.visibility],
                        has_constructor: false,
                    });
                }
                ModItem::Const(id) => {
                    let it = &self.item_tree[id];
                    let const_id = ConstLoc {
                        container: module.into(),
                        id: ItemTreeId::new(self.file_id, id),
                    }
                    .intern(self.def_collector.db);

                    match &it.name {
                        Some(name) => {
                            def = Some(DefData {
                                id: const_id.into(),
                                name,
                                visibility: &self.item_tree[it.visibility],
                                has_constructor: false,
                            });
                        }
                        None => {
                            // const _: T = ...;
                            self.def_collector.def_map.modules[self.module_id]
                                .scope
                                .define_unnamed_const(const_id);
                        }
                    }
                }
                ModItem::Static(id) => {
                    let it = &self.item_tree[id];

                    def = Some(DefData {
                        id: StaticLoc { container: module, id: ItemTreeId::new(self.file_id, id) }
                            .intern(self.def_collector.db)
                            .into(),
                        name: &it.name,
                        visibility: &self.item_tree[it.visibility],
                        has_constructor: false,
                    });
                }
                ModItem::Trait(id) => {
                    let it = &self.item_tree[id];

                    def = Some(DefData {
                        id: TraitLoc { container: module, id: ItemTreeId::new(self.file_id, id) }
                            .intern(self.def_collector.db)
                            .into(),
                        name: &it.name,
                        visibility: &self.item_tree[it.visibility],
                        has_constructor: false,
                    });
                }
                ModItem::TypeAlias(id) => {
                    let it = &self.item_tree[id];

                    def = Some(DefData {
                        id: TypeAliasLoc {
                            container: module.into(),
                            id: ItemTreeId::new(self.file_id, id),
                        }
                        .intern(self.def_collector.db)
                        .into(),
                        name: &it.name,
                        visibility: &self.item_tree[it.visibility],
                        has_constructor: false,
                    });
                }
            }

            if let Some(DefData { id, name, visibility, has_constructor }) = def {
                self.def_collector.def_map.modules[self.module_id].scope.declare(id);
                let vis = self
                    .def_collector
                    .def_map
                    .resolve_visibility(self.def_collector.db, self.module_id, visibility)
                    .unwrap_or(Visibility::Public);
                self.def_collector.update(
                    self.module_id,
                    &[(Some(name.clone()), PerNs::from_def(id, vis, has_constructor))],
                    vis,
                    ImportType::Named,
                )
            }
        }
    }

    fn collect_module(&mut self, module: &Mod, attrs: &Attrs) {
        let path_attr = attrs.by_key("path").string_value();
        let is_macro_use = attrs.by_key("macro_use").exists();
        match &module.kind {
            // inline module, just recurse
            ModKind::Inline { items } => {
                let module_id = self.push_child_module(
                    module.name.clone(),
                    AstId::new(self.file_id, module.ast_id),
                    None,
                    &self.item_tree[module.visibility],
                );

                if let Some(mod_dir) = self.mod_dir.descend_into_definition(&module.name, path_attr)
                {
                    ModCollector {
                        def_collector: &mut *self.def_collector,
                        macro_depth: self.macro_depth,
                        module_id,
                        file_id: self.file_id,
                        item_tree: self.item_tree,
                        mod_dir,
                    }
                    .collect(&*items);
                    if is_macro_use {
                        self.import_all_legacy_macros(module_id);
                    }
                }
            }
            // out of line module, resolve, parse and recurse
            ModKind::Outline {} => {
                let ast_id = AstId::new(self.file_id, module.ast_id);
                let db = self.def_collector.db;
                match self.mod_dir.resolve_declaration(db, self.file_id, &module.name, path_attr) {
                    Ok((file_id, is_mod_rs, mod_dir)) => {
                        let item_tree = db.file_item_tree(file_id.into());
                        if item_tree
                            .top_level_attrs(db, self.def_collector.def_map.krate)
                            .cfg()
                            .map_or(true, |cfg| {
                                self.def_collector.cfg_options.check(&cfg) != Some(false)
                            })
                        {
                            let module_id = self.push_child_module(
                                module.name.clone(),
                                ast_id,
                                Some((file_id, is_mod_rs)),
                                &self.item_tree[module.visibility],
                            );
                            ModCollector {
                                def_collector: &mut *self.def_collector,
                                macro_depth: self.macro_depth,
                                module_id,
                                file_id: file_id.into(),
                                item_tree: &item_tree,
                                mod_dir,
                            }
                            .collect(item_tree.top_level_items());
                            if is_macro_use
                                || item_tree
                                    .top_level_attrs(db, self.def_collector.def_map.krate)
                                    .by_key("macro_use")
                                    .exists()
                            {
                                self.import_all_legacy_macros(module_id);
                            }
                        }
                    }
                    Err(candidate) => {
                        self.def_collector.def_map.diagnostics.push(
                            DefDiagnostic::unresolved_module(self.module_id, ast_id, candidate),
                        );
                    }
                };
            }
        }
    }

    fn push_child_module(
        &mut self,
        name: Name,
        declaration: AstId<ast::Module>,
        definition: Option<(FileId, bool)>,
        visibility: &crate::visibility::RawVisibility,
    ) -> LocalModuleId {
        let vis = self
            .def_collector
            .def_map
            .resolve_visibility(self.def_collector.db, self.module_id, visibility)
            .unwrap_or(Visibility::Public);
        let modules = &mut self.def_collector.def_map.modules;
        let origin = match definition {
            None => ModuleOrigin::Inline { definition: declaration },
            Some((definition, is_mod_rs)) => {
                ModuleOrigin::File { declaration, definition, is_mod_rs }
            }
        };
        let res = modules.alloc(ModuleData::new(origin, vis));
        modules[res].parent = Some(self.module_id);
        for (name, mac) in modules[self.module_id].scope.collect_legacy_macros() {
            modules[res].scope.define_legacy_macro(name, mac)
        }
        modules[self.module_id].children.insert(name.clone(), res);
        let module = self.def_collector.def_map.module_id(res);
        let def: ModuleDefId = module.into();
        self.def_collector.def_map.modules[self.module_id].scope.declare(def);
        self.def_collector.update(
            self.module_id,
            &[(Some(name), PerNs::from_def(def, vis, false))],
            vis,
            ImportType::Named,
        );
        res
    }

    /// Resolves attributes on an item.
    ///
    /// Returns `Err` when some attributes could not be resolved to builtins and have been
    /// registered as unresolved.
    ///
    /// If `ignore_up_to` is `Some`, attributes precending and including that attribute will be
    /// assumed to be resolved already.
    fn resolve_attributes(&mut self, attrs: &Attrs, mod_item: ModItem) -> Result<(), ()> {
        let mut ignore_up_to =
            self.def_collector.skip_attrs.get(&InFile::new(self.file_id, mod_item)).copied();
        let iter = attrs
            .iter()
            .dedup_by(|a, b| {
                // FIXME: this should not be required, all attributes on an item should have a
                // unique ID!
                // Still, this occurs because `#[cfg_attr]` can "expand" to multiple attributes:
                //     #[cfg_attr(not(off), unresolved, unresolved)]
                //     struct S;
                // We should come up with a different way to ID attributes.
                a.id == b.id
            })
            .skip_while(|attr| match ignore_up_to {
                Some(id) if attr.id == id => {
                    ignore_up_to = None;
                    true
                }
                Some(_) => true,
                None => false,
            });

        for attr in iter {
            if attr.path.as_ident() == Some(&hir_expand::name![derive]) {
                self.collect_derive(attr, mod_item);
            } else if self.is_builtin_or_registered_attr(&attr.path) {
                continue;
            } else {
                log::debug!("non-builtin attribute {}", attr.path);

                let ast_id = AstIdWithPath::new(
                    self.file_id,
                    mod_item.ast_id(self.item_tree),
                    attr.path.as_ref().clone(),
                );
                self.def_collector.unresolved_macros.push(MacroDirective {
                    module_id: self.module_id,
                    depth: self.macro_depth + 1,
                    kind: MacroDirectiveKind::Attr { ast_id, attr: attr.clone(), mod_item },
                });

                return Err(());
            }
        }

        Ok(())
    }

    fn is_builtin_or_registered_attr(&self, path: &ModPath) -> bool {
        if path.kind == PathKind::Plain {
            if let Some(tool_module) = path.segments().first() {
                let tool_module = tool_module.to_string();
                if builtin_attr::TOOL_MODULES
                    .iter()
                    .copied()
                    .chain(self.def_collector.registered_tools.iter().map(|s| &**s))
                    .any(|m| tool_module == *m)
                {
                    return true;
                }
            }

            if let Some(name) = path.as_ident() {
                let name = name.to_string();
                if builtin_attr::INERT_ATTRIBUTES
                    .iter()
                    .chain(builtin_attr::EXTRA_ATTRIBUTES)
                    .copied()
                    .chain(self.def_collector.registered_attrs.iter().map(|s| &**s))
                    .any(|attr| name == *attr)
                {
                    return true;
                }
            }
        }

        false
    }

    fn collect_derive(&mut self, attr: &Attr, mod_item: ModItem) {
        let ast_id: FileAstId<ast::Item> = match mod_item {
            ModItem::Struct(it) => self.item_tree[it].ast_id.upcast(),
            ModItem::Union(it) => self.item_tree[it].ast_id.upcast(),
            ModItem::Enum(it) => self.item_tree[it].ast_id.upcast(),
            _ => {
                // Cannot use derive on this item.
                // FIXME: diagnose
                return;
            }
        };

        match attr.parse_derive() {
            Some(derive_macros) => {
                for path in derive_macros {
                    let ast_id = AstIdWithPath::new(self.file_id, ast_id, path);
                    self.def_collector.unresolved_macros.push(MacroDirective {
                        module_id: self.module_id,
                        depth: self.macro_depth + 1,
                        kind: MacroDirectiveKind::Derive { ast_id, derive_attr: attr.id },
                    });
                }
            }
            None => {
                // FIXME: diagnose
                log::debug!("malformed derive: {:?}", attr);
            }
        }
    }

    /// If `attrs` registers a procedural macro, collects its definition.
    fn collect_proc_macro_def(&mut self, func_name: &Name, ast_id: AstId<ast::Fn>, attrs: &Attrs) {
        // FIXME: this should only be done in the root module of `proc-macro` crates, not everywhere
        if let Some(proc_macro) = attrs.parse_proc_macro_decl(func_name) {
            self.def_collector.export_proc_macro(proc_macro, ast_id);
        }
    }

    fn collect_macro_rules(&mut self, id: FileItemTreeId<MacroRules>) {
        let krate = self.def_collector.def_map.krate;
        let mac = &self.item_tree[id];
        let attrs = self.item_tree.attrs(self.def_collector.db, krate, ModItem::from(id).into());
        let ast_id = InFile::new(self.file_id, mac.ast_id.upcast());

        let export_attr = attrs.by_key("macro_export");

        let is_export = export_attr.exists();
        let is_local_inner = if is_export {
            export_attr.tt_values().map(|it| &it.token_trees).flatten().any(|it| match it {
                tt::TokenTree::Leaf(tt::Leaf::Ident(ident)) => {
                    ident.text.contains("local_inner_macros")
                }
                _ => false,
            })
        } else {
            false
        };

        // Case 1: builtin macros
        if attrs.by_key("rustc_builtin_macro").exists() {
            // `#[rustc_builtin_macro = "builtin_name"]` overrides the `macro_rules!` name.
            let name;
            let name = match attrs.by_key("rustc_builtin_macro").string_value() {
                Some(it) => {
                    // FIXME: a hacky way to create a Name from string.
                    name = tt::Ident { text: it.clone(), id: tt::TokenId::unspecified() }.as_name();
                    &name
                }
                None => &mac.name,
            };
            let krate = self.def_collector.def_map.krate;
            match find_builtin_macro(name, krate, ast_id) {
                Some(macro_id) => {
                    self.def_collector.define_macro_rules(
                        self.module_id,
                        mac.name.clone(),
                        macro_id,
                        is_export,
                    );
                    return;
                }
                None => {
                    self.def_collector
                        .def_map
                        .diagnostics
                        .push(DefDiagnostic::unimplemented_builtin_macro(self.module_id, ast_id));
                }
            }
        }

        // Case 2: normal `macro_rules!` macro
        let macro_id = MacroDefId {
            krate: self.def_collector.def_map.krate,
            kind: MacroDefKind::Declarative(ast_id),
            local_inner: is_local_inner,
        };
        self.def_collector.define_macro_rules(
            self.module_id,
            mac.name.clone(),
            macro_id,
            is_export,
        );
    }

    fn collect_macro_def(&mut self, id: FileItemTreeId<MacroDef>) {
        let krate = self.def_collector.def_map.krate;
        let mac = &self.item_tree[id];
        let ast_id = InFile::new(self.file_id, mac.ast_id.upcast());

        // Case 1: bulitin macros
        let attrs = self.item_tree.attrs(self.def_collector.db, krate, ModItem::from(id).into());
        if attrs.by_key("rustc_builtin_macro").exists() {
            let macro_id = find_builtin_macro(&mac.name, krate, ast_id)
                .or_else(|| find_builtin_derive(&mac.name, krate, ast_id))
                .or_else(|| find_builtin_attr(&mac.name, krate, ast_id));

            match macro_id {
                Some(macro_id) => {
                    self.def_collector.define_macro_def(
                        self.module_id,
                        mac.name.clone(),
                        macro_id,
                        &self.item_tree[mac.visibility],
                    );
                    return;
                }
                None => {
                    self.def_collector
                        .def_map
                        .diagnostics
                        .push(DefDiagnostic::unimplemented_builtin_macro(self.module_id, ast_id));
                }
            }
        }

        // Case 2: normal `macro`
        let macro_id = MacroDefId {
            krate: self.def_collector.def_map.krate,
            kind: MacroDefKind::Declarative(ast_id),
            local_inner: false,
        };

        self.def_collector.define_macro_def(
            self.module_id,
            mac.name.clone(),
            macro_id,
            &self.item_tree[mac.visibility],
        );
    }

    fn collect_macro_call(&mut self, mac: &MacroCall) {
        let mut ast_id = AstIdWithPath::new(self.file_id, mac.ast_id, (*mac.path).clone());

        // Case 1: try to resolve in legacy scope and expand macro_rules
        let mut error = None;
        match macro_call_as_call_id(
            &ast_id,
            mac.fragment,
            self.def_collector.db,
            self.def_collector.def_map.krate,
            |path| {
                path.as_ident().and_then(|name| {
                    self.def_collector.def_map.with_ancestor_maps(
                        self.def_collector.db,
                        self.module_id,
                        &mut |map, module| map[module].scope.get_legacy_macro(name),
                    )
                })
            },
            &mut |err| {
                error.get_or_insert(err);
            },
        ) {
            Ok(Ok(macro_call_id)) => {
                // Legacy macros need to be expanded immediately, so that any macros they produce
                // are in scope.
                self.def_collector.collect_macro_expansion(
                    self.module_id,
                    macro_call_id,
                    self.macro_depth + 1,
                );

                return;
            }
            Ok(Err(_)) => {
                // Built-in macro failed eager expansion.

                // FIXME: don't parse the file here
                let fragment = hir_expand::to_fragment_kind(
                    &ast_id.ast_id.to_node(self.def_collector.db.upcast()),
                );
                self.def_collector.def_map.diagnostics.push(DefDiagnostic::macro_error(
                    self.module_id,
                    MacroCallKind::FnLike { ast_id: ast_id.ast_id, fragment },
                    error.unwrap().to_string(),
                ));
                return;
            }
            Err(UnresolvedMacro { .. }) => (),
        }

        // Case 2: resolve in module scope, expand during name resolution.
        // We rewrite simple path `macro_name` to `self::macro_name` to force resolve in module scope only.
        if ast_id.path.is_ident() {
            ast_id.path.kind = PathKind::Super(0);
        }

        self.def_collector.unresolved_macros.push(MacroDirective {
            module_id: self.module_id,
            depth: self.macro_depth + 1,
            kind: MacroDirectiveKind::FnLike { ast_id, fragment: mac.fragment },
        });
    }

    fn import_all_legacy_macros(&mut self, module_id: LocalModuleId) {
        let macros = self.def_collector.def_map[module_id].scope.collect_legacy_macros();
        for (name, macro_) in macros {
            self.def_collector.define_legacy_macro(self.module_id, name.clone(), macro_);
        }
    }

    fn is_cfg_enabled(&self, cfg: &CfgExpr) -> bool {
        self.def_collector.cfg_options.check(cfg) != Some(false)
    }

    fn emit_unconfigured_diagnostic(&mut self, item: ModItem, cfg: &CfgExpr) {
        let ast_id = item.ast_id(self.item_tree);

        let ast_id = InFile::new(self.file_id, ast_id);
        self.def_collector.def_map.diagnostics.push(DefDiagnostic::unconfigured_code(
            self.module_id,
            ast_id,
            cfg.clone(),
            self.def_collector.cfg_options.clone(),
        ));
    }
}

#[cfg(test)]
mod tests {
    use crate::{db::DefDatabase, test_db::TestDB};
    use base_db::{fixture::WithFixture, SourceDatabase};

    use super::*;

    fn do_collect_defs(db: &dyn DefDatabase, def_map: DefMap) -> DefMap {
        let mut collector = DefCollector {
            db,
            def_map,
            glob_imports: FxHashMap::default(),
            unresolved_imports: Vec::new(),
            resolved_imports: Vec::new(),
            unresolved_macros: Vec::new(),
            mod_dirs: FxHashMap::default(),
            cfg_options: &CfgOptions::default(),
            proc_macros: Default::default(),
            exports_proc_macros: false,
            from_glob_import: Default::default(),
            skip_attrs: Default::default(),
            derive_helpers_in_scope: Default::default(),
            registered_attrs: Default::default(),
            registered_tools: Default::default(),
        };
        collector.seed_with_top_level();
        collector.collect();
        collector.def_map
    }

    fn do_resolve(not_ra_fixture: &str) -> DefMap {
        let (db, file_id) = TestDB::with_single_file(not_ra_fixture);
        let krate = db.test_crate();

        let edition = db.crate_graph()[krate].edition;
        let module_origin = ModuleOrigin::CrateRoot { definition: file_id };
        let def_map = DefMap::empty(krate, edition, module_origin);
        do_collect_defs(&db, def_map)
    }

    #[test]
    fn test_macro_expand_will_stop_1() {
        do_resolve(
            r#"
macro_rules! foo {
    ($($ty:ty)*) => { foo!($($ty)*); }
}
foo!(KABOOM);
"#,
        );
        do_resolve(
            r#"
macro_rules! foo {
    ($($ty:ty)*) => { foo!(() $($ty)*); }
}
foo!(KABOOM);
"#,
        );
    }

    #[ignore]
    #[test]
    fn test_macro_expand_will_stop_2() {
        // FIXME: this test does succeed, but takes quite a while: 90 seconds in
        // the release mode. That's why the argument is not an ra_fixture --
        // otherwise injection highlighting gets stuck.
        //
        // We need to find a way to fail this faster.
        do_resolve(
            r#"
macro_rules! foo {
    ($($ty:ty)*) => { foo!($($ty)* $($ty)*); }
}
foo!(KABOOM);
"#,
        );
    }
}
