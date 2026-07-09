#![allow(dead_code)]

//! Pure frontier promotion policy.
//!
//! This module classifies follow-up proposals only. It does not apply proposals,
//! mutate queues, touch state, talk to workers, inspect git, read files, or ask
//! the clock. Runtime wiring is intentionally deferred to later AF slices.

pub(crate) mod promotion_policy {
    use crate::domain::{
        AutonomyLevel, FollowupSliceDraft, FrontierBudgetState, MissionEnvelope,
        ReplanProposalState,
    };
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub(crate) enum Tier {
        #[serde(rename = "tier_0")]
        Tier0,
        #[serde(rename = "tier_1")]
        Tier1,
        #[serde(rename = "tier_2")]
        Tier2,
        #[serde(rename = "tier_3")]
        Tier3,
        Stop,
    }

    impl Tier {
        pub(crate) fn as_str(self) -> &'static str {
            match self {
                Self::Tier0 => "tier_0",
                Self::Tier1 => "tier_1",
                Self::Tier2 => "tier_2",
                Self::Tier3 => "tier_3",
                Self::Stop => "stop",
            }
        }
    }

    impl std::fmt::Display for Tier {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.as_str())
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
    pub(crate) enum ReasonCode {
        #[serde(rename = "inline_within_slice_contract")]
        InlineWithinSliceContract,
        #[serde(rename = "inline_no_new_slice")]
        InlineNoNewSlice,
        #[serde(rename = "frontier_disabled")]
        FrontierDisabled,
        #[serde(rename = "shadow_observation_only")]
        ShadowObservationOnly,
        #[serde(rename = "inside_allowed_areas")]
        InsideAllowedAreas,
        #[serde(rename = "acceptance_present")]
        AcceptancePresent,
        #[serde(rename = "verify_present")]
        VerifyPresent,
        #[serde(rename = "within_budget")]
        WithinBudget,
        #[serde(rename = "within_depth")]
        WithinDepth,
        #[serde(rename = "not_duplicate")]
        NotDuplicate,
        #[serde(rename = "add_followup_slice_only")]
        AddFollowupSliceOnly,
        #[serde(rename = "area_outside_envelope")]
        AreaOutsideEnvelope,
        #[serde(rename = "area_ambiguous")]
        AreaAmbiguous,
        #[serde(rename = "non_goal_overlap")]
        NonGoalOverlap,
        #[serde(rename = "candidate_changes_dependencies")]
        CandidateChangesDependencies,
        #[serde(rename = "candidate_changes_acceptance")]
        CandidateChangesAcceptance,
        #[serde(rename = "candidate_changes_verify_profile")]
        CandidateChangesVerifyProfile,
        #[serde(rename = "candidate_changes_policy_or_schema")]
        CandidateChangesPolicyOrSchema,
        #[serde(rename = "candidate_hits_must_ask_if")]
        CandidateHitsMustAskIf,
        #[serde(rename = "envelope_must_ask_hit")]
        EnvelopeMustAskHit,
        #[serde(rename = "operator_only_change_kind")]
        OperatorOnlyChangeKind,
        #[serde(rename = "duplicate_rejected_or_deferred_proposal")]
        DuplicateRejectedOrDeferredProposal,
        #[serde(rename = "classification_ambiguous")]
        ClassificationAmbiguous,
        #[serde(rename = "frontier_budget_exhausted")]
        FrontierBudgetExhausted,
        #[serde(rename = "frontier_depth_exhausted")]
        FrontierDepthExhausted,
        #[serde(rename = "no_frontier")]
        NoFrontier,
        #[serde(rename = "cancel_requested")]
        CancelRequested,
        #[serde(rename = "replan_apply_incomplete")]
        ReplanApplyIncomplete,
        #[serde(rename = "candidate_missing_acceptance")]
        CandidateMissingAcceptance,
        #[serde(rename = "candidate_missing_verify")]
        CandidateMissingVerify,
        #[serde(rename = "duplicate_open_slice")]
        DuplicateOpenSlice,
        #[serde(rename = "duplicate_closed_slice")]
        DuplicateClosedSlice,
        #[serde(rename = "duplicate_pending_proposal")]
        DuplicatePendingProposal,
        #[serde(rename = "proposal_needs_operator_context")]
        ProposalNeedsOperatorContext,
    }

    impl ReasonCode {
        pub(crate) const ALL: &'static [ReasonCode] = &[
            Self::InlineWithinSliceContract,
            Self::InlineNoNewSlice,
            Self::FrontierDisabled,
            Self::ShadowObservationOnly,
            Self::InsideAllowedAreas,
            Self::AcceptancePresent,
            Self::VerifyPresent,
            Self::WithinBudget,
            Self::WithinDepth,
            Self::NotDuplicate,
            Self::AddFollowupSliceOnly,
            Self::AreaOutsideEnvelope,
            Self::AreaAmbiguous,
            Self::NonGoalOverlap,
            Self::CandidateChangesDependencies,
            Self::CandidateChangesAcceptance,
            Self::CandidateChangesVerifyProfile,
            Self::CandidateChangesPolicyOrSchema,
            Self::CandidateHitsMustAskIf,
            Self::EnvelopeMustAskHit,
            Self::OperatorOnlyChangeKind,
            Self::DuplicateRejectedOrDeferredProposal,
            Self::ClassificationAmbiguous,
            Self::FrontierBudgetExhausted,
            Self::FrontierDepthExhausted,
            Self::NoFrontier,
            Self::CancelRequested,
            Self::ReplanApplyIncomplete,
            Self::CandidateMissingAcceptance,
            Self::CandidateMissingVerify,
            Self::DuplicateOpenSlice,
            Self::DuplicateClosedSlice,
            Self::DuplicatePendingProposal,
            Self::ProposalNeedsOperatorContext,
        ];

        pub(crate) fn as_str(self) -> &'static str {
            match self {
                Self::InlineWithinSliceContract => "inline_within_slice_contract",
                Self::InlineNoNewSlice => "inline_no_new_slice",
                Self::FrontierDisabled => "frontier_disabled",
                Self::ShadowObservationOnly => "shadow_observation_only",
                Self::InsideAllowedAreas => "inside_allowed_areas",
                Self::AcceptancePresent => "acceptance_present",
                Self::VerifyPresent => "verify_present",
                Self::WithinBudget => "within_budget",
                Self::WithinDepth => "within_depth",
                Self::NotDuplicate => "not_duplicate",
                Self::AddFollowupSliceOnly => "add_followup_slice_only",
                Self::AreaOutsideEnvelope => "area_outside_envelope",
                Self::AreaAmbiguous => "area_ambiguous",
                Self::NonGoalOverlap => "non_goal_overlap",
                Self::CandidateChangesDependencies => "candidate_changes_dependencies",
                Self::CandidateChangesAcceptance => "candidate_changes_acceptance",
                Self::CandidateChangesVerifyProfile => "candidate_changes_verify_profile",
                Self::CandidateChangesPolicyOrSchema => "candidate_changes_policy_or_schema",
                Self::CandidateHitsMustAskIf => "candidate_hits_must_ask_if",
                Self::EnvelopeMustAskHit => "envelope_must_ask_hit",
                Self::OperatorOnlyChangeKind => "operator_only_change_kind",
                Self::DuplicateRejectedOrDeferredProposal => {
                    "duplicate_rejected_or_deferred_proposal"
                }
                Self::ClassificationAmbiguous => "classification_ambiguous",
                Self::FrontierBudgetExhausted => "frontier_budget_exhausted",
                Self::FrontierDepthExhausted => "frontier_depth_exhausted",
                Self::NoFrontier => "no_frontier",
                Self::CancelRequested => "cancel_requested",
                Self::ReplanApplyIncomplete => "replan_apply_incomplete",
                Self::CandidateMissingAcceptance => "candidate_missing_acceptance",
                Self::CandidateMissingVerify => "candidate_missing_verify",
                Self::DuplicateOpenSlice => "duplicate_open_slice",
                Self::DuplicateClosedSlice => "duplicate_closed_slice",
                Self::DuplicatePendingProposal => "duplicate_pending_proposal",
                Self::ProposalNeedsOperatorContext => "proposal_needs_operator_context",
            }
        }

        fn is_stop(self) -> bool {
            matches!(
                self,
                Self::FrontierBudgetExhausted
                    | Self::FrontierDepthExhausted
                    | Self::NoFrontier
                    | Self::CancelRequested
                    | Self::ReplanApplyIncomplete
            )
        }

        fn is_tier3(self) -> bool {
            matches!(
                self,
                Self::AreaOutsideEnvelope
                    | Self::AreaAmbiguous
                    | Self::NonGoalOverlap
                    | Self::CandidateChangesDependencies
                    | Self::CandidateChangesAcceptance
                    | Self::CandidateChangesVerifyProfile
                    | Self::CandidateChangesPolicyOrSchema
                    | Self::CandidateHitsMustAskIf
                    | Self::EnvelopeMustAskHit
                    | Self::OperatorOnlyChangeKind
                    | Self::DuplicateRejectedOrDeferredProposal
                    | Self::ClassificationAmbiguous
            )
        }

        fn is_tier2(self) -> bool {
            matches!(
                self,
                Self::FrontierDisabled
                    | Self::CandidateMissingAcceptance
                    | Self::CandidateMissingVerify
                    | Self::DuplicateOpenSlice
                    | Self::DuplicateClosedSlice
                    | Self::DuplicatePendingProposal
                    | Self::ProposalNeedsOperatorContext
            )
        }
    }

    impl std::fmt::Display for ReasonCode {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.as_str())
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub(crate) struct TierDecision {
        pub tier: Tier,
        pub reason_codes: Vec<ReasonCode>,
    }

    impl TierDecision {
        pub(crate) fn reason_strings(&self) -> Vec<&'static str> {
            self.reason_codes.iter().map(|code| code.as_str()).collect()
        }

        pub(crate) fn has_reason(&self, code: ReasonCode) -> bool {
            self.reason_codes.contains(&code)
        }
    }

    #[derive(Debug, Clone, Copy)]
    pub(crate) enum FollowupProposalChange<'a> {
        AddFollowupSlice(&'a FollowupSliceDraft),
        InlineWithinSlice,
        Other(&'a str),
    }

    #[derive(Debug, Clone, Copy)]
    pub(crate) struct FollowupProposalView<'a> {
        pub proposal_id: &'a str,
        pub source_slice_id: &'a str,
        pub change: FollowupProposalChange<'a>,
        pub source_must_ask_if_hits: &'a [&'a str],
        pub envelope_must_ask_if_hits: &'a [&'a str],
        pub external_dependency_claims: &'a [&'a str],
        pub changes_existing_dependencies: bool,
        pub changes_existing_acceptance: bool,
        pub changes_verify_profile: bool,
        pub changes_policy_or_schema: bool,
        pub needs_operator_context: bool,
        pub ambiguity_markers: &'a [&'a str],
    }

    impl<'a> FollowupProposalView<'a> {
        pub(crate) fn add_followup_slice(draft: &'a FollowupSliceDraft) -> Self {
            Self {
                proposal_id: "",
                source_slice_id: "",
                change: FollowupProposalChange::AddFollowupSlice(draft),
                source_must_ask_if_hits: &[],
                envelope_must_ask_if_hits: &[],
                external_dependency_claims: &[],
                changes_existing_dependencies: false,
                changes_existing_acceptance: false,
                changes_verify_profile: false,
                changes_policy_or_schema: false,
                needs_operator_context: false,
                ambiguity_markers: &[],
            }
        }

        pub(crate) fn inline_within_slice() -> Self {
            Self {
                proposal_id: "",
                source_slice_id: "",
                change: FollowupProposalChange::InlineWithinSlice,
                source_must_ask_if_hits: &[],
                envelope_must_ask_if_hits: &[],
                external_dependency_claims: &[],
                changes_existing_dependencies: false,
                changes_existing_acceptance: false,
                changes_verify_profile: false,
                changes_policy_or_schema: false,
                needs_operator_context: false,
                ambiguity_markers: &[],
            }
        }

        pub(crate) fn operator_only(change_kind: &'a str) -> Self {
            Self {
                proposal_id: "",
                source_slice_id: "",
                change: FollowupProposalChange::Other(change_kind),
                source_must_ask_if_hits: &[],
                envelope_must_ask_if_hits: &[],
                external_dependency_claims: &[],
                changes_existing_dependencies: false,
                changes_existing_acceptance: false,
                changes_verify_profile: false,
                changes_policy_or_schema: false,
                needs_operator_context: false,
                ambiguity_markers: &[],
            }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum SliceGraphSliceStatus {
        Open,
        Closed,
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct SliceGraphSlice {
        pub id: String,
        pub goal: String,
        pub status: SliceGraphSliceStatus,
        pub generation: i64,
    }

    impl SliceGraphSlice {
        pub(crate) fn open(id: impl Into<String>, goal: impl Into<String>) -> Self {
            Self {
                id: id.into(),
                goal: goal.into(),
                status: SliceGraphSliceStatus::Open,
                generation: 0,
            }
        }

        pub(crate) fn closed(id: impl Into<String>, goal: impl Into<String>) -> Self {
            Self {
                id: id.into(),
                goal: goal.into(),
                status: SliceGraphSliceStatus::Closed,
                generation: 0,
            }
        }

        pub(crate) fn generated(
            id: impl Into<String>,
            goal: impl Into<String>,
            status: SliceGraphSliceStatus,
            generation: i64,
        ) -> Self {
            Self {
                id: id.into(),
                goal: goal.into(),
                status,
                generation,
            }
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct ProposalGraphNode {
        pub id: String,
        pub state: ReplanProposalState,
        pub draft_id: String,
        pub draft_goal: String,
    }

    impl ProposalGraphNode {
        pub(crate) fn from_draft(
            id: impl Into<String>,
            state: ReplanProposalState,
            draft: &FollowupSliceDraft,
        ) -> Self {
            Self {
                id: id.into(),
                state,
                draft_id: draft.id.clone(),
                draft_goal: draft.goal.clone(),
            }
        }
    }

    #[derive(Debug, Clone, Default, PartialEq, Eq)]
    pub(crate) struct SliceGraphView {
        pub slices: Vec<SliceGraphSlice>,
        pub proposals: Vec<ProposalGraphNode>,
        pub no_frontier: bool,
        pub cancel_requested: bool,
        pub replan_apply_incomplete: bool,
    }

    pub(crate) fn classify_followup_proposal(
        envelope: &MissionEnvelope,
        graph: &SliceGraphView,
        proposal: &FollowupProposalView<'_>,
        budget: &FrontierBudgetState,
    ) -> TierDecision {
        let mut decision = DecisionBuilder::default();
        collect_graph_stops(graph, &mut decision);

        match proposal.change {
            FollowupProposalChange::InlineWithinSlice => {
                decision.push(ReasonCode::InlineWithinSliceContract);
                decision.push(ReasonCode::InlineNoNewSlice);
                decision.finish(true)
            }
            FollowupProposalChange::Other(change_kind) => {
                decision.push(ReasonCode::OperatorOnlyChangeKind);
                if change_kind.trim().is_empty() {
                    decision.push(ReasonCode::ClassificationAmbiguous);
                }
                decision.finish(false)
            }
            FollowupProposalChange::AddFollowupSlice(candidate) => {
                classify_add_followup_slice(envelope, graph, proposal, candidate, budget, decision)
            }
        }
    }

    fn classify_add_followup_slice(
        envelope: &MissionEnvelope,
        graph: &SliceGraphView,
        proposal: &FollowupProposalView<'_>,
        candidate: &FollowupSliceDraft,
        budget: &FrontierBudgetState,
        mut decision: DecisionBuilder,
    ) -> TierDecision {
        match envelope.autonomy_level {
            AutonomyLevel::Off => decision.push(ReasonCode::FrontierDisabled),
            AutonomyLevel::Shadow | AutonomyLevel::Promote | AutonomyLevel::Run => {
                decision.push(ReasonCode::ShadowObservationOnly)
            }
        }

        decision.push(ReasonCode::AddFollowupSliceOnly);

        if envelope.autonomy_level != AutonomyLevel::Off {
            if budget_exhausted(envelope, budget) {
                decision.push(ReasonCode::FrontierBudgetExhausted);
            } else {
                decision.push(ReasonCode::WithinBudget);
            }

            if depth_exhausted(envelope, graph, proposal, budget) {
                decision.push(ReasonCode::FrontierDepthExhausted);
            } else {
                decision.push(ReasonCode::WithinDepth);
            }
        }

        match classify_area_containment(&candidate.areas, &envelope.allowed_areas) {
            AreaClassification::Inside => decision.push(ReasonCode::InsideAllowedAreas),
            AreaClassification::Outside => decision.push(ReasonCode::AreaOutsideEnvelope),
            AreaClassification::Ambiguous => decision.push(ReasonCode::AreaAmbiguous),
        }

        classify_duplicate(candidate, graph, proposal.proposal_id, &mut decision);

        if non_goal_overlap(candidate, &envelope.non_goals) {
            decision.push(ReasonCode::NonGoalOverlap);
        }
        if !proposal.source_must_ask_if_hits.is_empty() {
            decision.push(ReasonCode::CandidateHitsMustAskIf);
        }
        if !proposal.envelope_must_ask_if_hits.is_empty()
            || envelope_must_ask_hit(candidate, &envelope.must_ask_if)
        {
            decision.push(ReasonCode::EnvelopeMustAskHit);
        }
        if proposal.changes_existing_dependencies
            || !proposal.external_dependency_claims.is_empty()
            || has_dependency_outside_graph(candidate, graph)
        {
            decision.push(ReasonCode::CandidateChangesDependencies);
        }
        if proposal.changes_existing_acceptance {
            decision.push(ReasonCode::CandidateChangesAcceptance);
        }
        if proposal.changes_verify_profile
            || (!candidate.verify_profile.trim().is_empty()
                && candidate.verify_profile.trim() != envelope.verify_profile.trim())
        {
            decision.push(ReasonCode::CandidateChangesVerifyProfile);
        }
        if proposal.changes_policy_or_schema {
            decision.push(ReasonCode::CandidateChangesPolicyOrSchema);
        }
        if !acceptance_is_testable(&candidate.acceptance) {
            decision.push(ReasonCode::CandidateMissingAcceptance);
        } else {
            decision.push(ReasonCode::AcceptancePresent);
        }
        if !verify_is_present(candidate, envelope) {
            decision.push(ReasonCode::CandidateMissingVerify);
        } else {
            decision.push(ReasonCode::VerifyPresent);
        }
        if proposal.needs_operator_context {
            decision.push(ReasonCode::ProposalNeedsOperatorContext);
        }
        if !proposal.ambiguity_markers.is_empty() {
            decision.push(ReasonCode::ClassificationAmbiguous);
        }

        let tier1_ready = TIER1_POSITIVE_CODES
            .iter()
            .all(|code| decision.has_reason(*code));
        decision.finish(tier1_ready)
    }

    const TIER1_POSITIVE_CODES: &[ReasonCode] = &[
        ReasonCode::InsideAllowedAreas,
        ReasonCode::AcceptancePresent,
        ReasonCode::VerifyPresent,
        ReasonCode::WithinBudget,
        ReasonCode::WithinDepth,
        ReasonCode::NotDuplicate,
        ReasonCode::AddFollowupSliceOnly,
    ];

    #[derive(Default)]
    struct DecisionBuilder {
        reason_codes: Vec<ReasonCode>,
    }

    impl DecisionBuilder {
        fn push(&mut self, code: ReasonCode) {
            if !self.reason_codes.contains(&code) {
                self.reason_codes.push(code);
            }
        }

        fn has_reason(&self, code: ReasonCode) -> bool {
            self.reason_codes.contains(&code)
        }

        fn finish(mut self, tier1_ready: bool) -> TierDecision {
            if self.reason_codes.is_empty() {
                self.push(ReasonCode::ClassificationAmbiguous);
            }
            let tier = if self.reason_codes.iter().any(|code| code.is_stop()) {
                Tier::Stop
            } else if self.reason_codes.iter().any(|code| code.is_tier3()) {
                Tier::Tier3
            } else if self.reason_codes.iter().any(|code| code.is_tier2()) {
                Tier::Tier2
            } else if self
                .reason_codes
                .contains(&ReasonCode::InlineWithinSliceContract)
            {
                Tier::Tier0
            } else if tier1_ready {
                Tier::Tier1
            } else {
                self.push(ReasonCode::ClassificationAmbiguous);
                Tier::Tier3
            };
            TierDecision {
                tier,
                reason_codes: self.reason_codes,
            }
        }
    }

    fn collect_graph_stops(graph: &SliceGraphView, decision: &mut DecisionBuilder) {
        if graph.cancel_requested {
            decision.push(ReasonCode::CancelRequested);
        }
        if graph.replan_apply_incomplete {
            decision.push(ReasonCode::ReplanApplyIncomplete);
        }
        if graph.no_frontier {
            decision.push(ReasonCode::NoFrontier);
        }
    }

    fn budget_exhausted(envelope: &MissionEnvelope, budget: &FrontierBudgetState) -> bool {
        envelope.max_auto_promotions <= budget.auto_promotions_used
            || envelope.max_generated_slices <= budget.generated_slices
    }

    fn depth_exhausted(
        envelope: &MissionEnvelope,
        graph: &SliceGraphView,
        proposal: &FollowupProposalView<'_>,
        budget: &FrontierBudgetState,
    ) -> bool {
        if budget.max_generation_reached || envelope.max_depth < 0 {
            return true;
        }
        let parent_generation = graph
            .slices
            .iter()
            .find(|slice| slice.id == proposal.source_slice_id)
            .map(|slice| slice.generation)
            .unwrap_or(0);
        let candidate_generation = parent_generation.saturating_add(1);
        candidate_generation > envelope.max_depth
    }

    enum AreaClassification {
        Inside,
        Outside,
        Ambiguous,
    }

    fn classify_area_containment(
        candidate_areas: &[String],
        allowed_areas: &[String],
    ) -> AreaClassification {
        if candidate_areas.is_empty()
            || allowed_areas.is_empty()
            || candidate_areas.iter().any(|area| !valid_area(area))
            || allowed_areas.iter().any(|area| !valid_area(area))
        {
            return AreaClassification::Ambiguous;
        }
        if candidate_areas.iter().all(|candidate| {
            allowed_areas
                .iter()
                .any(|allowed| area_contains(allowed, candidate))
        }) {
            AreaClassification::Inside
        } else {
            AreaClassification::Outside
        }
    }

    fn valid_area(area: &str) -> bool {
        !area.trim().is_empty()
            && area.trim() == area
            && !area.starts_with('/')
            && !area.starts_with("./")
            && !area.contains("..")
            && !area.chars().any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
    }

    fn area_contains(allowed: &str, candidate: &str) -> bool {
        if allowed.ends_with('/') {
            candidate.starts_with(allowed)
        } else {
            candidate == allowed
        }
    }

    fn classify_duplicate(
        candidate: &FollowupSliceDraft,
        graph: &SliceGraphView,
        current_proposal_id: &str,
        decision: &mut DecisionBuilder,
    ) {
        let mut duplicate_found = false;
        let candidate_goal = normalized_goal(&candidate.goal);

        for proposal in graph.proposals.iter().filter(|proposal| {
            current_proposal_id.is_empty() || proposal.id.as_str() != current_proposal_id
        }) {
            if same_candidate(
                candidate,
                &candidate_goal,
                &proposal.draft_id,
                &proposal.draft_goal,
            ) {
                duplicate_found = true;
                match proposal.state {
                    ReplanProposalState::Rejected | ReplanProposalState::Deferred => {
                        decision.push(ReasonCode::DuplicateRejectedOrDeferredProposal)
                    }
                    ReplanProposalState::Pending => {
                        decision.push(ReasonCode::DuplicatePendingProposal)
                    }
                    ReplanProposalState::Accepted | ReplanProposalState::Superseded => {}
                }
            }
        }

        for slice in &graph.slices {
            if same_candidate(candidate, &candidate_goal, &slice.id, &slice.goal) {
                duplicate_found = true;
                match slice.status {
                    SliceGraphSliceStatus::Open => decision.push(ReasonCode::DuplicateOpenSlice),
                    SliceGraphSliceStatus::Closed => {
                        decision.push(ReasonCode::DuplicateClosedSlice)
                    }
                }
            }
        }

        if !duplicate_found {
            decision.push(ReasonCode::NotDuplicate);
        }
    }

    fn same_candidate(
        candidate: &FollowupSliceDraft,
        normalized_candidate_goal: &str,
        other_id: &str,
        other_goal: &str,
    ) -> bool {
        (!candidate.id.trim().is_empty() && candidate.id == other_id)
            || (!normalized_candidate_goal.is_empty()
                && normalized_candidate_goal == normalized_goal(other_goal))
    }

    fn normalized_goal(value: &str) -> String {
        value
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn non_goal_overlap(candidate: &FollowupSliceDraft, non_goals: &[String]) -> bool {
        let text = normalized_text(candidate_text(candidate));
        non_goals
            .iter()
            .map(|non_goal| normalized_text(non_goal))
            .filter(|non_goal| !non_goal.is_empty())
            .any(|non_goal| text.contains(&non_goal))
    }

    fn envelope_must_ask_hit(candidate: &FollowupSliceDraft, must_ask_if: &[String]) -> bool {
        let text = normalized_text(candidate_text(candidate));
        must_ask_if
            .iter()
            .map(|rule| normalized_text(rule))
            .filter(|rule| !rule.is_empty())
            .any(|rule| text.contains(&rule))
    }

    fn candidate_text(candidate: &FollowupSliceDraft) -> String {
        let mut parts = vec![
            candidate.id.as_str(),
            candidate.title.as_str(),
            candidate.goal.as_str(),
            candidate.rationale.as_str(),
        ];
        parts.extend(candidate.areas.iter().map(String::as_str));
        parts.extend(candidate.acceptance.iter().map(String::as_str));
        parts.join("\n")
    }

    fn normalized_text(value: impl AsRef<str>) -> String {
        value
            .as_ref()
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn has_dependency_outside_graph(
        candidate: &FollowupSliceDraft,
        graph: &SliceGraphView,
    ) -> bool {
        candidate
            .depends_on
            .iter()
            .any(|dep| !dep.trim().is_empty() && !graph.slices.iter().any(|slice| slice.id == *dep))
    }

    fn acceptance_is_testable(acceptance: &[String]) -> bool {
        acceptance.iter().any(|criterion| {
            let criterion = normalized_text(criterion);
            !criterion.is_empty()
                && !matches!(
                    criterion.as_str(),
                    "todo" | "tbd" | "works" | "do the thing" | "good"
                )
        })
    }

    fn verify_is_present(candidate: &FollowupSliceDraft, envelope: &MissionEnvelope) -> bool {
        candidate
            .verify
            .iter()
            .any(|command| !command.trim().is_empty())
            || (!candidate.verify_profile.trim().is_empty()
                && candidate.verify_profile.trim() == envelope.verify_profile.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::promotion_policy::*;
    use crate::domain::{
        AutonomyLevel, FollowupSliceDraft, FrontierBudgetState, MissionEnvelope,
        ReplanProposalState,
    };
    use std::collections::BTreeSet;

    fn envelope() -> MissionEnvelope {
        MissionEnvelope {
            goal: "Keep generated work inside the mission".to_string(),
            allowed_areas: vec!["src/foo/".to_string(), "README.md".to_string()],
            non_goals: Vec::new(),
            verify_profile: "default".to_string(),
            max_auto_promotions: 1,
            max_depth: 1,
            max_generated_slices: 1,
            autonomy_level: AutonomyLevel::Promote,
            must_ask_if: Vec::new(),
        }
    }

    fn clean_draft(id: &str) -> FollowupSliceDraft {
        FollowupSliceDraft {
            id: id.to_string(),
            title: "Follow-up inside mission".to_string(),
            goal: "Add bounded follow-up behavior".to_string(),
            areas: vec!["src/foo/bar.rs".to_string()],
            acceptance: vec!["Test classifies the follow-up deterministically".to_string()],
            verify: vec!["cargo test frontier --quiet".to_string()],
            verify_profile: String::new(),
            depends_on: vec!["SRC-1".to_string()],
            must_ask_if: Vec::new(),
            rationale: "The worker found bounded follow-up work".to_string(),
        }
    }

    fn graph() -> SliceGraphView {
        SliceGraphView {
            slices: vec![SliceGraphSlice::open("SRC-1", "Parent work")],
            ..SliceGraphView::default()
        }
    }

    fn proposal<'a>(draft: &'a FollowupSliceDraft) -> FollowupProposalView<'a> {
        FollowupProposalView {
            source_slice_id: "SRC-1",
            ..FollowupProposalView::add_followup_slice(draft)
        }
    }

    fn classify(
        envelope: &MissionEnvelope,
        graph: &SliceGraphView,
        draft: &FollowupSliceDraft,
        budget: &FrontierBudgetState,
    ) -> TierDecision {
        classify_followup_proposal(envelope, graph, &proposal(draft), budget)
    }

    fn assert_decision_contains(
        decision: &TierDecision,
        tier: Tier,
        required_reasons: &[ReasonCode],
    ) {
        assert_eq!(
            decision.tier,
            tier,
            "reasons: {:?}",
            decision.reason_strings()
        );
        for reason in required_reasons {
            assert!(
                decision.has_reason(*reason),
                "missing {reason}; saw {:?}",
                decision.reason_strings()
            );
        }
    }

    #[test]
    fn frontier_promotion_policy_af00_rfc_scenario_table_is_deterministic() {
        let env = envelope();
        let base_graph = graph();
        let budget = FrontierBudgetState::default();
        let clean = clean_draft("AF-CLEAN");

        let clean_decision = classify(&env, &base_graph, &clean, &budget);
        assert_eq!(
            clean_decision,
            classify(&env, &base_graph, &clean, &budget),
            "classify must be referentially transparent for identical inputs"
        );
        assert_decision_contains(
            &clean_decision,
            Tier::Tier1,
            &[
                ReasonCode::AddFollowupSliceOnly,
                ReasonCode::InsideAllowedAreas,
                ReasonCode::AcceptancePresent,
                ReasonCode::VerifyPresent,
                ReasonCode::WithinBudget,
                ReasonCode::WithinDepth,
                ReasonCode::NotDuplicate,
            ],
        );

        let mut outside_area = clean_draft("AF-AREA");
        outside_area.areas = vec!["src/other/file.rs".to_string()];
        assert_decision_contains(
            &classify(&env, &base_graph, &outside_area, &budget),
            Tier::Tier3,
            &[ReasonCode::AreaOutsideEnvelope],
        );

        let mut outside_dependency = clean_draft("AF-DEP");
        outside_dependency.depends_on = vec!["NOT-IN-RUN".to_string()];
        assert_decision_contains(
            &classify(&env, &base_graph, &outside_dependency, &budget),
            Tier::Tier3,
            &[ReasonCode::CandidateChangesDependencies],
        );

        let source_must_ask = clean_draft("AF-SOURCE-ASK");
        let source_must_ask_proposal = FollowupProposalView {
            source_slice_id: "SRC-1",
            source_must_ask_if_hits: &["public API semantics changed"],
            ..FollowupProposalView::add_followup_slice(&source_must_ask)
        };
        assert_decision_contains(
            &classify_followup_proposal(&env, &base_graph, &source_must_ask_proposal, &budget),
            Tier::Tier3,
            &[ReasonCode::CandidateHitsMustAskIf],
        );

        let envelope_must_ask = clean_draft("AF-ENVELOPE-ASK");
        let envelope_must_ask_proposal = FollowupProposalView {
            source_slice_id: "SRC-1",
            envelope_must_ask_if_hits: &["candidate changes workflow policy"],
            ..FollowupProposalView::add_followup_slice(&envelope_must_ask)
        };
        assert_decision_contains(
            &classify_followup_proposal(&env, &base_graph, &envelope_must_ask_proposal, &budget),
            Tier::Tier3,
            &[ReasonCode::EnvelopeMustAskHit],
        );

        let mut missing_verify = clean_draft("AF-NO-VERIFY");
        missing_verify.verify.clear();
        assert_decision_contains(
            &classify(&env, &base_graph, &missing_verify, &budget),
            Tier::Tier2,
            &[ReasonCode::CandidateMissingVerify],
        );

        let duplicate_open_graph = SliceGraphView {
            slices: vec![
                SliceGraphSlice::open("SRC-1", "Parent work"),
                SliceGraphSlice::open("AF-OPEN", "Another open slice"),
            ],
            ..SliceGraphView::default()
        };
        let duplicate_open = clean_draft("AF-OPEN");
        assert_decision_contains(
            &classify(&env, &duplicate_open_graph, &duplicate_open, &budget),
            Tier::Tier2,
            &[ReasonCode::DuplicateOpenSlice],
        );

        let duplicate_closed_graph = SliceGraphView {
            slices: vec![
                SliceGraphSlice::open("SRC-1", "Parent work"),
                SliceGraphSlice::closed("DONE", "Add bounded follow-up behavior"),
            ],
            ..SliceGraphView::default()
        };
        let duplicate_closed = clean_draft("AF-CLOSED");
        assert_decision_contains(
            &classify(&env, &duplicate_closed_graph, &duplicate_closed, &budget),
            Tier::Tier2,
            &[ReasonCode::DuplicateClosedSlice],
        );

        let duplicate_rejected = clean_draft("AF-REJECTED");
        let duplicate_rejected_graph = SliceGraphView {
            slices: vec![SliceGraphSlice::open("SRC-1", "Parent work")],
            proposals: vec![ProposalGraphNode::from_draft(
                "rp-rejected",
                ReplanProposalState::Rejected,
                &duplicate_rejected,
            )],
            ..SliceGraphView::default()
        };
        assert_decision_contains(
            &classify(
                &env,
                &duplicate_rejected_graph,
                &duplicate_rejected,
                &budget,
            ),
            Tier::Tier3,
            &[ReasonCode::DuplicateRejectedOrDeferredProposal],
        );

        let exhausted_budget = FrontierBudgetState {
            auto_promotions_used: 1,
            ..FrontierBudgetState::default()
        };
        assert_decision_contains(
            &classify(
                &env,
                &base_graph,
                &clean_draft("AF-BUDGET"),
                &exhausted_budget,
            ),
            Tier::Stop,
            &[ReasonCode::FrontierBudgetExhausted],
        );

        let depth_graph = SliceGraphView {
            slices: vec![SliceGraphSlice::generated(
                "SRC-1",
                "Parent work",
                SliceGraphSliceStatus::Open,
                1,
            )],
            ..SliceGraphView::default()
        };
        assert_decision_contains(
            &classify(&env, &depth_graph, &clean_draft("AF-DEPTH"), &budget),
            Tier::Stop,
            &[ReasonCode::FrontierDepthExhausted],
        );

        let mut non_goal_env = env.clone();
        non_goal_env.non_goals = vec!["forbidden policy".to_string()];
        let mut non_goal = clean_draft("AF-NON-GOAL");
        non_goal.goal = "Implement forbidden policy".to_string();
        assert_decision_contains(
            &classify(&non_goal_env, &base_graph, &non_goal, &budget),
            Tier::Tier3,
            &[ReasonCode::NonGoalOverlap],
        );
    }

    #[test]
    fn frontier_promotion_policy_covers_every_stable_reason_code() {
        let mut seen = BTreeSet::new();
        let mut add_seen = |decision: TierDecision| {
            for reason in decision.reason_codes {
                seen.insert(reason);
            }
        };

        let env = envelope();
        let base_graph = graph();
        let budget = FrontierBudgetState::default();

        add_seen(classify(
            &env,
            &base_graph,
            &clean_draft("AF-COVER-CLEAN"),
            &budget,
        ));

        add_seen(classify_followup_proposal(
            &env,
            &base_graph,
            &FollowupProposalView::inline_within_slice(),
            &budget,
        ));

        let mut off_env = env.clone();
        off_env.autonomy_level = AutonomyLevel::Off;
        add_seen(classify(
            &off_env,
            &base_graph,
            &clean_draft("AF-COVER-OFF"),
            &budget,
        ));

        let mut shadow_env = env.clone();
        shadow_env.autonomy_level = AutonomyLevel::Shadow;
        add_seen(classify(
            &shadow_env,
            &base_graph,
            &clean_draft("AF-COVER-SHADOW"),
            &budget,
        ));

        let mut outside = clean_draft("AF-COVER-OUTSIDE");
        outside.areas = vec!["src/outside.rs".to_string()];
        add_seen(classify(&env, &base_graph, &outside, &budget));

        let mut ambiguous_area = clean_draft("AF-COVER-AMBIGUOUS-AREA");
        ambiguous_area.areas = vec!["src/*.rs".to_string()];
        add_seen(classify(&env, &base_graph, &ambiguous_area, &budget));

        let mut non_goal_env = env.clone();
        non_goal_env.non_goals = vec!["blocked literal".to_string()];
        let mut non_goal = clean_draft("AF-COVER-NON-GOAL");
        non_goal.rationale = "blocked literal".to_string();
        add_seen(classify(&non_goal_env, &base_graph, &non_goal, &budget));

        let mut dependency = clean_draft("AF-COVER-DEPENDENCY");
        dependency.depends_on = vec!["OUTSIDE".to_string()];
        add_seen(classify(&env, &base_graph, &dependency, &budget));

        let acceptance_change = clean_draft("AF-COVER-ACCEPTANCE-CHANGE");
        let acceptance_change_proposal = FollowupProposalView {
            source_slice_id: "SRC-1",
            changes_existing_acceptance: true,
            ..FollowupProposalView::add_followup_slice(&acceptance_change)
        };
        add_seen(classify_followup_proposal(
            &env,
            &base_graph,
            &acceptance_change_proposal,
            &budget,
        ));

        let mut verify_profile_change = clean_draft("AF-COVER-VERIFY-PROFILE");
        verify_profile_change.verify_profile = "heavy".to_string();
        add_seen(classify(&env, &base_graph, &verify_profile_change, &budget));

        let policy_change = clean_draft("AF-COVER-POLICY");
        let policy_change_proposal = FollowupProposalView {
            source_slice_id: "SRC-1",
            changes_policy_or_schema: true,
            ..FollowupProposalView::add_followup_slice(&policy_change)
        };
        add_seen(classify_followup_proposal(
            &env,
            &base_graph,
            &policy_change_proposal,
            &budget,
        ));

        let source_ask = clean_draft("AF-COVER-SOURCE-ASK");
        let source_ask_proposal = FollowupProposalView {
            source_slice_id: "SRC-1",
            source_must_ask_if_hits: &["ask source"],
            ..FollowupProposalView::add_followup_slice(&source_ask)
        };
        add_seen(classify_followup_proposal(
            &env,
            &base_graph,
            &source_ask_proposal,
            &budget,
        ));

        let envelope_ask = clean_draft("AF-COVER-ENVELOPE-ASK");
        let envelope_ask_proposal = FollowupProposalView {
            source_slice_id: "SRC-1",
            envelope_must_ask_if_hits: &["ask envelope"],
            ..FollowupProposalView::add_followup_slice(&envelope_ask)
        };
        add_seen(classify_followup_proposal(
            &env,
            &base_graph,
            &envelope_ask_proposal,
            &budget,
        ));

        add_seen(classify_followup_proposal(
            &env,
            &base_graph,
            &FollowupProposalView::operator_only("change_verify_profile"),
            &budget,
        ));

        let rejected = clean_draft("AF-COVER-REJECTED");
        let rejected_graph = SliceGraphView {
            slices: vec![SliceGraphSlice::open("SRC-1", "Parent work")],
            proposals: vec![ProposalGraphNode::from_draft(
                "rp-rejected",
                ReplanProposalState::Rejected,
                &rejected,
            )],
            ..SliceGraphView::default()
        };
        add_seen(classify(&env, &rejected_graph, &rejected, &budget));

        let ambiguous = clean_draft("AF-COVER-AMBIGUOUS");
        let ambiguous_proposal = FollowupProposalView {
            source_slice_id: "SRC-1",
            ambiguity_markers: &["api"],
            ..FollowupProposalView::add_followup_slice(&ambiguous)
        };
        add_seen(classify_followup_proposal(
            &env,
            &base_graph,
            &ambiguous_proposal,
            &budget,
        ));

        add_seen(classify(
            &env,
            &base_graph,
            &clean_draft("AF-COVER-BUDGET"),
            &FrontierBudgetState {
                generated_slices: 1,
                ..FrontierBudgetState::default()
            },
        ));

        add_seen(classify(
            &env,
            &SliceGraphView {
                slices: vec![SliceGraphSlice::generated(
                    "SRC-1",
                    "Parent work",
                    SliceGraphSliceStatus::Open,
                    1,
                )],
                ..SliceGraphView::default()
            },
            &clean_draft("AF-COVER-DEPTH"),
            &budget,
        ));

        add_seen(classify(
            &env,
            &SliceGraphView {
                no_frontier: true,
                ..base_graph.clone()
            },
            &clean_draft("AF-COVER-NO-FRONTIER"),
            &budget,
        ));
        add_seen(classify(
            &env,
            &SliceGraphView {
                cancel_requested: true,
                ..base_graph.clone()
            },
            &clean_draft("AF-COVER-CANCEL"),
            &budget,
        ));
        add_seen(classify(
            &env,
            &SliceGraphView {
                replan_apply_incomplete: true,
                ..base_graph.clone()
            },
            &clean_draft("AF-COVER-APPLY-INCOMPLETE"),
            &budget,
        ));

        let mut missing_acceptance = clean_draft("AF-COVER-NO-ACCEPTANCE");
        missing_acceptance.acceptance.clear();
        add_seen(classify(&env, &base_graph, &missing_acceptance, &budget));

        let mut missing_verify = clean_draft("AF-COVER-NO-VERIFY");
        missing_verify.verify.clear();
        add_seen(classify(&env, &base_graph, &missing_verify, &budget));

        let duplicate_open_graph = SliceGraphView {
            slices: vec![
                SliceGraphSlice::open("SRC-1", "Parent work"),
                SliceGraphSlice::open("AF-COVER-OPEN", "Open duplicate"),
            ],
            ..SliceGraphView::default()
        };
        add_seen(classify(
            &env,
            &duplicate_open_graph,
            &clean_draft("AF-COVER-OPEN"),
            &budget,
        ));

        let duplicate_closed_graph = SliceGraphView {
            slices: vec![
                SliceGraphSlice::open("SRC-1", "Parent work"),
                SliceGraphSlice::closed("AF-COVER-CLOSED", "Closed duplicate"),
            ],
            ..SliceGraphView::default()
        };
        add_seen(classify(
            &env,
            &duplicate_closed_graph,
            &clean_draft("AF-COVER-CLOSED"),
            &budget,
        ));

        let pending = clean_draft("AF-COVER-PENDING");
        let pending_graph = SliceGraphView {
            slices: vec![SliceGraphSlice::open("SRC-1", "Parent work")],
            proposals: vec![ProposalGraphNode::from_draft(
                "rp-pending",
                ReplanProposalState::Pending,
                &pending,
            )],
            ..SliceGraphView::default()
        };
        add_seen(classify(&env, &pending_graph, &pending, &budget));

        let context = clean_draft("AF-COVER-CONTEXT");
        let context_proposal = FollowupProposalView {
            source_slice_id: "SRC-1",
            needs_operator_context: true,
            ..FollowupProposalView::add_followup_slice(&context)
        };
        add_seen(classify_followup_proposal(
            &env,
            &base_graph,
            &context_proposal,
            &budget,
        ));

        let expected: BTreeSet<_> = ReasonCode::ALL.iter().copied().collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn frontier_promotion_policy_tier3_precedence_and_reason_aggregation() {
        let env = envelope();
        let mut draft = clean_draft("AF-PRECEDENCE");
        draft.verify.clear();
        draft.areas = vec!["src/outside.rs".to_string()];
        let decision = classify(&env, &graph(), &draft, &FrontierBudgetState::default());
        assert_decision_contains(
            &decision,
            Tier::Tier3,
            &[
                ReasonCode::AreaOutsideEnvelope,
                ReasonCode::CandidateMissingVerify,
            ],
        );

        let rejected = clean_draft("AF-REJECTED-PRECEDENCE");
        let rejected_graph = SliceGraphView {
            slices: vec![SliceGraphSlice::open("SRC-1", "Parent work")],
            proposals: vec![ProposalGraphNode::from_draft(
                "rp-rejected",
                ReplanProposalState::Rejected,
                &rejected,
            )],
            ..SliceGraphView::default()
        };
        let decision = classify(
            &env,
            &rejected_graph,
            &rejected,
            &FrontierBudgetState::default(),
        );
        assert_decision_contains(
            &decision,
            Tier::Tier3,
            &[ReasonCode::DuplicateRejectedOrDeferredProposal],
        );
    }

    #[test]
    fn frontier_promotion_policy_exact_area_containment_uses_literal_prefixes() {
        let env = envelope();
        let graph = graph();
        let budget = FrontierBudgetState::default();

        let inside = clean_draft("AF-AREA-INSIDE");
        assert_decision_contains(
            &classify(&env, &graph, &inside, &budget),
            Tier::Tier1,
            &[ReasonCode::InsideAllowedAreas],
        );

        let mut no_trailing_slash_candidate = clean_draft("AF-AREA-NO-SLASH");
        no_trailing_slash_candidate.areas = vec!["src/foo".to_string()];
        assert_decision_contains(
            &classify(&env, &graph, &no_trailing_slash_candidate, &budget),
            Tier::Tier3,
            &[ReasonCode::AreaOutsideEnvelope],
        );

        let mut exact_file = clean_draft("AF-AREA-README");
        exact_file.areas = vec!["README.md".to_string()];
        assert_decision_contains(
            &classify(&env, &graph, &exact_file, &budget),
            Tier::Tier1,
            &[ReasonCode::InsideAllowedAreas],
        );

        let mut exact_allowed_env = env.clone();
        exact_allowed_env.allowed_areas = vec!["src/foo".to_string()];
        let mut directory_candidate = clean_draft("AF-AREA-DIRECTORY");
        directory_candidate.areas = vec!["src/foo/".to_string()];
        assert_decision_contains(
            &classify(&exact_allowed_env, &graph, &directory_candidate, &budget),
            Tier::Tier3,
            &[ReasonCode::AreaOutsideEnvelope],
        );

        let mut sibling = clean_draft("AF-AREA-SIBLING");
        sibling.areas = vec!["src/foobar.rs".to_string()];
        assert_decision_contains(
            &classify(&env, &graph, &sibling, &budget),
            Tier::Tier3,
            &[ReasonCode::AreaOutsideEnvelope],
        );
    }

    #[test]
    fn frontier_promotion_policy_budget_and_depth_stops() {
        let env = envelope();
        let graph = graph();
        let draft = clean_draft("AF-STOPS");

        assert_decision_contains(
            &classify(
                &env,
                &graph,
                &draft,
                &FrontierBudgetState {
                    auto_promotions_used: 1,
                    ..FrontierBudgetState::default()
                },
            ),
            Tier::Stop,
            &[ReasonCode::FrontierBudgetExhausted],
        );
        assert_decision_contains(
            &classify(
                &env,
                &graph,
                &draft,
                &FrontierBudgetState {
                    generated_slices: 1,
                    ..FrontierBudgetState::default()
                },
            ),
            Tier::Stop,
            &[ReasonCode::FrontierBudgetExhausted],
        );

        let depth_graph = SliceGraphView {
            slices: vec![SliceGraphSlice::generated(
                "SRC-1",
                "Parent work",
                SliceGraphSliceStatus::Open,
                1,
            )],
            ..SliceGraphView::default()
        };
        assert_decision_contains(
            &classify(
                &env,
                &depth_graph,
                &clean_draft("AF-DEPTH-PARENT"),
                &FrontierBudgetState::default(),
            ),
            Tier::Stop,
            &[ReasonCode::FrontierDepthExhausted],
        );
        assert_decision_contains(
            &classify(
                &env,
                &graph,
                &clean_draft("AF-DEPTH-FLAG"),
                &FrontierBudgetState {
                    max_generation_reached: true,
                    ..FrontierBudgetState::default()
                },
            ),
            Tier::Stop,
            &[ReasonCode::FrontierDepthExhausted],
        );
    }

    #[test]
    fn frontier_promotion_policy_duplicate_states_match_rfc() {
        let env = envelope();
        let budget = FrontierBudgetState::default();

        let open = clean_draft("AF-DUP-OPEN");
        let open_graph = SliceGraphView {
            slices: vec![
                SliceGraphSlice::open("SRC-1", "Parent work"),
                SliceGraphSlice::open("AF-DUP-OPEN", "Different goal"),
            ],
            ..SliceGraphView::default()
        };
        assert_decision_contains(
            &classify(&env, &open_graph, &open, &budget),
            Tier::Tier2,
            &[ReasonCode::DuplicateOpenSlice],
        );

        let closed = clean_draft("AF-DUP-CLOSED-CANDIDATE");
        let closed_graph = SliceGraphView {
            slices: vec![
                SliceGraphSlice::open("SRC-1", "Parent work"),
                SliceGraphSlice::closed("AF-DUP-CLOSED", "  add   bounded FOLLOW-up behavior "),
            ],
            ..SliceGraphView::default()
        };
        assert_decision_contains(
            &classify(&env, &closed_graph, &closed, &budget),
            Tier::Tier2,
            &[ReasonCode::DuplicateClosedSlice],
        );

        let pending = clean_draft("AF-DUP-PENDING");
        let pending_graph = SliceGraphView {
            slices: vec![SliceGraphSlice::open("SRC-1", "Parent work")],
            proposals: vec![ProposalGraphNode::from_draft(
                "rp-pending",
                ReplanProposalState::Pending,
                &pending,
            )],
            ..SliceGraphView::default()
        };
        assert_decision_contains(
            &classify(&env, &pending_graph, &pending, &budget),
            Tier::Tier2,
            &[ReasonCode::DuplicatePendingProposal],
        );

        for state in [ReplanProposalState::Rejected, ReplanProposalState::Deferred] {
            let draft = clean_draft(match state {
                ReplanProposalState::Rejected => "AF-DUP-REJECTED",
                ReplanProposalState::Deferred => "AF-DUP-DEFERRED",
                _ => unreachable!(),
            });
            let rejected_or_deferred_graph = SliceGraphView {
                slices: vec![SliceGraphSlice::open("SRC-1", "Parent work")],
                proposals: vec![ProposalGraphNode::from_draft("rp-old", state, &draft)],
                ..SliceGraphView::default()
            };
            let decision = classify(&env, &rejected_or_deferred_graph, &draft, &budget);
            assert_decision_contains(
                &decision,
                Tier::Tier3,
                &[ReasonCode::DuplicateRejectedOrDeferredProposal],
            );
            assert_ne!(
                decision.tier,
                Tier::Tier1,
                "{state:?} must never auto-promote"
            );
        }
    }

    #[test]
    fn frontier_promotion_policy_no_panic_for_arbitraryish_inputs() {
        let weird_inputs = [
            "",
            " ",
            "./bad",
            "../bad",
            "/absolute",
            "src/*.rs",
            "src/foo",
            "src/foo/",
            "README.md",
            "emoji-☃",
            "line\nbreak",
        ];

        for (index, value) in weird_inputs.iter().enumerate() {
            let env = MissionEnvelope {
                goal: value.to_string(),
                allowed_areas: vec![value.to_string()],
                non_goals: vec![value.to_string()],
                verify_profile: value.to_string(),
                max_auto_promotions: index as i64 % 2,
                max_depth: index as i64 % 2,
                max_generated_slices: index as i64 % 2,
                autonomy_level: if index % 2 == 0 {
                    AutonomyLevel::Promote
                } else {
                    AutonomyLevel::Shadow
                },
                must_ask_if: vec![value.to_string()],
            };
            let draft = FollowupSliceDraft {
                id: value.to_string(),
                title: value.to_string(),
                goal: value.to_string(),
                areas: vec![value.to_string()],
                acceptance: vec![value.to_string()],
                verify: vec![value.to_string()],
                verify_profile: value.to_string(),
                depends_on: vec![value.to_string()],
                must_ask_if: vec![value.to_string()],
                rationale: value.to_string(),
            };
            let graph = SliceGraphView {
                slices: vec![SliceGraphSlice::generated(
                    value.to_string(),
                    value.to_string(),
                    SliceGraphSliceStatus::Open,
                    i64::MAX,
                )],
                proposals: vec![ProposalGraphNode::from_draft(
                    "rp-weird",
                    ReplanProposalState::Pending,
                    &draft,
                )],
                no_frontier: index % 3 == 0,
                cancel_requested: index % 5 == 0,
                replan_apply_incomplete: index % 7 == 0,
            };
            let proposal = FollowupProposalView {
                proposal_id: "rp-current",
                source_slice_id: value,
                ambiguity_markers: if index % 2 == 0 { &["api"] } else { &[] },
                ..FollowupProposalView::add_followup_slice(&draft)
            };
            let result = std::panic::catch_unwind(|| {
                classify_followup_proposal(
                    &env,
                    &graph,
                    &proposal,
                    &FrontierBudgetState {
                        auto_promotions_used: index as i64,
                        generated_slices: index as i64,
                        max_generation_reached: index % 4 == 0,
                    },
                )
            });
            let decision = result.expect("classifier must not panic");
            assert!(
                !decision.reason_codes.is_empty(),
                "classifier must always return at least one reason code"
            );
        }
    }
}
