//! Various diagnostics for expressions that are collected together in one pass
//! through the body using inference results: mismatched arg counts, missing
//! fields, etc.

use std::{cell::RefCell, sync::Arc};

use hir_def::{
    expr::Statement, path::path, resolver::HasResolver, type_ref::Mutability, AssocItemId,
    DefWithBodyId, HasModule,
};
use hir_expand::name;
use itertools::Either;
use rustc_hash::FxHashSet;

use crate::{
    db::HirDatabase,
    diagnostics::match_check::{
        self,
        usefulness::{compute_match_usefulness, expand_pattern, MatchCheckCtx, PatternArena},
    },
    AdtId, InferenceResult, Interner, Ty, TyExt, TyKind,
};

pub(crate) use hir_def::{
    body::{Body, BodySourceMap},
    expr::{Expr, ExprId, MatchArm, Pat, PatId},
    LocalFieldId, VariantId,
};

pub enum BodyValidationDiagnostic {
    RecordMissingFields {
        record: Either<ExprId, PatId>,
        variant: VariantId,
        missed_fields: Vec<LocalFieldId>,
    },
    ReplaceFilterMapNextWithFindMap {
        method_call_expr: ExprId,
    },
    MismatchedArgCount {
        call_expr: ExprId,
        expected: usize,
        found: usize,
    },
    RemoveThisSemicolon {
        expr: ExprId,
    },
    MissingOkOrSomeInTailExpr {
        expr: ExprId,
        required: String,
    },
    MissingMatchArms {
        match_expr: ExprId,
    },
    AddReferenceHere {
        arg_expr: ExprId,
        mutability: Mutability,
    },
}

impl BodyValidationDiagnostic {
    pub fn collect(db: &dyn HirDatabase, owner: DefWithBodyId) -> Vec<BodyValidationDiagnostic> {
        let _p = profile::span("BodyValidationDiagnostic::collect");
        let infer = db.infer(owner);
        let mut validator = ExprValidator::new(owner, infer);
        validator.validate_body(db);
        validator.diagnostics
    }
}

struct ExprValidator {
    owner: DefWithBodyId,
    infer: Arc<InferenceResult>,
    pub(super) diagnostics: Vec<BodyValidationDiagnostic>,
}

impl ExprValidator {
    fn new(owner: DefWithBodyId, infer: Arc<InferenceResult>) -> ExprValidator {
        ExprValidator { owner, infer, diagnostics: Vec::new() }
    }

    fn validate_body(&mut self, db: &dyn HirDatabase) {
        self.check_for_filter_map_next(db);

        let body = db.body(self.owner);

        for (id, expr) in body.exprs.iter() {
            if let Some((variant, missed_fields, true)) =
                record_literal_missing_fields(db, &self.infer, id, expr)
            {
                self.diagnostics.push(BodyValidationDiagnostic::RecordMissingFields {
                    record: Either::Left(id),
                    variant,
                    missed_fields,
                });
            }

            match expr {
                Expr::Match { expr, arms } => {
                    self.validate_match(id, *expr, arms, db, self.infer.clone());
                }
                Expr::Call { .. } | Expr::MethodCall { .. } => {
                    self.validate_call(db, id, expr);
                }
                _ => {}
            }
        }
        for (id, pat) in body.pats.iter() {
            if let Some((variant, missed_fields, true)) =
                record_pattern_missing_fields(db, &self.infer, id, pat)
            {
                self.diagnostics.push(BodyValidationDiagnostic::RecordMissingFields {
                    record: Either::Right(id),
                    variant,
                    missed_fields,
                });
            }
        }
        let body_expr = &body[body.body_expr];
        if let Expr::Block { statements, tail, .. } = body_expr {
            if let Some(t) = tail {
                self.validate_results_in_tail_expr(body.body_expr, *t, db);
            } else if let Some(Statement::Expr { expr: id, .. }) = statements.last() {
                self.validate_missing_tail_expr(body.body_expr, *id);
            }
        }

        let infer = &self.infer;
        let diagnostics = &mut self.diagnostics;

        infer
            .expr_type_mismatches()
            .filter_map(|(expr, mismatch)| {
                let (expr_without_ref, mutability) =
                    check_missing_refs(infer, expr, &mismatch.expected)?;

                Some((expr_without_ref, mutability))
            })
            .for_each(|(arg_expr, mutability)| {
                diagnostics
                    .push(BodyValidationDiagnostic::AddReferenceHere { arg_expr, mutability });
            });
    }

    fn check_for_filter_map_next(&mut self, db: &dyn HirDatabase) {
        // Find the FunctionIds for Iterator::filter_map and Iterator::next
        let iterator_path = path![core::iter::Iterator];
        let resolver = self.owner.resolver(db.upcast());
        let iterator_trait_id = match resolver.resolve_known_trait(db.upcast(), &iterator_path) {
            Some(id) => id,
            None => return,
        };
        let iterator_trait_items = &db.trait_data(iterator_trait_id).items;
        let filter_map_function_id =
            match iterator_trait_items.iter().find(|item| item.0 == name![filter_map]) {
                Some((_, AssocItemId::FunctionId(id))) => id,
                _ => return,
            };
        let next_function_id = match iterator_trait_items.iter().find(|item| item.0 == name![next])
        {
            Some((_, AssocItemId::FunctionId(id))) => id,
            _ => return,
        };

        // Search function body for instances of .filter_map(..).next()
        let body = db.body(self.owner);
        let mut prev = None;
        for (id, expr) in body.exprs.iter() {
            if let Expr::MethodCall { receiver, .. } = expr {
                let function_id = match self.infer.method_resolution(id) {
                    Some((id, _)) => id,
                    None => continue,
                };

                if function_id == *filter_map_function_id {
                    prev = Some(id);
                    continue;
                }

                if function_id == *next_function_id {
                    if let Some(filter_map_id) = prev {
                        if *receiver == filter_map_id {
                            self.diagnostics.push(
                                BodyValidationDiagnostic::ReplaceFilterMapNextWithFindMap {
                                    method_call_expr: id,
                                },
                            );
                        }
                    }
                }
            }
            prev = None;
        }
    }

    fn validate_call(&mut self, db: &dyn HirDatabase, call_id: ExprId, expr: &Expr) {
        // Check that the number of arguments matches the number of parameters.

        // FIXME: Due to shortcomings in the current type system implementation, only emit this
        // diagnostic if there are no type mismatches in the containing function.
        if self.infer.expr_type_mismatches().next().is_some() {
            return;
        }

        let is_method_call = matches!(expr, Expr::MethodCall { .. });
        let (sig, args) = match expr {
            Expr::Call { callee, args } => {
                let callee = &self.infer.type_of_expr[*callee];
                let sig = match callee.callable_sig(db) {
                    Some(sig) => sig,
                    None => return,
                };
                (sig, args.clone())
            }
            Expr::MethodCall { receiver, args, .. } => {
                let mut args = args.clone();
                args.insert(0, *receiver);

                let receiver = &self.infer.type_of_expr[*receiver];
                if receiver.strip_references().is_unknown() {
                    // if the receiver is of unknown type, it's very likely we
                    // don't know enough to correctly resolve the method call.
                    // This is kind of a band-aid for #6975.
                    return;
                }

                let (callee, subst) = match self.infer.method_resolution(call_id) {
                    Some(it) => it,
                    None => return,
                };
                let sig = db.callable_item_signature(callee.into()).substitute(&Interner, &subst);

                (sig, args)
            }
            _ => return,
        };

        if sig.is_varargs {
            return;
        }

        let params = sig.params();

        let mut param_count = params.len();
        let mut arg_count = args.len();

        if arg_count != param_count {
            if is_method_call {
                param_count -= 1;
                arg_count -= 1;
            }
            self.diagnostics.push(BodyValidationDiagnostic::MismatchedArgCount {
                call_expr: call_id,
                expected: param_count,
                found: arg_count,
            });
        }
    }

    fn validate_match(
        &mut self,
        id: ExprId,
        match_expr: ExprId,
        arms: &[MatchArm],
        db: &dyn HirDatabase,
        infer: Arc<InferenceResult>,
    ) {
        let (body, source_map): (Arc<Body>, Arc<BodySourceMap>) =
            db.body_with_source_map(self.owner);

        let match_expr_ty = if infer.type_of_expr[match_expr].is_unknown() {
            return;
        } else {
            &infer.type_of_expr[match_expr]
        };

        let pattern_arena = RefCell::new(PatternArena::new());

        let mut m_arms = Vec::new();
        let mut has_lowering_errors = false;
        for arm in arms {
            if let Some(pat_ty) = infer.type_of_pat.get(arm.pat) {
                // We only include patterns whose type matches the type
                // of the match expression. If we had a InvalidMatchArmPattern
                // diagnostic or similar we could raise that in an else
                // block here.
                //
                // When comparing the types, we also have to consider that rustc
                // will automatically de-reference the match expression type if
                // necessary.
                //
                // FIXME we should use the type checker for this.
                if (pat_ty == match_expr_ty
                    || match_expr_ty
                        .as_reference()
                        .map(|(match_expr_ty, ..)| match_expr_ty == pat_ty)
                        .unwrap_or(false))
                    && types_of_subpatterns_do_match(arm.pat, &body, &infer)
                {
                    // If we had a NotUsefulMatchArm diagnostic, we could
                    // check the usefulness of each pattern as we added it
                    // to the matrix here.
                    let m_arm = match_check::MatchArm {
                        pat: self.lower_pattern(
                            arm.pat,
                            &mut pattern_arena.borrow_mut(),
                            db,
                            &body,
                            &mut has_lowering_errors,
                        ),
                        has_guard: arm.guard.is_some(),
                    };
                    m_arms.push(m_arm);
                    if !has_lowering_errors {
                        continue;
                    }
                }
            }

            // If we can't resolve the type of a pattern, or the pattern type doesn't
            // fit the match expression, we skip this diagnostic. Skipping the entire
            // diagnostic rather than just not including this match arm is preferred
            // to avoid the chance of false positives.
            cov_mark::hit!(validate_match_bailed_out);
            return;
        }

        let cx = MatchCheckCtx {
            module: self.owner.module(db.upcast()),
            match_expr,
            infer: &infer,
            db,
            pattern_arena: &pattern_arena,
            panic_context: &|| {
                use syntax::AstNode;
                let match_expr_text = source_map
                    .expr_syntax(match_expr)
                    .ok()
                    .and_then(|scrutinee_sptr| {
                        let root = scrutinee_sptr.file_syntax(db.upcast());
                        scrutinee_sptr.value.to_node(&root).syntax().parent()
                    })
                    .map(|node| node.to_string());
                format!(
                    "expression:\n{}",
                    match_expr_text.as_deref().unwrap_or("<synthesized expr>")
                )
            },
        };
        let report = compute_match_usefulness(&cx, &m_arms);

        // FIXME Report unreacheble arms
        // https://github.com/rust-lang/rust/blob/25c15cdbe/compiler/rustc_mir_build/src/thir/pattern/check_match.rs#L200-L201

        let witnesses = report.non_exhaustiveness_witnesses;
        // FIXME Report witnesses
        // eprintln!("compute_match_usefulness(..) -> {:?}", &witnesses);
        if !witnesses.is_empty() {
            self.diagnostics.push(BodyValidationDiagnostic::MissingMatchArms { match_expr: id });
        }
    }

    fn lower_pattern(
        &self,
        pat: PatId,
        pattern_arena: &mut PatternArena,
        db: &dyn HirDatabase,
        body: &Body,
        have_errors: &mut bool,
    ) -> match_check::PatId {
        let mut patcx = match_check::PatCtxt::new(db, &self.infer, body);
        let pattern = patcx.lower_pattern(pat);
        let pattern = pattern_arena.alloc(expand_pattern(pattern));
        if !patcx.errors.is_empty() {
            *have_errors = true;
        }
        pattern
    }

    fn validate_results_in_tail_expr(&mut self, body_id: ExprId, id: ExprId, db: &dyn HirDatabase) {
        // the mismatch will be on the whole block currently
        let mismatch = match self.infer.type_mismatch_for_expr(body_id) {
            Some(m) => m,
            None => return,
        };

        let core_result_path = path![core::result::Result];
        let core_option_path = path![core::option::Option];

        let resolver = self.owner.resolver(db.upcast());
        let core_result_enum = match resolver.resolve_known_enum(db.upcast(), &core_result_path) {
            Some(it) => it,
            _ => return,
        };
        let core_option_enum = match resolver.resolve_known_enum(db.upcast(), &core_option_path) {
            Some(it) => it,
            _ => return,
        };

        let (params, required) = match mismatch.expected.kind(&Interner) {
            TyKind::Adt(AdtId(hir_def::AdtId::EnumId(enum_id)), parameters)
                if *enum_id == core_result_enum =>
            {
                (parameters, "Ok".to_string())
            }
            TyKind::Adt(AdtId(hir_def::AdtId::EnumId(enum_id)), parameters)
                if *enum_id == core_option_enum =>
            {
                (parameters, "Some".to_string())
            }
            _ => return,
        };

        if params.len(&Interner) > 0
            && params.at(&Interner, 0).ty(&Interner) == Some(&mismatch.actual)
        {
            self.diagnostics
                .push(BodyValidationDiagnostic::MissingOkOrSomeInTailExpr { expr: id, required });
        }
    }

    fn validate_missing_tail_expr(&mut self, body_id: ExprId, possible_tail_id: ExprId) {
        let mismatch = match self.infer.type_mismatch_for_expr(body_id) {
            Some(m) => m,
            None => return,
        };

        let possible_tail_ty = match self.infer.type_of_expr.get(possible_tail_id) {
            Some(ty) => ty,
            None => return,
        };

        if !mismatch.actual.is_unit() || mismatch.expected != *possible_tail_ty {
            return;
        }

        self.diagnostics
            .push(BodyValidationDiagnostic::RemoveThisSemicolon { expr: possible_tail_id });
    }
}

pub fn record_literal_missing_fields(
    db: &dyn HirDatabase,
    infer: &InferenceResult,
    id: ExprId,
    expr: &Expr,
) -> Option<(VariantId, Vec<LocalFieldId>, /*exhaustive*/ bool)> {
    let (fields, exhaustive) = match expr {
        Expr::RecordLit { path: _, fields, spread } => (fields, spread.is_none()),
        _ => return None,
    };

    let variant_def = infer.variant_resolution_for_expr(id)?;
    if let VariantId::UnionId(_) = variant_def {
        return None;
    }

    let variant_data = variant_def.variant_data(db.upcast());

    let specified_fields: FxHashSet<_> = fields.iter().map(|f| &f.name).collect();
    let missed_fields: Vec<LocalFieldId> = variant_data
        .fields()
        .iter()
        .filter_map(|(f, d)| if specified_fields.contains(&d.name) { None } else { Some(f) })
        .collect();
    if missed_fields.is_empty() {
        return None;
    }
    Some((variant_def, missed_fields, exhaustive))
}

pub fn record_pattern_missing_fields(
    db: &dyn HirDatabase,
    infer: &InferenceResult,
    id: PatId,
    pat: &Pat,
) -> Option<(VariantId, Vec<LocalFieldId>, /*exhaustive*/ bool)> {
    let (fields, exhaustive) = match pat {
        Pat::Record { path: _, args, ellipsis } => (args, !ellipsis),
        _ => return None,
    };

    let variant_def = infer.variant_resolution_for_pat(id)?;
    if let VariantId::UnionId(_) = variant_def {
        return None;
    }

    let variant_data = variant_def.variant_data(db.upcast());

    let specified_fields: FxHashSet<_> = fields.iter().map(|f| &f.name).collect();
    let missed_fields: Vec<LocalFieldId> = variant_data
        .fields()
        .iter()
        .filter_map(|(f, d)| if specified_fields.contains(&d.name) { None } else { Some(f) })
        .collect();
    if missed_fields.is_empty() {
        return None;
    }
    Some((variant_def, missed_fields, exhaustive))
}

fn types_of_subpatterns_do_match(pat: PatId, body: &Body, infer: &InferenceResult) -> bool {
    fn walk(pat: PatId, body: &Body, infer: &InferenceResult, has_type_mismatches: &mut bool) {
        match infer.type_mismatch_for_pat(pat) {
            Some(_) => *has_type_mismatches = true,
            None => {
                body[pat].walk_child_pats(|subpat| walk(subpat, body, infer, has_type_mismatches))
            }
        }
    }

    let mut has_type_mismatches = false;
    walk(pat, body, infer, &mut has_type_mismatches);
    !has_type_mismatches
}

fn check_missing_refs(
    infer: &InferenceResult,
    arg: ExprId,
    param: &Ty,
) -> Option<(ExprId, Mutability)> {
    let arg_ty = infer.type_of_expr.get(arg)?;

    let reference_one = arg_ty.as_reference();
    let reference_two = param.as_reference();

    match (reference_one, reference_two) {
        (None, Some((referenced_ty, _, mutability))) if referenced_ty == arg_ty => {
            Some((arg, Mutability::from_mutable(matches!(mutability, chalk_ir::Mutability::Mut))))
        }
        (None, Some((referenced_ty, _, mutability))) => match referenced_ty.kind(&Interner) {
            TyKind::Slice(subst) if matches!(arg_ty.kind(&Interner), TyKind::Array(arr_subst, _) if arr_subst == subst) => {
                Some((
                    arg,
                    Mutability::from_mutable(matches!(mutability, chalk_ir::Mutability::Mut)),
                ))
            }
            _ => None,
        },
        _ => None,
    }
}
