//! Unification and canonicalization logic.

use std::{fmt, mem, sync::Arc};

use chalk_ir::{
    cast::Cast, fold::Fold, interner::HasInterner, zip::Zip, FloatTy, IntTy, TyVariableKind,
    UniverseIndex,
};
use chalk_solve::infer::ParameterEnaVariableExt;
use ena::unify::UnifyKey;

use super::{InferOk, InferResult, InferenceContext, TypeError};
use crate::{
    db::HirDatabase, fold_tys, static_lifetime, AliasEq, AliasTy, BoundVar, Canonical,
    DebruijnIndex, GenericArg, Goal, Guidance, InEnvironment, InferenceVar, Interner, ProjectionTy,
    Scalar, Solution, Substitution, TraitEnvironment, Ty, TyKind, VariableKind,
};

impl<'a> InferenceContext<'a> {
    pub(super) fn canonicalize<T: Fold<Interner> + HasInterner<Interner = Interner>>(
        &mut self,
        t: T,
    ) -> Canonicalized<T::Result>
    where
        T::Result: HasInterner<Interner = Interner>,
    {
        // try to resolve obligations before canonicalizing, since this might
        // result in new knowledge about variables
        self.resolve_obligations_as_possible();
        self.table.canonicalize(t)
    }
}

#[derive(Debug, Clone)]
pub(super) struct Canonicalized<T>
where
    T: HasInterner<Interner = Interner>,
{
    pub(super) value: Canonical<T>,
    free_vars: Vec<GenericArg>,
}

impl<T: HasInterner<Interner = Interner>> Canonicalized<T> {
    pub(super) fn decanonicalize_ty(&self, ty: Ty) -> Ty {
        chalk_ir::Substitute::apply(&self.free_vars, ty, &Interner)
    }

    pub(super) fn apply_solution(
        &self,
        ctx: &mut InferenceTable,
        solution: Canonical<Substitution>,
    ) {
        // the solution may contain new variables, which we need to convert to new inference vars
        let new_vars = Substitution::from_iter(
            &Interner,
            solution.binders.iter(&Interner).map(|k| match k.kind {
                VariableKind::Ty(TyVariableKind::General) => ctx.new_type_var().cast(&Interner),
                VariableKind::Ty(TyVariableKind::Integer) => ctx.new_integer_var().cast(&Interner),
                VariableKind::Ty(TyVariableKind::Float) => ctx.new_float_var().cast(&Interner),
                // Chalk can sometimes return new lifetime variables. We just use the static lifetime everywhere
                VariableKind::Lifetime => static_lifetime().cast(&Interner),
                _ => panic!("const variable in solution"),
            }),
        );
        for (i, v) in solution.value.iter(&Interner).enumerate() {
            let var = self.free_vars[i].clone();
            if let Some(ty) = v.ty(&Interner) {
                // eagerly replace projections in the type; we may be getting types
                // e.g. from where clauses where this hasn't happened yet
                let ty = ctx.normalize_associated_types_in(new_vars.apply(ty.clone(), &Interner));
                ctx.unify(var.assert_ty_ref(&Interner), &ty);
            } else {
                let _ = ctx.try_unify(&var, &new_vars.apply(v.clone(), &Interner));
            }
        }
    }
}

pub fn could_unify(
    db: &dyn HirDatabase,
    env: Arc<TraitEnvironment>,
    tys: &Canonical<(Ty, Ty)>,
) -> bool {
    unify(db, env, tys).is_some()
}

pub(crate) fn unify(
    db: &dyn HirDatabase,
    env: Arc<TraitEnvironment>,
    tys: &Canonical<(Ty, Ty)>,
) -> Option<Substitution> {
    let mut table = InferenceTable::new(db, env);
    let vars = Substitution::from_iter(
        &Interner,
        tys.binders
            .iter(&Interner)
            // we always use type vars here because we want everything to
            // fallback to Unknown in the end (kind of hacky, as below)
            .map(|_| table.new_type_var()),
    );
    let ty1_with_vars = vars.apply(tys.value.0.clone(), &Interner);
    let ty2_with_vars = vars.apply(tys.value.1.clone(), &Interner);
    if !table.unify(&ty1_with_vars, &ty2_with_vars) {
        return None;
    }
    // default any type vars that weren't unified back to their original bound vars
    // (kind of hacky)
    let find_var = |iv| {
        vars.iter(&Interner).position(|v| match v.interned() {
            chalk_ir::GenericArgData::Ty(ty) => ty.inference_var(&Interner),
            chalk_ir::GenericArgData::Lifetime(lt) => lt.inference_var(&Interner),
            chalk_ir::GenericArgData::Const(c) => c.inference_var(&Interner),
        } == Some(iv))
    };
    let fallback = |iv, kind, default, binder| match kind {
        chalk_ir::VariableKind::Ty(_ty_kind) => find_var(iv)
            .map_or(default, |i| BoundVar::new(binder, i).to_ty(&Interner).cast(&Interner)),
        chalk_ir::VariableKind::Lifetime => find_var(iv)
            .map_or(default, |i| BoundVar::new(binder, i).to_lifetime(&Interner).cast(&Interner)),
        chalk_ir::VariableKind::Const(ty) => find_var(iv)
            .map_or(default, |i| BoundVar::new(binder, i).to_const(&Interner, ty).cast(&Interner)),
    };
    Some(Substitution::from_iter(
        &Interner,
        vars.iter(&Interner)
            .map(|v| table.resolve_with_fallback(v.assert_ty_ref(&Interner).clone(), fallback)),
    ))
}

#[derive(Copy, Clone, Debug)]
pub(crate) struct TypeVariableData {
    diverging: bool,
}

type ChalkInferenceTable = chalk_solve::infer::InferenceTable<Interner>;

#[derive(Clone)]
pub(crate) struct InferenceTable<'a> {
    pub(crate) db: &'a dyn HirDatabase,
    pub(crate) trait_env: Arc<TraitEnvironment>,
    var_unification_table: ChalkInferenceTable,
    type_variable_table: Vec<TypeVariableData>,
    pending_obligations: Vec<Canonicalized<InEnvironment<Goal>>>,
}

impl<'a> InferenceTable<'a> {
    pub(crate) fn new(db: &'a dyn HirDatabase, trait_env: Arc<TraitEnvironment>) -> Self {
        InferenceTable {
            db,
            trait_env,
            var_unification_table: ChalkInferenceTable::new(),
            type_variable_table: Vec::new(),
            pending_obligations: Vec::new(),
        }
    }

    /// Chalk doesn't know about the `diverging` flag, so when it unifies two
    /// type variables of which one is diverging, the chosen root might not be
    /// diverging and we have no way of marking it as such at that time. This
    /// function goes through all type variables and make sure their root is
    /// marked as diverging if necessary, so that resolving them gives the right
    /// result.
    pub(super) fn propagate_diverging_flag(&mut self) {
        for i in 0..self.type_variable_table.len() {
            if !self.type_variable_table[i].diverging {
                continue;
            }
            let v = InferenceVar::from(i as u32);
            let root = self.var_unification_table.inference_var_root(v);
            if let Some(data) = self.type_variable_table.get_mut(root.index() as usize) {
                data.diverging = true;
            }
        }
    }

    pub(super) fn set_diverging(&mut self, iv: InferenceVar, diverging: bool) {
        self.type_variable_table[iv.index() as usize].diverging = diverging;
    }

    fn fallback_value(&self, iv: InferenceVar, kind: TyVariableKind) -> Ty {
        match kind {
            _ if self
                .type_variable_table
                .get(iv.index() as usize)
                .map_or(false, |data| data.diverging) =>
            {
                TyKind::Never
            }
            TyVariableKind::General => TyKind::Error,
            TyVariableKind::Integer => TyKind::Scalar(Scalar::Int(IntTy::I32)),
            TyVariableKind::Float => TyKind::Scalar(Scalar::Float(FloatTy::F64)),
        }
        .intern(&Interner)
    }

    pub(super) fn canonicalize<T: Fold<Interner> + HasInterner<Interner = Interner>>(
        &mut self,
        t: T,
    ) -> Canonicalized<T::Result>
    where
        T::Result: HasInterner<Interner = Interner>,
    {
        let result = self.var_unification_table.canonicalize(&Interner, t);
        let free_vars = result
            .free_vars
            .into_iter()
            .map(|free_var| free_var.to_generic_arg(&Interner))
            .collect();
        Canonicalized { value: result.quantified, free_vars }
    }

    /// Recurses through the given type, normalizing associated types mentioned
    /// in it by replacing them by type variables and registering obligations to
    /// resolve later. This should be done once for every type we get from some
    /// type annotation (e.g. from a let type annotation, field type or function
    /// call). `make_ty` handles this already, but e.g. for field types we need
    /// to do it as well.
    pub(super) fn normalize_associated_types_in(&mut self, ty: Ty) -> Ty {
        fold_tys(
            ty,
            |ty, _| match ty.kind(&Interner) {
                TyKind::Alias(AliasTy::Projection(proj_ty)) => {
                    self.normalize_projection_ty(proj_ty.clone())
                }
                _ => ty,
            },
            DebruijnIndex::INNERMOST,
        )
    }

    pub(super) fn normalize_projection_ty(&mut self, proj_ty: ProjectionTy) -> Ty {
        let var = self.new_type_var();
        let alias_eq = AliasEq { alias: AliasTy::Projection(proj_ty), ty: var.clone() };
        let obligation = alias_eq.cast(&Interner);
        self.register_obligation(obligation);
        var
    }

    fn extend_type_variable_table(&mut self, to_index: usize) {
        self.type_variable_table.extend(
            (0..1 + to_index - self.type_variable_table.len())
                .map(|_| TypeVariableData { diverging: false }),
        );
    }

    fn new_var(&mut self, kind: TyVariableKind, diverging: bool) -> Ty {
        let var = self.var_unification_table.new_variable(UniverseIndex::ROOT);
        // Chalk might have created some type variables for its own purposes that we don't know about...
        self.extend_type_variable_table(var.index() as usize);
        assert_eq!(var.index() as usize, self.type_variable_table.len() - 1);
        self.type_variable_table[var.index() as usize].diverging = diverging;
        var.to_ty_with_kind(&Interner, kind)
    }

    pub(crate) fn new_type_var(&mut self) -> Ty {
        self.new_var(TyVariableKind::General, false)
    }

    pub(crate) fn new_integer_var(&mut self) -> Ty {
        self.new_var(TyVariableKind::Integer, false)
    }

    pub(crate) fn new_float_var(&mut self) -> Ty {
        self.new_var(TyVariableKind::Float, false)
    }

    pub(crate) fn new_maybe_never_var(&mut self) -> Ty {
        self.new_var(TyVariableKind::General, true)
    }

    pub(crate) fn resolve_with_fallback<T>(
        &mut self,
        t: T,
        fallback: impl Fn(InferenceVar, VariableKind, GenericArg, DebruijnIndex) -> GenericArg,
    ) -> T::Result
    where
        T: HasInterner<Interner = Interner> + Fold<Interner>,
    {
        self.resolve_with_fallback_inner(&mut Vec::new(), t, &fallback)
    }

    fn resolve_with_fallback_inner<T>(
        &mut self,
        var_stack: &mut Vec<InferenceVar>,
        t: T,
        fallback: &impl Fn(InferenceVar, VariableKind, GenericArg, DebruijnIndex) -> GenericArg,
    ) -> T::Result
    where
        T: HasInterner<Interner = Interner> + Fold<Interner>,
    {
        t.fold_with(
            &mut resolve::Resolver { table: self, var_stack, fallback },
            DebruijnIndex::INNERMOST,
        )
        .expect("fold failed unexpectedly")
    }

    pub(crate) fn resolve_completely<T>(&mut self, t: T) -> T::Result
    where
        T: HasInterner<Interner = Interner> + Fold<Interner>,
    {
        self.resolve_with_fallback(t, |_, _, d, _| d)
    }

    /// Unify two types and register new trait goals that arise from that.
    pub(crate) fn unify(&mut self, ty1: &Ty, ty2: &Ty) -> bool {
        let result = if let Ok(r) = self.try_unify(ty1, ty2) {
            r
        } else {
            return false;
        };
        self.register_infer_ok(result);
        true
    }

    /// Unify two types and return new trait goals arising from it, so the
    /// caller needs to deal with them.
    pub(crate) fn try_unify<T: Zip<Interner>>(&mut self, t1: &T, t2: &T) -> InferResult<()> {
        match self.var_unification_table.relate(
            &Interner,
            &self.db,
            &self.trait_env.env,
            chalk_ir::Variance::Invariant,
            t1,
            t2,
        ) {
            Ok(result) => Ok(InferOk { goals: result.goals, value: () }),
            Err(chalk_ir::NoSolution) => Err(TypeError),
        }
    }

    /// If `ty` is a type variable with known type, returns that type;
    /// otherwise, return ty.
    pub(crate) fn resolve_ty_shallow(&mut self, ty: &Ty) -> Ty {
        self.var_unification_table.normalize_ty_shallow(&Interner, ty).unwrap_or_else(|| ty.clone())
    }

    pub(crate) fn register_obligation(&mut self, goal: Goal) {
        let in_env = InEnvironment::new(&self.trait_env.env, goal);
        self.register_obligation_in_env(in_env)
    }

    fn register_obligation_in_env(&mut self, goal: InEnvironment<Goal>) {
        let canonicalized = self.canonicalize(goal);
        if !self.try_resolve_obligation(&canonicalized) {
            self.pending_obligations.push(canonicalized);
        }
    }

    pub(crate) fn register_infer_ok<T>(&mut self, infer_ok: InferOk<T>) {
        infer_ok.goals.into_iter().for_each(|goal| self.register_obligation_in_env(goal));
    }

    pub(crate) fn resolve_obligations_as_possible(&mut self) {
        let _span = profile::span("resolve_obligations_as_possible");
        let mut changed = true;
        let mut obligations = Vec::new();
        while changed {
            changed = false;
            mem::swap(&mut self.pending_obligations, &mut obligations);
            for canonicalized in obligations.drain(..) {
                if !self.check_changed(&canonicalized) {
                    self.pending_obligations.push(canonicalized);
                    continue;
                }
                changed = true;
                let uncanonical = chalk_ir::Substitute::apply(
                    &canonicalized.free_vars,
                    canonicalized.value.value,
                    &Interner,
                );
                self.register_obligation_in_env(uncanonical);
            }
        }
    }

    /// This checks whether any of the free variables in the `canonicalized`
    /// have changed (either been unified with another variable, or with a
    /// value). If this is not the case, we don't need to try to solve the goal
    /// again -- it'll give the same result as last time.
    fn check_changed(&mut self, canonicalized: &Canonicalized<InEnvironment<Goal>>) -> bool {
        canonicalized.free_vars.iter().any(|var| {
            let iv = match var.data(&Interner) {
                chalk_ir::GenericArgData::Ty(ty) => ty.inference_var(&Interner),
                chalk_ir::GenericArgData::Lifetime(lt) => lt.inference_var(&Interner),
                chalk_ir::GenericArgData::Const(c) => c.inference_var(&Interner),
            }
            .expect("free var is not inference var");
            if self.var_unification_table.probe_var(iv).is_some() {
                return true;
            }
            let root = self.var_unification_table.inference_var_root(iv);
            iv != root
        })
    }

    fn try_resolve_obligation(
        &mut self,
        canonicalized: &Canonicalized<InEnvironment<Goal>>,
    ) -> bool {
        let solution = self.db.trait_solve(self.trait_env.krate, canonicalized.value.clone());

        match solution {
            Some(Solution::Unique(canonical_subst)) => {
                canonicalized.apply_solution(
                    self,
                    Canonical {
                        binders: canonical_subst.binders,
                        // FIXME: handle constraints
                        value: canonical_subst.value.subst,
                    },
                );
                true
            }
            Some(Solution::Ambig(Guidance::Definite(substs))) => {
                canonicalized.apply_solution(self, substs);
                false
            }
            Some(_) => {
                // FIXME use this when trying to resolve everything at the end
                false
            }
            None => {
                // FIXME obligation cannot be fulfilled => diagnostic
                true
            }
        }
    }
}

impl<'a> fmt::Debug for InferenceTable<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InferenceTable").field("num_vars", &self.type_variable_table.len()).finish()
    }
}

mod resolve {
    use super::InferenceTable;
    use crate::{
        ConcreteConst, Const, ConstData, ConstValue, DebruijnIndex, GenericArg, InferenceVar,
        Interner, Lifetime, Ty, TyVariableKind, VariableKind,
    };
    use chalk_ir::{
        cast::Cast,
        fold::{Fold, Folder},
        Fallible,
    };
    use hir_def::type_ref::ConstScalar;

    pub(super) struct Resolver<'a, 'b, F> {
        pub(super) table: &'a mut InferenceTable<'b>,
        pub(super) var_stack: &'a mut Vec<InferenceVar>,
        pub(super) fallback: F,
    }
    impl<'a, 'b, 'i, F> Folder<'i, Interner> for Resolver<'a, 'b, F>
    where
        F: Fn(InferenceVar, VariableKind, GenericArg, DebruijnIndex) -> GenericArg + 'i,
    {
        fn as_dyn(&mut self) -> &mut dyn Folder<'i, Interner> {
            self
        }

        fn interner(&self) -> &'i Interner {
            &Interner
        }

        fn fold_inference_ty(
            &mut self,
            var: InferenceVar,
            kind: TyVariableKind,
            outer_binder: DebruijnIndex,
        ) -> Fallible<Ty> {
            let var = self.table.var_unification_table.inference_var_root(var);
            if self.var_stack.contains(&var) {
                // recursive type
                let default = self.table.fallback_value(var, kind).cast(&Interner);
                return Ok((self.fallback)(var, VariableKind::Ty(kind), default, outer_binder)
                    .assert_ty_ref(&Interner)
                    .clone());
            }
            let result = if let Some(known_ty) = self.table.var_unification_table.probe_var(var) {
                // known_ty may contain other variables that are known by now
                self.var_stack.push(var);
                let result =
                    known_ty.fold_with(self, outer_binder).expect("fold failed unexpectedly");
                self.var_stack.pop();
                result.assert_ty_ref(&Interner).clone()
            } else {
                let default = self.table.fallback_value(var, kind).cast(&Interner);
                (self.fallback)(var, VariableKind::Ty(kind), default, outer_binder)
                    .assert_ty_ref(&Interner)
                    .clone()
            };
            Ok(result)
        }

        fn fold_inference_const(
            &mut self,
            ty: Ty,
            var: InferenceVar,
            outer_binder: DebruijnIndex,
        ) -> Fallible<Const> {
            let var = self.table.var_unification_table.inference_var_root(var);
            let default = ConstData {
                ty: ty.clone(),
                value: ConstValue::Concrete(ConcreteConst { interned: ConstScalar::Unknown }),
            }
            .intern(&Interner)
            .cast(&Interner);
            if self.var_stack.contains(&var) {
                // recursive
                return Ok((self.fallback)(var, VariableKind::Const(ty), default, outer_binder)
                    .assert_const_ref(&Interner)
                    .clone());
            }
            let result = if let Some(known_ty) = self.table.var_unification_table.probe_var(var) {
                // known_ty may contain other variables that are known by now
                self.var_stack.push(var);
                let result =
                    known_ty.fold_with(self, outer_binder).expect("fold failed unexpectedly");
                self.var_stack.pop();
                result.assert_const_ref(&Interner).clone()
            } else {
                (self.fallback)(var, VariableKind::Const(ty), default, outer_binder)
                    .assert_const_ref(&Interner)
                    .clone()
            };
            Ok(result)
        }

        fn fold_inference_lifetime(
            &mut self,
            _var: InferenceVar,
            _outer_binder: DebruijnIndex,
        ) -> Fallible<Lifetime> {
            // fall back all lifetimes to 'static -- currently we don't deal
            // with any lifetimes, but we can sometimes get some lifetime
            // variables through Chalk's unification, and this at least makes
            // sure we don't leak them outside of inference
            Ok(crate::static_lifetime())
        }
    }
}
