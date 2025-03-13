//! Code shared by trait and projection goals for candidate assembly.

pub(super) mod structural_traits;

use derive_where::derive_where;
use rustc_type_ir::inherent::*;
use rustc_type_ir::lang_items::TraitSolverLangItem;
use rustc_type_ir::visit::TypeVisitableExt as _;
use rustc_type_ir::{self as ty, Interner, TypeFoldable, TypingMode, Upcast as _, elaborate};
use tracing::{debug, instrument};

use super::trait_goals::TraitGoalProvenVia;
use crate::delegate::SolverDelegate;
use crate::solve::inspect::ProbeKind;
use crate::solve::{
    BuiltinImplSource, CandidateSource, CanonicalResponse, Certainty, EvalCtxt, Goal, GoalSource,
    MaybeCause, NoSolution, QueryResult,
};

enum AliasBoundKind {
    SelfBounds,
    NonSelfBounds,
}

/// A candidate is a possible way to prove a goal.
///
/// It consists of both the `source`, which describes how that goal would be proven,
/// and the `result` when using the given `source`.
#[derive_where(Clone, Debug; I: Interner)]
pub(super) struct Candidate<I: Interner> {
    pub(super) source: CandidateSource<I>,
    pub(super) result: CanonicalResponse<I>,
}

/// Methods used to assemble candidates for either trait or projection goals.
pub(super) trait GoalKind<D, I = <D as SolverDelegate>::Interner>:
    TypeFoldable<I> + Copy + Eq + std::fmt::Display
where
    D: SolverDelegate<Interner = I>,
    I: Interner,
{
    fn self_ty(self) -> I::Ty;

    fn trait_ref(self, cx: I) -> ty::TraitRef<I>;

    fn with_self_ty(self, cx: I, self_ty: I::Ty) -> Self;

    fn trait_def_id(self, cx: I) -> I::DefId;

    /// Try equating an assumption predicate against a goal's predicate. If it
    /// holds, then execute the `then` callback, which should do any additional
    /// work, then produce a response (typically by executing
    /// [`EvalCtxt::evaluate_added_goals_and_make_canonical_response`]).
    fn probe_and_match_goal_against_assumption(
        ecx: &mut EvalCtxt<'_, D>,
        source: CandidateSource<I>,
        goal: Goal<I, Self>,
        assumption: I::Clause,
        then: impl FnOnce(&mut EvalCtxt<'_, D>) -> QueryResult<I>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// Consider a clause, which consists of a "assumption" and some "requirements",
    /// to satisfy a goal. If the requirements hold, then attempt to satisfy our
    /// goal by equating it with the assumption.
    fn probe_and_consider_implied_clause(
        ecx: &mut EvalCtxt<'_, D>,
        parent_source: CandidateSource<I>,
        goal: Goal<I, Self>,
        assumption: I::Clause,
        requirements: impl IntoIterator<Item = (GoalSource, Goal<I, I::Predicate>)>,
    ) -> Result<Candidate<I>, NoSolution> {
        Self::probe_and_match_goal_against_assumption(ecx, parent_source, goal, assumption, |ecx| {
            for (nested_source, goal) in requirements {
                ecx.add_goal(nested_source, goal);
            }
            ecx.evaluate_added_goals_and_make_canonical_response(Certainty::Yes)
        })
    }

    /// Consider a clause specifically for a `dyn Trait` self type. This requires
    /// additionally checking all of the supertraits and object bounds to hold,
    /// since they're not implied by the well-formedness of the object type.
    fn probe_and_consider_object_bound_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        source: CandidateSource<I>,
        goal: Goal<I, Self>,
        assumption: I::Clause,
    ) -> Result<Candidate<I>, NoSolution> {
        Self::probe_and_match_goal_against_assumption(ecx, source, goal, assumption, |ecx| {
            let cx = ecx.cx();
            let ty::Dynamic(bounds, _, _) = goal.predicate.self_ty().kind() else {
                panic!("expected object type in `probe_and_consider_object_bound_candidate`");
            };
            ecx.add_goals(
                GoalSource::ImplWhereBound,
                structural_traits::predicates_for_object_candidate(
                    ecx,
                    goal.param_env,
                    goal.predicate.trait_ref(cx),
                    bounds,
                ),
            );
            ecx.evaluate_added_goals_and_make_canonical_response(Certainty::Yes)
        })
    }

    /// Assemble additional assumptions for an alias that are not included
    /// in the item bounds of the alias. For now, this is limited to the
    /// `explicit_implied_const_bounds` for an associated type.
    fn consider_additional_alias_assumptions(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
        alias_ty: ty::AliasTy<I>,
    ) -> Vec<Candidate<I>>;

    fn consider_impl_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
        impl_def_id: I::DefId,
    ) -> Result<Candidate<I>, NoSolution>;

    /// If the predicate contained an error, we want to avoid emitting unnecessary trait
    /// errors but still want to emit errors for other trait goals. We have some special
    /// handling for this case.
    ///
    /// Trait goals always hold while projection goals never do. This is a bit arbitrary
    /// but prevents incorrect normalization while hiding any trait errors.
    fn consider_error_guaranteed_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        guar: I::ErrorGuaranteed,
    ) -> Result<Candidate<I>, NoSolution>;

    /// A type implements an `auto trait` if its components do as well.
    ///
    /// These components are given by built-in rules from
    /// [`structural_traits::instantiate_constituent_tys_for_auto_trait`].
    fn consider_auto_trait_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// A trait alias holds if the RHS traits and `where` clauses hold.
    fn consider_trait_alias_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// A type is `Sized` if its tail component is `Sized`.
    ///
    /// These components are given by built-in rules from
    /// [`structural_traits::instantiate_constituent_tys_for_sized_trait`].
    fn consider_builtin_sized_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// A type is `Copy` or `Clone` if its components are `Copy` or `Clone`.
    ///
    /// These components are given by built-in rules from
    /// [`structural_traits::instantiate_constituent_tys_for_copy_clone_trait`].
    fn consider_builtin_copy_clone_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// A type is a `FnPtr` if it is of `FnPtr` type.
    fn consider_builtin_fn_ptr_trait_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// A callable type (a closure, fn def, or fn ptr) is known to implement the `Fn<A>`
    /// family of traits where `A` is given by the signature of the type.
    fn consider_builtin_fn_trait_candidates(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
        kind: ty::ClosureKind,
    ) -> Result<Candidate<I>, NoSolution>;

    /// An async closure is known to implement the `AsyncFn<A>` family of traits
    /// where `A` is given by the signature of the type.
    fn consider_builtin_async_fn_trait_candidates(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
        kind: ty::ClosureKind,
    ) -> Result<Candidate<I>, NoSolution>;

    /// Compute the built-in logic of the `AsyncFnKindHelper` helper trait, which
    /// is used internally to delay computation for async closures until after
    /// upvar analysis is performed in HIR typeck.
    fn consider_builtin_async_fn_kind_helper_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// `Tuple` is implemented if the `Self` type is a tuple.
    fn consider_builtin_tuple_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// `Pointee` is always implemented.
    ///
    /// See the projection implementation for the `Metadata` types for all of
    /// the built-in types. For structs, the metadata type is given by the struct
    /// tail.
    fn consider_builtin_pointee_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// A coroutine (that comes from an `async` desugaring) is known to implement
    /// `Future<Output = O>`, where `O` is given by the coroutine's return type
    /// that was computed during type-checking.
    fn consider_builtin_future_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// A coroutine (that comes from a `gen` desugaring) is known to implement
    /// `Iterator<Item = O>`, where `O` is given by the generator's yield type
    /// that was computed during type-checking.
    fn consider_builtin_iterator_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// A coroutine (that comes from a `gen` desugaring) is known to implement
    /// `FusedIterator`
    fn consider_builtin_fused_iterator_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    fn consider_builtin_async_iterator_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// A coroutine (that doesn't come from an `async` or `gen` desugaring) is known to
    /// implement `Coroutine<R, Yield = Y, Return = O>`, given the resume, yield,
    /// and return types of the coroutine computed during type-checking.
    fn consider_builtin_coroutine_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    fn consider_builtin_discriminant_kind_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    fn consider_builtin_async_destruct_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    fn consider_builtin_destruct_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    fn consider_builtin_transmute_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    fn consider_builtin_bikeshed_guaranteed_no_drop_candidate(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Result<Candidate<I>, NoSolution>;

    /// Consider (possibly several) candidates to upcast or unsize a type to another
    /// type, excluding the coercion of a sized type into a `dyn Trait`.
    ///
    /// We return the `BuiltinImplSource` for each candidate as it is needed
    /// for unsize coercion in hir typeck and because it is difficult to
    /// otherwise recompute this for codegen. This is a bit of a mess but the
    /// easiest way to maintain the existing behavior for now.
    fn consider_structural_builtin_unsize_candidates(
        ecx: &mut EvalCtxt<'_, D>,
        goal: Goal<I, Self>,
    ) -> Vec<Candidate<I>>;
}

impl<D, I> EvalCtxt<'_, D>
where
    D: SolverDelegate<Interner = I>,
    I: Interner,
{
    pub(super) fn assemble_and_evaluate_candidates<G: GoalKind<D>>(
        &mut self,
        goal: Goal<I, G>,
    ) -> Vec<Candidate<I>> {
        let Ok(normalized_self_ty) =
            self.structurally_normalize_ty(goal.param_env, goal.predicate.self_ty())
        else {
            return vec![];
        };

        if normalized_self_ty.is_ty_var() {
            debug!("self type has been normalized to infer");
            return self.forced_ambiguity(MaybeCause::Ambiguity).into_iter().collect();
        }

        let goal: Goal<I, G> =
            goal.with(self.cx(), goal.predicate.with_self_ty(self.cx(), normalized_self_ty));
        // Vars that show up in the rest of the goal substs may have been constrained by
        // normalizing the self type as well, since type variables are not uniquified.
        let goal = self.resolve_vars_if_possible(goal);

        let mut candidates = vec![];

        if let TypingMode::Coherence = self.typing_mode() {
            if let Ok(candidate) = self.consider_coherence_unknowable_candidate(goal) {
                return vec![candidate];
            }
        }

        self.assemble_impl_candidates(goal, &mut candidates);

        self.assemble_builtin_impl_candidates(goal, &mut candidates);

        self.assemble_alias_bound_candidates(goal, &mut candidates);

        self.assemble_object_bound_candidates(goal, &mut candidates);

        self.assemble_param_env_candidates(goal, &mut candidates);

        candidates
    }

    pub(super) fn forced_ambiguity(
        &mut self,
        cause: MaybeCause,
    ) -> Result<Candidate<I>, NoSolution> {
        // This may fail if `try_evaluate_added_goals` overflows because it
        // fails to reach a fixpoint but ends up getting an error after
        // running for some additional step.
        //
        // cc trait-system-refactor-initiative#105
        let source = CandidateSource::BuiltinImpl(BuiltinImplSource::Misc);
        let certainty = Certainty::Maybe(cause);
        self.probe_trait_candidate(source)
            .enter(|this| this.evaluate_added_goals_and_make_canonical_response(certainty))
    }

    #[instrument(level = "trace", skip_all)]
    fn assemble_impl_candidates<G: GoalKind<D>>(
        &mut self,
        goal: Goal<I, G>,
        candidates: &mut Vec<Candidate<I>>,
    ) {
        let cx = self.cx();
        cx.for_each_relevant_impl(
            goal.predicate.trait_def_id(cx),
            goal.predicate.self_ty(),
            |impl_def_id| {
                // For every `default impl`, there's always a non-default `impl`
                // that will *also* apply. There's no reason to register a candidate
                // for this impl, since it is *not* proof that the trait goal holds.
                if cx.impl_is_default(impl_def_id) {
                    return;
                }

                match G::consider_impl_candidate(self, goal, impl_def_id) {
                    Ok(candidate) => candidates.push(candidate),
                    Err(NoSolution) => (),
                }
            },
        );
    }

    #[instrument(level = "trace", skip_all)]
    fn assemble_builtin_impl_candidates<G: GoalKind<D>>(
        &mut self,
        goal: Goal<I, G>,
        candidates: &mut Vec<Candidate<I>>,
    ) {
        let cx = self.cx();
        let trait_def_id = goal.predicate.trait_def_id(cx);

        // N.B. When assembling built-in candidates for lang items that are also
        // `auto` traits, then the auto trait candidate that is assembled in
        // `consider_auto_trait_candidate` MUST be disqualified to remain sound.
        //
        // Instead of adding the logic here, it's a better idea to add it in
        // `EvalCtxt::disqualify_auto_trait_candidate_due_to_possible_impl` in
        // `solve::trait_goals` instead.
        let result = if let Err(guar) = goal.predicate.error_reported() {
            G::consider_error_guaranteed_candidate(self, guar)
        } else if cx.trait_is_auto(trait_def_id) {
            G::consider_auto_trait_candidate(self, goal)
        } else if cx.trait_is_alias(trait_def_id) {
            G::consider_trait_alias_candidate(self, goal)
        } else {
            match cx.as_lang_item(trait_def_id) {
                Some(TraitSolverLangItem::Sized) => G::consider_builtin_sized_candidate(self, goal),
                Some(TraitSolverLangItem::Copy | TraitSolverLangItem::Clone) => {
                    G::consider_builtin_copy_clone_candidate(self, goal)
                }
                Some(TraitSolverLangItem::Fn) => {
                    G::consider_builtin_fn_trait_candidates(self, goal, ty::ClosureKind::Fn)
                }
                Some(TraitSolverLangItem::FnMut) => {
                    G::consider_builtin_fn_trait_candidates(self, goal, ty::ClosureKind::FnMut)
                }
                Some(TraitSolverLangItem::FnOnce) => {
                    G::consider_builtin_fn_trait_candidates(self, goal, ty::ClosureKind::FnOnce)
                }
                Some(TraitSolverLangItem::AsyncFn) => {
                    G::consider_builtin_async_fn_trait_candidates(self, goal, ty::ClosureKind::Fn)
                }
                Some(TraitSolverLangItem::AsyncFnMut) => {
                    G::consider_builtin_async_fn_trait_candidates(
                        self,
                        goal,
                        ty::ClosureKind::FnMut,
                    )
                }
                Some(TraitSolverLangItem::AsyncFnOnce) => {
                    G::consider_builtin_async_fn_trait_candidates(
                        self,
                        goal,
                        ty::ClosureKind::FnOnce,
                    )
                }
                Some(TraitSolverLangItem::FnPtrTrait) => {
                    G::consider_builtin_fn_ptr_trait_candidate(self, goal)
                }
                Some(TraitSolverLangItem::AsyncFnKindHelper) => {
                    G::consider_builtin_async_fn_kind_helper_candidate(self, goal)
                }
                Some(TraitSolverLangItem::Tuple) => G::consider_builtin_tuple_candidate(self, goal),
                Some(TraitSolverLangItem::PointeeTrait) => {
                    G::consider_builtin_pointee_candidate(self, goal)
                }
                Some(TraitSolverLangItem::Future) => {
                    G::consider_builtin_future_candidate(self, goal)
                }
                Some(TraitSolverLangItem::Iterator) => {
                    G::consider_builtin_iterator_candidate(self, goal)
                }
                Some(TraitSolverLangItem::FusedIterator) => {
                    G::consider_builtin_fused_iterator_candidate(self, goal)
                }
                Some(TraitSolverLangItem::AsyncIterator) => {
                    G::consider_builtin_async_iterator_candidate(self, goal)
                }
                Some(TraitSolverLangItem::Coroutine) => {
                    G::consider_builtin_coroutine_candidate(self, goal)
                }
                Some(TraitSolverLangItem::DiscriminantKind) => {
                    G::consider_builtin_discriminant_kind_candidate(self, goal)
                }
                Some(TraitSolverLangItem::AsyncDestruct) => {
                    G::consider_builtin_async_destruct_candidate(self, goal)
                }
                Some(TraitSolverLangItem::Destruct) => {
                    G::consider_builtin_destruct_candidate(self, goal)
                }
                Some(TraitSolverLangItem::TransmuteTrait) => {
                    G::consider_builtin_transmute_candidate(self, goal)
                }
                Some(TraitSolverLangItem::BikeshedGuaranteedNoDrop) => {
                    G::consider_builtin_bikeshed_guaranteed_no_drop_candidate(self, goal)
                }
                _ => Err(NoSolution),
            }
        };

        candidates.extend(result);

        // There may be multiple unsize candidates for a trait with several supertraits:
        // `trait Foo: Bar<A> + Bar<B>` and `dyn Foo: Unsize<dyn Bar<_>>`
        if cx.is_lang_item(trait_def_id, TraitSolverLangItem::Unsize) {
            candidates.extend(G::consider_structural_builtin_unsize_candidates(self, goal));
        }
    }

    #[instrument(level = "trace", skip_all)]
    fn assemble_param_env_candidates<G: GoalKind<D>>(
        &mut self,
        goal: Goal<I, G>,
        candidates: &mut Vec<Candidate<I>>,
    ) {
        for (i, assumption) in goal.param_env.caller_bounds().iter().enumerate() {
            candidates.extend(G::probe_and_consider_implied_clause(
                self,
                CandidateSource::ParamEnv(i),
                goal,
                assumption,
                [],
            ));
        }
    }

    #[instrument(level = "trace", skip_all)]
    fn assemble_alias_bound_candidates<G: GoalKind<D>>(
        &mut self,
        goal: Goal<I, G>,
        candidates: &mut Vec<Candidate<I>>,
    ) {
        let () = self.probe(|_| ProbeKind::NormalizedSelfTyAssembly).enter(|ecx| {
            ecx.assemble_alias_bound_candidates_recur(
                goal.predicate.self_ty(),
                goal,
                candidates,
                AliasBoundKind::SelfBounds,
            );
        });
    }

    /// For some deeply nested `<T>::A::B::C::D` rigid associated type,
    /// we should explore the item bounds for all levels, since the
    /// `associated_type_bounds` feature means that a parent associated
    /// type may carry bounds for a nested associated type.
    ///
    /// If we have a projection, check that its self type is a rigid projection.
    /// If so, continue searching by recursively calling after normalization.
    // FIXME: This may recurse infinitely, but I can't seem to trigger it without
    // hitting another overflow error something. Add a depth parameter needed later.
    fn assemble_alias_bound_candidates_recur<G: GoalKind<D>>(
        &mut self,
        self_ty: I::Ty,
        goal: Goal<I, G>,
        candidates: &mut Vec<Candidate<I>>,
        consider_self_bounds: AliasBoundKind,
    ) {
        let (kind, alias_ty) = match self_ty.kind() {
            ty::Bool
            | ty::Char
            | ty::Int(_)
            | ty::Uint(_)
            | ty::Float(_)
            | ty::Adt(_, _)
            | ty::Foreign(_)
            | ty::Str
            | ty::Array(_, _)
            | ty::Pat(_, _)
            | ty::Slice(_)
            | ty::RawPtr(_, _)
            | ty::Ref(_, _, _)
            | ty::FnDef(_, _)
            | ty::FnPtr(..)
            | ty::UnsafeBinder(_)
            | ty::Dynamic(..)
            | ty::Closure(..)
            | ty::CoroutineClosure(..)
            | ty::Coroutine(..)
            | ty::CoroutineWitness(..)
            | ty::Never
            | ty::Tuple(_)
            | ty::Param(_)
            | ty::Placeholder(..)
            | ty::Infer(ty::IntVar(_) | ty::FloatVar(_))
            | ty::Error(_) => return,
            ty::Infer(ty::FreshTy(_) | ty::FreshIntTy(_) | ty::FreshFloatTy(_)) | ty::Bound(..) => {
                panic!("unexpected self type for `{goal:?}`")
            }

            ty::Infer(ty::TyVar(_)) => {
                // If we hit infer when normalizing the self type of an alias,
                // then bail with ambiguity. We should never encounter this on
                // the *first* iteration of this recursive function.
                if let Ok(result) =
                    self.evaluate_added_goals_and_make_canonical_response(Certainty::AMBIGUOUS)
                {
                    candidates.push(Candidate { source: CandidateSource::AliasBound, result });
                }
                return;
            }

            ty::Alias(kind @ (ty::Projection | ty::Opaque), alias_ty) => (kind, alias_ty),
            ty::Alias(ty::Inherent | ty::Weak, _) => {
                self.cx().delay_bug(format!("could not normalize {self_ty:?}, it is not WF"));
                return;
            }
        };

        match consider_self_bounds {
            AliasBoundKind::SelfBounds => {
                for assumption in self
                    .cx()
                    .item_self_bounds(alias_ty.def_id)
                    .iter_instantiated(self.cx(), alias_ty.args)
                {
                    candidates.extend(G::probe_and_consider_implied_clause(
                        self,
                        CandidateSource::AliasBound,
                        goal,
                        assumption,
                        [],
                    ));
                }
            }
            AliasBoundKind::NonSelfBounds => {
                for assumption in self
                    .cx()
                    .item_non_self_bounds(alias_ty.def_id)
                    .iter_instantiated(self.cx(), alias_ty.args)
                {
                    candidates.extend(G::probe_and_consider_implied_clause(
                        self,
                        CandidateSource::AliasBound,
                        goal,
                        assumption,
                        [],
                    ));
                }
            }
        }

        candidates.extend(G::consider_additional_alias_assumptions(self, goal, alias_ty));

        if kind != ty::Projection {
            return;
        }

        // Recurse on the self type of the projection.
        match self.structurally_normalize_ty(goal.param_env, alias_ty.self_ty()) {
            Ok(next_self_ty) => self.assemble_alias_bound_candidates_recur(
                next_self_ty,
                goal,
                candidates,
                AliasBoundKind::NonSelfBounds,
            ),
            Err(NoSolution) => {}
        }
    }

    #[instrument(level = "trace", skip_all)]
    fn assemble_object_bound_candidates<G: GoalKind<D>>(
        &mut self,
        goal: Goal<I, G>,
        candidates: &mut Vec<Candidate<I>>,
    ) {
        let cx = self.cx();
        if !cx.trait_may_be_implemented_via_object(goal.predicate.trait_def_id(cx)) {
            return;
        }

        let self_ty = goal.predicate.self_ty();
        let bounds = match self_ty.kind() {
            ty::Bool
            | ty::Char
            | ty::Int(_)
            | ty::Uint(_)
            | ty::Float(_)
            | ty::Adt(_, _)
            | ty::Foreign(_)
            | ty::Str
            | ty::Array(_, _)
            | ty::Pat(_, _)
            | ty::Slice(_)
            | ty::RawPtr(_, _)
            | ty::Ref(_, _, _)
            | ty::FnDef(_, _)
            | ty::FnPtr(..)
            | ty::UnsafeBinder(_)
            | ty::Alias(..)
            | ty::Closure(..)
            | ty::CoroutineClosure(..)
            | ty::Coroutine(..)
            | ty::CoroutineWitness(..)
            | ty::Never
            | ty::Tuple(_)
            | ty::Param(_)
            | ty::Placeholder(..)
            | ty::Infer(ty::IntVar(_) | ty::FloatVar(_))
            | ty::Error(_) => return,
            ty::Infer(ty::TyVar(_) | ty::FreshTy(_) | ty::FreshIntTy(_) | ty::FreshFloatTy(_))
            | ty::Bound(..) => panic!("unexpected self type for `{goal:?}`"),
            ty::Dynamic(bounds, ..) => bounds,
        };

        // Do not consider built-in object impls for dyn-incompatible types.
        if bounds.principal_def_id().is_some_and(|def_id| !cx.trait_is_dyn_compatible(def_id)) {
            return;
        }

        // Consider all of the auto-trait and projection bounds, which don't
        // need to be recorded as a `BuiltinImplSource::Object` since they don't
        // really have a vtable base...
        for bound in bounds.iter() {
            match bound.skip_binder() {
                ty::ExistentialPredicate::Trait(_) => {
                    // Skip principal
                }
                ty::ExistentialPredicate::Projection(_)
                | ty::ExistentialPredicate::AutoTrait(_) => {
                    candidates.extend(G::probe_and_consider_object_bound_candidate(
                        self,
                        CandidateSource::BuiltinImpl(BuiltinImplSource::Misc),
                        goal,
                        bound.with_self_ty(cx, self_ty),
                    ));
                }
            }
        }

        // FIXME: We only need to do *any* of this if we're considering a trait goal,
        // since we don't need to look at any supertrait or anything if we are doing
        // a projection goal.
        if let Some(principal) = bounds.principal() {
            let principal_trait_ref = principal.with_self_ty(cx, self_ty);
            for (idx, assumption) in elaborate::supertraits(cx, principal_trait_ref).enumerate() {
                candidates.extend(G::probe_and_consider_object_bound_candidate(
                    self,
                    CandidateSource::BuiltinImpl(BuiltinImplSource::Object(idx)),
                    goal,
                    assumption.upcast(cx),
                ));
            }
        }
    }

    /// In coherence we have to not only care about all impls we know about, but
    /// also consider impls which may get added in a downstream or sibling crate
    /// or which an upstream impl may add in a minor release.
    ///
    /// To do so we return a single ambiguous candidate in case such an unknown
    /// impl could apply to the current goal.
    #[instrument(level = "trace", skip_all)]
    fn consider_coherence_unknowable_candidate<G: GoalKind<D>>(
        &mut self,
        goal: Goal<I, G>,
    ) -> Result<Candidate<I>, NoSolution> {
        self.probe_trait_candidate(CandidateSource::CoherenceUnknowable).enter(|ecx| {
            let cx = ecx.cx();
            let trait_ref = goal.predicate.trait_ref(cx);
            if ecx.trait_ref_is_knowable(goal.param_env, trait_ref)? {
                Err(NoSolution)
            } else {
                // While the trait bound itself may be unknowable, we may be able to
                // prove that a super trait is not implemented. For this, we recursively
                // prove the super trait bounds of the current goal.
                //
                // We skip the goal itself as that one would cycle.
                let predicate: I::Predicate = trait_ref.upcast(cx);
                ecx.add_goals(
                    GoalSource::Misc,
                    elaborate::elaborate(cx, [predicate])
                        .skip(1)
                        .map(|predicate| goal.with(cx, predicate)),
                );
                ecx.evaluate_added_goals_and_make_canonical_response(Certainty::AMBIGUOUS)
            }
        })
    }

    /// We sadly can't simply take all possible candidates for normalization goals
    /// and check whether they result in the same constraints. We want to make sure
    /// that trying to normalize an alias doesn't result in constraints which aren't
    /// otherwise required.
    ///
    /// Most notably, when proving a trait goal by via a where-bound, we should not
    /// normalize via impls which have stricter region constraints than the where-bound:
    ///
    /// ```rust
    /// trait Trait<'a> {
    ///     type Assoc;
    /// }
    ///
    /// impl<'a, T: 'a> Trait<'a> for T {
    ///     type Assoc = u32;
    /// }
    ///
    /// fn with_bound<'a, T: Trait<'a>>(_value: T::Assoc) {}
    /// ```
    ///
    /// The where-bound of `with_bound` doesn't specify the associated type, so we would
    /// only be able to normalize `<T as Trait<'a>>::Assoc` by using the impl. This impl
    /// adds a `T: 'a` bound however, which would result in a region error. Given that the
    /// user explicitly wrote that `T: Trait<'a>` holds, this is undesirable and we instead
    /// treat the alias as rigid.
    ///
    /// See trait-system-refactor-initiative#124 for more details.
    #[instrument(level = "debug", skip(self, inject_normalize_to_rigid_candidate), ret)]
    pub(super) fn merge_candidates(
        &mut self,
        proven_via: Option<TraitGoalProvenVia>,
        candidates: Vec<Candidate<I>>,
        inject_normalize_to_rigid_candidate: impl FnOnce(&mut EvalCtxt<'_, D>) -> QueryResult<I>,
    ) -> QueryResult<I> {
        let Some(proven_via) = proven_via else {
            // We don't care about overflow. If proving the trait goal overflowed, then
            // it's enough to report an overflow error for that, we don't also have to
            // overflow during normalization.
            return Ok(self.make_ambiguous_response_no_constraints(MaybeCause::Ambiguity));
        };

        match proven_via {
            // Even when a trait bound has been proven using a where-bound, we
            // still need to consider alias-bounds for normalization, see
            // tests/ui/next-solver/alias-bound-shadowed-by-env.rs.
            //
            // FIXME(const_trait_impl): should this behavior also be used by
            // constness checking. Doing so is *at least theoretically* breaking,
            // see github.com/rust-lang/rust/issues/133044#issuecomment-2500709754
            TraitGoalProvenVia::ParamEnv | TraitGoalProvenVia::AliasBound => {
                let mut candidates_from_env_and_bounds: Vec<_> = candidates
                    .iter()
                    .filter(|c| {
                        matches!(
                            c.source,
                            CandidateSource::AliasBound | CandidateSource::ParamEnv(_)
                        )
                    })
                    .map(|c| c.result)
                    .collect();

                // If the trait goal has been proven by using the environment, we want to treat
                // aliases as rigid if there are no applicable projection bounds in the environment.
                if candidates_from_env_and_bounds.is_empty() {
                    if let Ok(response) = inject_normalize_to_rigid_candidate(self) {
                        candidates_from_env_and_bounds.push(response);
                    }
                }

                if let Some(response) = self.try_merge_responses(&candidates_from_env_and_bounds) {
                    Ok(response)
                } else {
                    self.flounder(&candidates_from_env_and_bounds)
                }
            }
            TraitGoalProvenVia::Misc => {
                // Prefer "orphaned" param-env normalization predicates, which are used
                // (for example, and ideally only) when proving item bounds for an impl.
                let candidates_from_env: Vec<_> = candidates
                    .iter()
                    .filter(|c| matches!(c.source, CandidateSource::ParamEnv(_)))
                    .map(|c| c.result)
                    .collect();
                if let Some(response) = self.try_merge_responses(&candidates_from_env) {
                    return Ok(response);
                }

                let responses: Vec<_> = candidates.iter().map(|c| c.result).collect();
                if let Some(response) = self.try_merge_responses(&responses) {
                    Ok(response)
                } else {
                    self.flounder(&responses)
                }
            }
        }
    }
}
