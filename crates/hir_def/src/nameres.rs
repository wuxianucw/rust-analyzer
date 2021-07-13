//! This module implements import-resolution/macro expansion algorithm.
//!
//! The result of this module is `CrateDefMap`: a data structure which contains:
//!
//!   * a tree of modules for the crate
//!   * for each module, a set of items visible in the module (directly declared
//!     or imported)
//!
//! Note that `CrateDefMap` contains fully macro expanded code.
//!
//! Computing `CrateDefMap` can be partitioned into several logically
//! independent "phases". The phases are mutually recursive though, there's no
//! strict ordering.
//!
//! ## Collecting RawItems
//!
//! This happens in the `raw` module, which parses a single source file into a
//! set of top-level items. Nested imports are desugared to flat imports in this
//! phase. Macro calls are represented as a triple of (Path, Option<Name>,
//! TokenTree).
//!
//! ## Collecting Modules
//!
//! This happens in the `collector` module. In this phase, we recursively walk
//! tree of modules, collect raw items from submodules, populate module scopes
//! with defined items (so, we assign item ids in this phase) and record the set
//! of unresolved imports and macros.
//!
//! While we walk tree of modules, we also record macro_rules definitions and
//! expand calls to macro_rules defined macros.
//!
//! ## Resolving Imports
//!
//! We maintain a list of currently unresolved imports. On every iteration, we
//! try to resolve some imports from this list. If the import is resolved, we
//! record it, by adding an item to current module scope and, if necessary, by
//! recursively populating glob imports.
//!
//! ## Resolving Macros
//!
//! macro_rules from the same crate use a global mutable namespace. We expand
//! them immediately, when we collect modules.
//!
//! Macros from other crates (including proc-macros) can be used with
//! `foo::bar!` syntax. We handle them similarly to imports. There's a list of
//! unexpanded macros. On every iteration, we try to resolve each macro call
//! path and, upon success, we run macro expansion and "collect module" phase on
//! the result

pub mod diagnostics;
mod collector;
mod mod_resolution;
mod path_resolution;
mod proc_macro;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use base_db::{CrateId, Edition, FileId};
use hir_expand::{name::Name, InFile, MacroDefId};
use la_arena::Arena;
use profile::Count;
use rustc_hash::FxHashMap;
use stdx::format_to;
use syntax::ast;

use crate::{
    db::DefDatabase,
    item_scope::{BuiltinShadowMode, ItemScope},
    nameres::{diagnostics::DefDiagnostic, path_resolution::ResolveMode},
    path::ModPath,
    per_ns::PerNs,
    visibility::Visibility,
    AstId, BlockId, BlockLoc, LocalModuleId, ModuleDefId, ModuleId,
};

use self::proc_macro::ProcMacroDef;

/// Contains the results of (early) name resolution.
///
/// A `DefMap` stores the module tree and the definitions that are in scope in every module after
/// item-level macros have been expanded.
///
/// Every crate has a primary `DefMap` whose root is the crate's main file (`main.rs`/`lib.rs`),
/// computed by the `crate_def_map` query. Additionally, every block expression introduces the
/// opportunity to write arbitrary item and module hierarchies, and thus gets its own `DefMap` that
/// is computed by the `block_def_map` query.
#[derive(Debug, PartialEq, Eq)]
pub struct DefMap {
    _c: Count<Self>,
    block: Option<BlockInfo>,
    root: LocalModuleId,
    modules: Arena<ModuleData>,
    krate: CrateId,
    /// The prelude module for this crate. This either comes from an import
    /// marked with the `prelude_import` attribute, or (in the normal case) from
    /// a dependency (`std` or `core`).
    prelude: Option<ModuleId>,
    extern_prelude: FxHashMap<Name, ModuleDefId>,

    /// Side table with additional proc. macro info, for use by name resolution in downstream
    /// crates.
    ///
    /// (the primary purpose is to resolve derive helpers)
    exported_proc_macros: FxHashMap<MacroDefId, ProcMacroDef>,

    edition: Edition,
    diagnostics: Vec<DefDiagnostic>,
}

/// For `DefMap`s computed for a block expression, this stores its location in the parent map.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct BlockInfo {
    /// The `BlockId` this `DefMap` was created from.
    block: BlockId,
    /// The containing module.
    parent: ModuleId,
}

impl std::ops::Index<LocalModuleId> for DefMap {
    type Output = ModuleData;
    fn index(&self, id: LocalModuleId) -> &ModuleData {
        &self.modules[id]
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum ModuleOrigin {
    CrateRoot {
        definition: FileId,
    },
    /// Note that non-inline modules, by definition, live inside non-macro file.
    File {
        is_mod_rs: bool,
        declaration: AstId<ast::Module>,
        definition: FileId,
    },
    Inline {
        definition: AstId<ast::Module>,
    },
    /// Pseudo-module introduced by a block scope (contains only inner items).
    BlockExpr {
        block: AstId<ast::BlockExpr>,
    },
}

impl ModuleOrigin {
    fn declaration(&self) -> Option<AstId<ast::Module>> {
        match self {
            ModuleOrigin::File { declaration: module, .. }
            | ModuleOrigin::Inline { definition: module, .. } => Some(*module),
            ModuleOrigin::CrateRoot { .. } | ModuleOrigin::BlockExpr { .. } => None,
        }
    }

    pub fn file_id(&self) -> Option<FileId> {
        match self {
            ModuleOrigin::File { definition, .. } | ModuleOrigin::CrateRoot { definition } => {
                Some(*definition)
            }
            _ => None,
        }
    }

    pub fn is_inline(&self) -> bool {
        match self {
            ModuleOrigin::Inline { .. } | ModuleOrigin::BlockExpr { .. } => true,
            ModuleOrigin::CrateRoot { .. } | ModuleOrigin::File { .. } => false,
        }
    }

    /// Returns a node which defines this module.
    /// That is, a file or a `mod foo {}` with items.
    fn definition_source(&self, db: &dyn DefDatabase) -> InFile<ModuleSource> {
        match self {
            ModuleOrigin::File { definition, .. } | ModuleOrigin::CrateRoot { definition } => {
                let file_id = *definition;
                let sf = db.parse(file_id).tree();
                InFile::new(file_id.into(), ModuleSource::SourceFile(sf))
            }
            ModuleOrigin::Inline { definition } => InFile::new(
                definition.file_id,
                ModuleSource::Module(definition.to_node(db.upcast())),
            ),
            ModuleOrigin::BlockExpr { block } => {
                InFile::new(block.file_id, ModuleSource::BlockExpr(block.to_node(db.upcast())))
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ModuleData {
    /// Where does this module come from?
    pub origin: ModuleOrigin,
    /// Declared visibility of this module.
    pub visibility: Visibility,

    pub parent: Option<LocalModuleId>,
    pub children: FxHashMap<Name, LocalModuleId>,
    pub scope: ItemScope,
}

impl DefMap {
    pub(crate) fn crate_def_map_query(db: &dyn DefDatabase, krate: CrateId) -> Arc<DefMap> {
        let _p = profile::span("crate_def_map_query").detail(|| {
            db.crate_graph()[krate].display_name.as_deref().unwrap_or_default().to_string()
        });

        let crate_graph = db.crate_graph();

        let edition = crate_graph[krate].edition;
        let origin = ModuleOrigin::CrateRoot { definition: crate_graph[krate].root_file_id };
        let def_map = DefMap::empty(krate, edition, origin);
        let def_map = collector::collect_defs(db, def_map, None);

        Arc::new(def_map)
    }

    pub(crate) fn block_def_map_query(
        db: &dyn DefDatabase,
        block_id: BlockId,
    ) -> Option<Arc<DefMap>> {
        let block: BlockLoc = db.lookup_intern_block(block_id);

        let item_tree = db.file_item_tree(block.ast_id.file_id);
        if item_tree.inner_items_of_block(block.ast_id.value).is_empty() {
            return None;
        }

        let block_info = BlockInfo { block: block_id, parent: block.module };

        let parent_map = block.module.def_map(db);
        let mut def_map = DefMap::empty(
            block.module.krate,
            parent_map.edition,
            ModuleOrigin::BlockExpr { block: block.ast_id },
        );
        def_map.block = Some(block_info);

        let def_map = collector::collect_defs(db, def_map, Some(block.ast_id));
        Some(Arc::new(def_map))
    }

    fn empty(krate: CrateId, edition: Edition, root_module_origin: ModuleOrigin) -> DefMap {
        let mut modules: Arena<ModuleData> = Arena::default();

        let local_id = LocalModuleId::from_raw(la_arena::RawIdx::from(0));
        // NB: we use `None` as block here, which would be wrong for implicit
        // modules declared by blocks with items. At the moment, we don't use
        // this visibility for anything outside IDE, so that's probably OK.
        let visibility = Visibility::Module(ModuleId { krate, local_id, block: None });
        let root = modules.alloc(ModuleData::new(root_module_origin, visibility));
        assert_eq!(local_id, root);

        DefMap {
            _c: Count::new(),
            block: None,
            krate,
            edition,
            extern_prelude: FxHashMap::default(),
            exported_proc_macros: FxHashMap::default(),
            prelude: None,
            root,
            modules,
            diagnostics: Vec::new(),
        }
    }

    pub fn modules_for_file(&self, file_id: FileId) -> impl Iterator<Item = LocalModuleId> + '_ {
        self.modules
            .iter()
            .filter(move |(_id, data)| data.origin.file_id() == Some(file_id))
            .map(|(id, _data)| id)
    }

    pub fn modules(&self) -> impl Iterator<Item = (LocalModuleId, &ModuleData)> + '_ {
        self.modules.iter()
    }

    pub fn root(&self) -> LocalModuleId {
        self.root
    }

    pub(crate) fn krate(&self) -> CrateId {
        self.krate
    }

    pub(crate) fn block_id(&self) -> Option<BlockId> {
        self.block.as_ref().map(|block| block.block)
    }

    pub(crate) fn prelude(&self) -> Option<ModuleId> {
        self.prelude
    }

    pub(crate) fn extern_prelude(&self) -> impl Iterator<Item = (&Name, &ModuleDefId)> + '_ {
        self.extern_prelude.iter()
    }

    pub fn module_id(&self, local_id: LocalModuleId) -> ModuleId {
        let block = self.block.as_ref().map(|b| b.block);
        ModuleId { krate: self.krate, local_id, block }
    }

    pub(crate) fn crate_root(&self, db: &dyn DefDatabase) -> ModuleId {
        self.with_ancestor_maps(db, self.root, &mut |def_map, _module| {
            if def_map.block.is_none() {
                Some(def_map.module_id(def_map.root))
            } else {
                None
            }
        })
        .expect("DefMap chain without root")
    }

    pub(crate) fn resolve_path(
        &self,
        db: &dyn DefDatabase,
        original_module: LocalModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
    ) -> (PerNs, Option<usize>) {
        let res =
            self.resolve_path_fp_with_macro(db, ResolveMode::Other, original_module, path, shadow);
        (res.resolved_def, res.segment_index)
    }

    pub(crate) fn resolve_path_locally(
        &self,
        db: &dyn DefDatabase,
        original_module: LocalModuleId,
        path: &ModPath,
        shadow: BuiltinShadowMode,
    ) -> (PerNs, Option<usize>) {
        let res = self.resolve_path_fp_with_macro_single(
            db,
            ResolveMode::Other,
            original_module,
            path,
            shadow,
        );
        (res.resolved_def, res.segment_index)
    }

    /// Ascends the `DefMap` hierarchy and calls `f` with every `DefMap` and containing module.
    ///
    /// If `f` returns `Some(val)`, iteration is stopped and `Some(val)` is returned. If `f` returns
    /// `None`, iteration continues.
    pub fn with_ancestor_maps<T>(
        &self,
        db: &dyn DefDatabase,
        local_mod: LocalModuleId,
        f: &mut dyn FnMut(&DefMap, LocalModuleId) -> Option<T>,
    ) -> Option<T> {
        if let Some(it) = f(self, local_mod) {
            return Some(it);
        }
        let mut block = self.block;
        while let Some(block_info) = block {
            let parent = block_info.parent.def_map(db);
            if let Some(it) = f(&parent, block_info.parent.local_id) {
                return Some(it);
            }
            block = parent.block;
        }

        None
    }

    /// If this `DefMap` is for a block expression, returns the module containing the block (which
    /// might again be a block, or a module inside a block).
    pub fn parent(&self) -> Option<ModuleId> {
        Some(self.block?.parent)
    }

    /// Returns the module containing `local_mod`, either the parent `mod`, or the module containing
    /// the block, if `self` corresponds to a block expression.
    pub fn containing_module(&self, local_mod: LocalModuleId) -> Option<ModuleId> {
        match &self[local_mod].parent {
            Some(parent) => Some(self.module_id(*parent)),
            None => self.block.as_ref().map(|block| block.parent),
        }
    }

    // FIXME: this can use some more human-readable format (ideally, an IR
    // even), as this should be a great debugging aid.
    pub fn dump(&self, db: &dyn DefDatabase) -> String {
        let mut buf = String::new();
        let mut arc;
        let mut current_map = self;
        while let Some(block) = &current_map.block {
            go(&mut buf, current_map, "block scope", current_map.root);
            buf.push('\n');
            arc = block.parent.def_map(db);
            current_map = &*arc;
        }
        go(&mut buf, current_map, "crate", current_map.root);
        return buf;

        fn go(buf: &mut String, map: &DefMap, path: &str, module: LocalModuleId) {
            format_to!(buf, "{}\n", path);

            map.modules[module].scope.dump(buf);

            for (name, child) in map.modules[module].children.iter() {
                let path = format!("{}::{}", path, name);
                buf.push('\n');
                go(buf, map, &path, *child);
            }
        }
    }

    pub fn dump_block_scopes(&self, db: &dyn DefDatabase) -> String {
        let mut buf = String::new();
        let mut arc;
        let mut current_map = self;
        while let Some(block) = &current_map.block {
            format_to!(buf, "{:?} in {:?}\n", block.block, block.parent);
            arc = block.parent.def_map(db);
            current_map = &*arc;
        }

        format_to!(buf, "crate scope\n");
        buf
    }

    fn shrink_to_fit(&mut self) {
        // Exhaustive match to require handling new fields.
        let Self {
            _c: _,
            exported_proc_macros,
            extern_prelude,
            diagnostics,
            modules,
            block: _,
            edition: _,
            krate: _,
            prelude: _,
            root: _,
        } = self;

        extern_prelude.shrink_to_fit();
        exported_proc_macros.shrink_to_fit();
        diagnostics.shrink_to_fit();
        modules.shrink_to_fit();
        for (_, module) in modules.iter_mut() {
            module.children.shrink_to_fit();
            module.scope.shrink_to_fit();
        }
    }

    /// Get a reference to the def map's diagnostics.
    pub fn diagnostics(&self) -> &[DefDiagnostic] {
        self.diagnostics.as_slice()
    }
}

impl ModuleData {
    pub(crate) fn new(origin: ModuleOrigin, visibility: Visibility) -> Self {
        ModuleData {
            origin,
            visibility,
            parent: None,
            children: FxHashMap::default(),
            scope: ItemScope::default(),
        }
    }

    /// Returns a node which defines this module. That is, a file or a `mod foo {}` with items.
    pub fn definition_source(&self, db: &dyn DefDatabase) -> InFile<ModuleSource> {
        self.origin.definition_source(db)
    }

    /// Returns a node which declares this module, either a `mod foo;` or a `mod foo {}`.
    /// `None` for the crate root or block.
    pub fn declaration_source(&self, db: &dyn DefDatabase) -> Option<InFile<ast::Module>> {
        let decl = self.origin.declaration()?;
        let value = decl.to_node(db.upcast());
        Some(InFile { file_id: decl.file_id, value })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleSource {
    SourceFile(ast::SourceFile),
    Module(ast::Module),
    BlockExpr(ast::BlockExpr),
}
