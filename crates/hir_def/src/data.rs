//! Contains basic data about various HIR declarations.

use std::sync::Arc;

use hir_expand::{name::Name, InFile};
use syntax::ast;

use crate::{
    attr::Attrs,
    body::Expander,
    db::DefDatabase,
    intern::Interned,
    item_tree::{self, AssocItem, FnFlags, ItemTreeId, ModItem, Param},
    type_ref::{TraitRef, TypeBound, TypeRef},
    visibility::RawVisibility,
    AssocContainerId, AssocItemId, ConstId, ConstLoc, FunctionId, FunctionLoc, HasModule, ImplId,
    Intern, Lookup, ModuleId, StaticId, TraitId, TypeAliasId, TypeAliasLoc,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionData {
    pub name: Name,
    pub params: Vec<Interned<TypeRef>>,
    pub ret_type: Interned<TypeRef>,
    pub async_ret_type: Option<Interned<TypeRef>>,
    pub attrs: Attrs,
    pub visibility: RawVisibility,
    pub abi: Option<Interned<str>>,
    flags: FnFlags,
}

impl FunctionData {
    pub(crate) fn fn_data_query(db: &dyn DefDatabase, func: FunctionId) -> Arc<FunctionData> {
        let loc = func.lookup(db);
        let krate = loc.container.module(db).krate;
        let crate_graph = db.crate_graph();
        let cfg_options = &crate_graph[krate].cfg_options;
        let item_tree = loc.id.item_tree(db);
        let func = &item_tree[loc.id.value];

        let enabled_params = func
            .params
            .clone()
            .filter(|&param| item_tree.attrs(db, krate, param.into()).is_cfg_enabled(cfg_options));

        // If last cfg-enabled param is a `...` param, it's a varargs function.
        let is_varargs = enabled_params
            .clone()
            .next_back()
            .map_or(false, |param| matches!(item_tree[param], Param::Varargs));

        let mut flags = func.flags;
        if is_varargs {
            flags.bits |= FnFlags::IS_VARARGS;
        }

        Arc::new(FunctionData {
            name: func.name.clone(),
            params: enabled_params
                .clone()
                .filter_map(|id| match &item_tree[id] {
                    Param::Normal(ty) => Some(ty.clone()),
                    Param::Varargs => None,
                })
                .collect(),
            ret_type: func.ret_type.clone(),
            async_ret_type: func.async_ret_type.clone(),
            attrs: item_tree.attrs(db, krate, ModItem::from(loc.id.value).into()),
            visibility: item_tree[func.visibility].clone(),
            abi: func.abi.clone(),
            flags,
        })
    }

    pub fn has_body(&self) -> bool {
        self.flags.bits & FnFlags::HAS_BODY != 0
    }

    /// True if the first param is `self`. This is relevant to decide whether this
    /// can be called as a method.
    pub fn has_self_param(&self) -> bool {
        self.flags.bits & FnFlags::HAS_SELF_PARAM != 0
    }

    pub fn is_default(&self) -> bool {
        self.flags.bits & FnFlags::IS_DEFAULT != 0
    }

    pub fn is_const(&self) -> bool {
        self.flags.bits & FnFlags::IS_CONST != 0
    }

    pub fn is_async(&self) -> bool {
        self.flags.bits & FnFlags::IS_ASYNC != 0
    }

    pub fn is_unsafe(&self) -> bool {
        self.flags.bits & FnFlags::IS_UNSAFE != 0
    }

    pub fn is_in_extern_block(&self) -> bool {
        self.flags.bits & FnFlags::IS_IN_EXTERN_BLOCK != 0
    }

    pub fn is_varargs(&self) -> bool {
        self.flags.bits & FnFlags::IS_VARARGS != 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeAliasData {
    pub name: Name,
    pub type_ref: Option<Interned<TypeRef>>,
    pub visibility: RawVisibility,
    pub is_extern: bool,
    /// Bounds restricting the type alias itself (eg. `type Ty: Bound;` in a trait or impl).
    pub bounds: Vec<Interned<TypeBound>>,
}

impl TypeAliasData {
    pub(crate) fn type_alias_data_query(
        db: &dyn DefDatabase,
        typ: TypeAliasId,
    ) -> Arc<TypeAliasData> {
        let loc = typ.lookup(db);
        let item_tree = loc.id.item_tree(db);
        let typ = &item_tree[loc.id.value];

        Arc::new(TypeAliasData {
            name: typ.name.clone(),
            type_ref: typ.type_ref.clone(),
            visibility: item_tree[typ.visibility].clone(),
            is_extern: typ.is_extern,
            bounds: typ.bounds.to_vec(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraitData {
    pub name: Name,
    pub items: Vec<(Name, AssocItemId)>,
    pub is_auto: bool,
    pub is_unsafe: bool,
    pub visibility: RawVisibility,
    /// Whether the trait has `#[rust_skip_array_during_method_dispatch]`. `hir_ty` will ignore
    /// method calls to this trait's methods when the receiver is an array and the crate edition is
    /// 2015 or 2018.
    pub skip_array_during_method_dispatch: bool,
}

impl TraitData {
    pub(crate) fn trait_data_query(db: &dyn DefDatabase, tr: TraitId) -> Arc<TraitData> {
        let tr_loc = tr.lookup(db);
        let item_tree = tr_loc.id.item_tree(db);
        let tr_def = &item_tree[tr_loc.id.value];
        let name = tr_def.name.clone();
        let is_auto = tr_def.is_auto;
        let is_unsafe = tr_def.is_unsafe;
        let module_id = tr_loc.container;
        let container = AssocContainerId::TraitId(tr);
        let visibility = item_tree[tr_def.visibility].clone();
        let mut expander = Expander::new(db, tr_loc.id.file_id(), module_id);
        let skip_array_during_method_dispatch = item_tree
            .attrs(db, tr_loc.container.krate(), ModItem::from(tr_loc.id.value).into())
            .by_key("rustc_skip_array_during_method_dispatch")
            .exists();

        let items = collect_items(
            db,
            module_id,
            &mut expander,
            tr_def.items.iter().copied(),
            tr_loc.id.tree_id(),
            container,
            100,
        );

        Arc::new(TraitData {
            name,
            items,
            is_auto,
            is_unsafe,
            visibility,
            skip_array_during_method_dispatch,
        })
    }

    pub fn associated_types(&self) -> impl Iterator<Item = TypeAliasId> + '_ {
        self.items.iter().filter_map(|(_name, item)| match item {
            AssocItemId::TypeAliasId(t) => Some(*t),
            _ => None,
        })
    }

    pub fn associated_type_by_name(&self, name: &Name) -> Option<TypeAliasId> {
        self.items.iter().find_map(|(item_name, item)| match item {
            AssocItemId::TypeAliasId(t) if item_name == name => Some(*t),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImplData {
    pub target_trait: Option<Interned<TraitRef>>,
    pub self_ty: Interned<TypeRef>,
    pub items: Vec<AssocItemId>,
    pub is_negative: bool,
}

impl ImplData {
    pub(crate) fn impl_data_query(db: &dyn DefDatabase, id: ImplId) -> Arc<ImplData> {
        let _p = profile::span("impl_data_query");
        let impl_loc = id.lookup(db);

        let item_tree = impl_loc.id.item_tree(db);
        let impl_def = &item_tree[impl_loc.id.value];
        let target_trait = impl_def.target_trait.clone();
        let self_ty = impl_def.self_ty.clone();
        let is_negative = impl_def.is_negative;
        let module_id = impl_loc.container;
        let container = AssocContainerId::ImplId(id);
        let mut expander = Expander::new(db, impl_loc.id.file_id(), module_id);

        let items = collect_items(
            db,
            module_id,
            &mut expander,
            impl_def.items.iter().copied(),
            impl_loc.id.tree_id(),
            container,
            100,
        );
        let items = items.into_iter().map(|(_, item)| item).collect();

        Arc::new(ImplData { target_trait, self_ty, items, is_negative })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConstData {
    /// const _: () = ();
    pub name: Option<Name>,
    pub type_ref: Interned<TypeRef>,
    pub visibility: RawVisibility,
}

impl ConstData {
    pub(crate) fn const_data_query(db: &dyn DefDatabase, konst: ConstId) -> Arc<ConstData> {
        let loc = konst.lookup(db);
        let item_tree = loc.id.item_tree(db);
        let konst = &item_tree[loc.id.value];

        Arc::new(ConstData {
            name: konst.name.clone(),
            type_ref: konst.type_ref.clone(),
            visibility: item_tree[konst.visibility].clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticData {
    pub name: Option<Name>,
    pub type_ref: Interned<TypeRef>,
    pub visibility: RawVisibility,
    pub mutable: bool,
    pub is_extern: bool,
}

impl StaticData {
    pub(crate) fn static_data_query(db: &dyn DefDatabase, konst: StaticId) -> Arc<StaticData> {
        let node = konst.lookup(db);
        let item_tree = node.id.item_tree(db);
        let statik = &item_tree[node.id.value];

        Arc::new(StaticData {
            name: Some(statik.name.clone()),
            type_ref: statik.type_ref.clone(),
            visibility: item_tree[statik.visibility].clone(),
            mutable: statik.mutable,
            is_extern: statik.is_extern,
        })
    }
}

fn collect_items(
    db: &dyn DefDatabase,
    module: ModuleId,
    expander: &mut Expander,
    assoc_items: impl Iterator<Item = AssocItem>,
    tree_id: item_tree::TreeId,
    container: AssocContainerId,
    limit: usize,
) -> Vec<(Name, AssocItemId)> {
    if limit == 0 {
        return Vec::new();
    }

    let item_tree = tree_id.item_tree(db);
    let crate_graph = db.crate_graph();
    let cfg_options = &crate_graph[module.krate].cfg_options;

    let mut items = Vec::new();
    for item in assoc_items {
        let attrs = item_tree.attrs(db, module.krate, ModItem::from(item).into());
        if !attrs.is_cfg_enabled(cfg_options) {
            continue;
        }

        match item {
            AssocItem::Function(id) => {
                let item = &item_tree[id];
                let def = FunctionLoc { container, id: ItemTreeId::new(tree_id, id) }.intern(db);
                items.push((item.name.clone(), def.into()));
            }
            AssocItem::Const(id) => {
                let item = &item_tree[id];
                let name = match item.name.clone() {
                    Some(name) => name,
                    None => continue,
                };
                let def = ConstLoc { container, id: ItemTreeId::new(tree_id, id) }.intern(db);
                items.push((name, def.into()));
            }
            AssocItem::TypeAlias(id) => {
                let item = &item_tree[id];
                let def = TypeAliasLoc { container, id: ItemTreeId::new(tree_id, id) }.intern(db);
                items.push((item.name.clone(), def.into()));
            }
            AssocItem::MacroCall(call) => {
                let call = &item_tree[call];
                let ast_id_map = db.ast_id_map(tree_id.file_id());
                let root = db.parse_or_expand(tree_id.file_id()).unwrap();
                let call = ast_id_map.get(call.ast_id).to_node(&root);
                let res = expander.enter_expand(db, call);

                if let Ok(res) = res {
                    if let Some((mark, mac)) = res.value {
                        let src: InFile<ast::MacroItems> = expander.to_source(mac);
                        let tree_id = item_tree::TreeId::new(src.file_id, None);
                        let item_tree = tree_id.item_tree(db);
                        let iter =
                            item_tree.top_level_items().iter().filter_map(ModItem::as_assoc_item);
                        items.extend(collect_items(
                            db,
                            module,
                            expander,
                            iter,
                            tree_id,
                            container,
                            limit - 1,
                        ));

                        expander.exit(db, mark);
                    }
                }
            }
        }
    }

    items
}
