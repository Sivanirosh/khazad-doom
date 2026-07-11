use crate::domain::{
    AutonomyLevel, FrontierBudgetState, FrontierClassification, GateResult, IntegrationMergeIntent,
    MissionEnvelope, RepairResult, ReplanDecision, ReplanProposal, ReplanProposalSource,
    ReplanProposalState, ReplanProposedChange, Run, RunLaunchIntent, RunStatus,
    WorkerProfileEvidence, WorkerQuestion, WorkflowExitStates,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

pub(crate) const ATTENTION_FOCUS_SENT: &str = "attention_focus_sent";
pub(crate) const ATTENTION_NOTIFICATION_SENT: &str = "attention_notification_sent";
pub(crate) const CHECKPOINT_WRITTEN: &str = "checkpoint_written";
pub(crate) const COCKPIT_READY: &str = "cockpit_ready";
pub(crate) const COCKPIT_WORKER_READY: &str = "cockpit_worker_ready";
pub(crate) const FRONTIER_AUTO_ACCEPT_RECORDED: &str = "frontier_auto_accept_recorded";
pub(crate) const FRONTIER_CLASSIFIED: &str = "frontier_classified";
pub(crate) const IMPLEMENTATION_SUMMARY: &str = "implementation_summary";
pub(crate) const INTEGRATION_REPAIR_COMPLETED: &str = "integration_repair_completed";
pub(crate) const PARALLEL_LAYER_COMPLETED: &str = "parallel_layer_completed";
pub(crate) const PARALLEL_LAYER_FAILED: &str = "parallel_layer_failed";
pub(crate) const PARALLEL_LAYER_STARTED: &str = "parallel_layer_started";
pub(crate) const RUN_CANCEL_REQUESTED: &str = "run_cancel_requested";
pub(crate) const RUN_CANCELLED: &str = "run_cancelled";
pub(crate) const RUN_COMPLETED: &str = "run_completed";
pub(crate) const RUN_ERROR: &str = "run_error";
pub(crate) const RUN_INCIDENT: &str = "run_incident";
pub(crate) const RUN_STARTED: &str = "run_started";
pub(crate) const REPLAN_PROPOSAL_DECIDED: &str = "replan_proposal_decided";
pub(crate) const REPLAN_CHECKPOINT_BLOCKED: &str = "replan_checkpoint_blocked";
pub(crate) const SLICE_STARTED: &str = "slice_started";
pub(crate) const TERMINAL_NOTIFICATION_SENT: &str = "terminal_notification_sent";
pub(crate) const TERMINAL_NOTIFICATION_SKIPPED: &str = "terminal_notification_skipped";
pub(crate) const TERMINAL_SUMMARY_WRITTEN: &str = "terminal_summary_written";
pub(crate) const WORKER_ATTEMPT_FAILURE: &str = "worker_attempt_failure";
pub(crate) const WORKER_ATTEMPT_TIMEOUT: &str = "worker_attempt_timeout";
pub(crate) const WORKER_ERROR: &str = "worker_error";
pub(crate) const WORKER_ENVELOPE_RETRY_SUCCEEDED: &str = "worker_envelope_retry_succeeded";
pub(crate) const WORKER_QUESTION_ANSWERED: &str = "worker_question_answered";
pub(crate) const WORKER_QUESTION_ASKED: &str = "worker_question_asked";
pub(crate) const WORKTREES_CLEANED: &str = "worktrees_cleaned";
pub(crate) const ATTENTION_FOCUS_FAILED: &str = "attention_focus_failed";
pub(crate) const ATTENTION_NOTIFICATION_FAILED: &str = "attention_notification_failed";
pub(crate) const ATTENTION_NOTIFICATION_RECORD_FAILED: &str =
    "attention_notification_record_failed";
pub(crate) const CANDIDATE_FOLLOWUP_SLICE_REPLAN_PROPOSAL_CREATED: &str =
    "candidate_followup_slice_replan_proposal_created";
pub(crate) const COCKPIT_WORKER_RENAMED: &str = "cockpit_worker_renamed";
pub(crate) const COMPLETION_PUBLICATION_COMMITTED: &str = "completion_publication_committed";
pub(crate) const DAEMON_RECOVERY_CLEANUP_ERROR: &str = "daemon_recovery_cleanup_error";
pub(crate) const DAEMON_RECOVERY_COMPLETED: &str = "daemon_recovery_completed";
pub(crate) const DAEMON_RECOVERY_STARTED: &str = "daemon_recovery_started";
pub(crate) const FINDING_REPLAN_PROPOSAL_CREATED: &str = "finding_replan_proposal_created";
pub(crate) const FRONTIER_AUTO_ACCEPT_SKIPPED: &str = "frontier_auto_accept_skipped";
pub(crate) const FRONTIER_AUTO_ACCEPT_STOPPED: &str = "frontier_auto_accept_stopped";
pub(crate) const FRONTIER_SLICE_PROMOTED: &str = "frontier_slice_promoted";
pub(crate) const INTEGRATION_GATE_CANCELLED: &str = "integration_gate_cancelled";
pub(crate) const INTEGRATION_MERGE_APPLIED: &str = "integration_merge_applied";
pub(crate) const INTEGRATION_MERGE_PREPARED: &str = "integration_merge_prepared";
pub(crate) const INTEGRATION_MERGE_RECONCILED: &str = "integration_merge_reconciled";
pub(crate) const INTEGRATION_MERGE_REPREPARED: &str = "integration_merge_reprepared";
pub(crate) const INVALID_WORKER_OUTPUT: &str = "invalid_worker_output";
pub(crate) const MISSION_ENVELOPE_RECORDED: &str = "mission_envelope_recorded";
pub(crate) const ORIGIN_NOTIFICATION_TARGET_RECORDED: &str = "origin_notification_target_recorded";
pub(crate) const PROGRESS: &str = "progress";
pub(crate) const REPLAN_APPLY_COMPLETED: &str = "replan_apply_completed";
pub(crate) const REPLAN_APPLY_INCOMPLETE: &str = "replan_apply_incomplete";
pub(crate) const REPLAN_APPLY_REFUSED: &str = "replan_apply_refused";
pub(crate) const REPLAN_APPLY_STARTED: &str = "replan_apply_started";
pub(crate) const REPLAN_PROPOSAL_CREATED: &str = "replan_proposal_created";
pub(crate) const REPAIR_AUTHORITY_PROPOSAL_CREATED: &str = "repair_authority_proposal_created";
pub(crate) const RUN_LAUNCH_ACTIVATED: &str = "run_launch_activated";
pub(crate) const RUN_LAUNCH_COMPENSATED: &str = "run_launch_compensated";
pub(crate) const RUN_LAUNCH_COMPENSATION_FAILED: &str = "run_launch_compensation_failed";
pub(crate) const RUN_LAUNCH_COMPLETED: &str = "run_launch_completed";
pub(crate) const RUN_LAUNCH_FAILED: &str = "run_launch_failed";
pub(crate) const RUN_LAUNCH_INTEGRATION_RESOURCES_CREATED: &str =
    "run_launch_integration_resources_created";
pub(crate) const RUN_LAUNCH_INTERRUPTED: &str = "run_launch_interrupted";
pub(crate) const RUN_LAUNCH_PREPARED: &str = "run_launch_prepared";
pub(crate) const RUN_LAUNCH_TRANSITIONED: &str = "run_launch_transitioned";
pub(crate) const RUN_RESUMED: &str = "run_resumed";
pub(crate) const SLICE_MERGE_CONFLICT: &str = "slice_merge_conflict";
pub(crate) const SLICE_MERGED: &str = "slice_merged";
pub(crate) const SLICE_REPAIR_COMPLETED: &str = "slice_repair_completed";
pub(crate) const TERMINAL_TRANSITION_INTENDED: &str = "terminal_transition_intended";
pub(crate) const WORKER_ATTEMPT_ALLOCATED: &str = "worker_attempt_allocated";
pub(crate) const WORKER_ATTEMPT_FINISHED: &str = "worker_attempt_finished";
pub(crate) const WORKER_ATTEMPT_INTERRUPTED: &str = "worker_attempt_interrupted";
pub(crate) const WORKER_ATTEMPT_LAUNCHED: &str = "worker_attempt_launched";
pub(crate) const WORKER_QUESTIONS_INTERRUPTED: &str = "worker_questions_interrupted";
pub(crate) const WORKER_QUESTION_INTERRUPTED: &str = "worker_question_interrupted";
pub(crate) const WORKTREE_CLEANUP_ERROR: &str = "worktree_cleanup_error";
pub(crate) const WORKTREE_SETUP_COMPLETED: &str = "worktree_setup_completed";
pub(crate) const WORKTREE_SETUP_FAILED: &str = "worktree_setup_failed";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EventKind {
    AttentionFocusSent,
    AttentionNotificationSent,
    CheckpointWritten,
    CockpitReady,
    CockpitWorkerReady,
    FrontierAutoAcceptRecorded,
    FrontierClassified,
    ImplementationSummary,
    IntegrationRepairCompleted,
    ParallelLayerCompleted,
    ParallelLayerFailed,
    ParallelLayerStarted,
    RunCancelRequested,
    RunCancelled,
    RunCompleted,
    RunError,
    RunIncident,
    RunStarted,
    ReplanProposalDecided,
    ReplanCheckpointBlocked,
    SliceStarted,
    TerminalNotificationSent,
    TerminalNotificationSkipped,
    TerminalSummaryWritten,
    WorkerAttemptFailure,
    WorkerAttemptTimeout,
    WorkerError,
    WorkerEnvelopeRetrySucceeded,
    WorkerQuestionAnswered,
    WorkerQuestionAsked,
    WorktreesCleaned,
    AttentionFocusFailed,
    AttentionNotificationFailed,
    AttentionNotificationRecordFailed,
    CandidateFollowupSliceReplanProposalCreated,
    CockpitWorkerRenamed,
    CompletionPublicationCommitted,
    DaemonRecoveryCleanupError,
    DaemonRecoveryCompleted,
    DaemonRecoveryStarted,
    FindingReplanProposalCreated,
    FrontierAutoAcceptSkipped,
    FrontierAutoAcceptStopped,
    FrontierSlicePromoted,
    IntegrationGateCancelled,
    IntegrationMergeApplied,
    IntegrationMergePrepared,
    IntegrationMergeReconciled,
    IntegrationMergeReprepared,
    InvalidWorkerOutput,
    MissionEnvelopeRecorded,
    OriginNotificationTargetRecorded,
    Progress,
    ReplanApplyCompleted,
    ReplanApplyIncomplete,
    ReplanApplyRefused,
    ReplanApplyStarted,
    ReplanProposalCreated,
    RepairAuthorityProposalCreated,
    RunLaunchActivated,
    RunLaunchCompensated,
    RunLaunchCompensationFailed,
    RunLaunchCompleted,
    RunLaunchFailed,
    RunLaunchIntegrationResourcesCreated,
    RunLaunchInterrupted,
    RunLaunchPrepared,
    RunLaunchTransitioned,
    RunResumed,
    SliceMergeConflict,
    SliceMerged,
    SliceRepairCompleted,
    TerminalTransitionIntended,
    WorkerAttemptAllocated,
    WorkerAttemptFinished,
    WorkerAttemptInterrupted,
    WorkerAttemptLaunched,
    WorkerQuestionsInterrupted,
    WorkerQuestionInterrupted,
    WorktreeCleanupError,
    WorktreeSetupCompleted,
    WorktreeSetupFailed,
    Unknown(String),
}

impl EventKind {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::AttentionFocusSent => ATTENTION_FOCUS_SENT,
            Self::AttentionNotificationSent => ATTENTION_NOTIFICATION_SENT,
            Self::CheckpointWritten => CHECKPOINT_WRITTEN,
            Self::CockpitReady => COCKPIT_READY,
            Self::CockpitWorkerReady => COCKPIT_WORKER_READY,
            Self::FrontierAutoAcceptRecorded => FRONTIER_AUTO_ACCEPT_RECORDED,
            Self::FrontierClassified => FRONTIER_CLASSIFIED,
            Self::ImplementationSummary => IMPLEMENTATION_SUMMARY,
            Self::IntegrationRepairCompleted => INTEGRATION_REPAIR_COMPLETED,
            Self::ParallelLayerCompleted => PARALLEL_LAYER_COMPLETED,
            Self::ParallelLayerFailed => PARALLEL_LAYER_FAILED,
            Self::ParallelLayerStarted => PARALLEL_LAYER_STARTED,
            Self::RunCancelRequested => RUN_CANCEL_REQUESTED,
            Self::RunCancelled => RUN_CANCELLED,
            Self::RunCompleted => RUN_COMPLETED,
            Self::RunError => RUN_ERROR,
            Self::RunIncident => RUN_INCIDENT,
            Self::RunStarted => RUN_STARTED,
            Self::ReplanProposalDecided => REPLAN_PROPOSAL_DECIDED,
            Self::ReplanCheckpointBlocked => REPLAN_CHECKPOINT_BLOCKED,
            Self::SliceStarted => SLICE_STARTED,
            Self::TerminalNotificationSent => TERMINAL_NOTIFICATION_SENT,
            Self::TerminalNotificationSkipped => TERMINAL_NOTIFICATION_SKIPPED,
            Self::TerminalSummaryWritten => TERMINAL_SUMMARY_WRITTEN,
            Self::WorkerAttemptFailure => WORKER_ATTEMPT_FAILURE,
            Self::WorkerAttemptTimeout => WORKER_ATTEMPT_TIMEOUT,
            Self::WorkerError => WORKER_ERROR,
            Self::WorkerEnvelopeRetrySucceeded => WORKER_ENVELOPE_RETRY_SUCCEEDED,
            Self::WorkerQuestionAnswered => WORKER_QUESTION_ANSWERED,
            Self::WorkerQuestionAsked => WORKER_QUESTION_ASKED,
            Self::WorktreesCleaned => WORKTREES_CLEANED,
            Self::AttentionFocusFailed => ATTENTION_FOCUS_FAILED,
            Self::AttentionNotificationFailed => ATTENTION_NOTIFICATION_FAILED,
            Self::AttentionNotificationRecordFailed => ATTENTION_NOTIFICATION_RECORD_FAILED,
            Self::CandidateFollowupSliceReplanProposalCreated => {
                CANDIDATE_FOLLOWUP_SLICE_REPLAN_PROPOSAL_CREATED
            }
            Self::CockpitWorkerRenamed => COCKPIT_WORKER_RENAMED,
            Self::CompletionPublicationCommitted => COMPLETION_PUBLICATION_COMMITTED,
            Self::DaemonRecoveryCleanupError => DAEMON_RECOVERY_CLEANUP_ERROR,
            Self::DaemonRecoveryCompleted => DAEMON_RECOVERY_COMPLETED,
            Self::DaemonRecoveryStarted => DAEMON_RECOVERY_STARTED,
            Self::FindingReplanProposalCreated => FINDING_REPLAN_PROPOSAL_CREATED,
            Self::FrontierAutoAcceptSkipped => FRONTIER_AUTO_ACCEPT_SKIPPED,
            Self::FrontierAutoAcceptStopped => FRONTIER_AUTO_ACCEPT_STOPPED,
            Self::FrontierSlicePromoted => FRONTIER_SLICE_PROMOTED,
            Self::IntegrationGateCancelled => INTEGRATION_GATE_CANCELLED,
            Self::IntegrationMergeApplied => INTEGRATION_MERGE_APPLIED,
            Self::IntegrationMergePrepared => INTEGRATION_MERGE_PREPARED,
            Self::IntegrationMergeReconciled => INTEGRATION_MERGE_RECONCILED,
            Self::IntegrationMergeReprepared => INTEGRATION_MERGE_REPREPARED,
            Self::InvalidWorkerOutput => INVALID_WORKER_OUTPUT,
            Self::MissionEnvelopeRecorded => MISSION_ENVELOPE_RECORDED,
            Self::OriginNotificationTargetRecorded => ORIGIN_NOTIFICATION_TARGET_RECORDED,
            Self::Progress => PROGRESS,
            Self::ReplanApplyCompleted => REPLAN_APPLY_COMPLETED,
            Self::ReplanApplyIncomplete => REPLAN_APPLY_INCOMPLETE,
            Self::ReplanApplyRefused => REPLAN_APPLY_REFUSED,
            Self::ReplanApplyStarted => REPLAN_APPLY_STARTED,
            Self::ReplanProposalCreated => REPLAN_PROPOSAL_CREATED,
            Self::RepairAuthorityProposalCreated => REPAIR_AUTHORITY_PROPOSAL_CREATED,
            Self::RunLaunchActivated => RUN_LAUNCH_ACTIVATED,
            Self::RunLaunchCompensated => RUN_LAUNCH_COMPENSATED,
            Self::RunLaunchCompensationFailed => RUN_LAUNCH_COMPENSATION_FAILED,
            Self::RunLaunchCompleted => RUN_LAUNCH_COMPLETED,
            Self::RunLaunchFailed => RUN_LAUNCH_FAILED,
            Self::RunLaunchIntegrationResourcesCreated => RUN_LAUNCH_INTEGRATION_RESOURCES_CREATED,
            Self::RunLaunchInterrupted => RUN_LAUNCH_INTERRUPTED,
            Self::RunLaunchPrepared => RUN_LAUNCH_PREPARED,
            Self::RunLaunchTransitioned => RUN_LAUNCH_TRANSITIONED,
            Self::RunResumed => RUN_RESUMED,
            Self::SliceMergeConflict => SLICE_MERGE_CONFLICT,
            Self::SliceMerged => SLICE_MERGED,
            Self::SliceRepairCompleted => SLICE_REPAIR_COMPLETED,
            Self::TerminalTransitionIntended => TERMINAL_TRANSITION_INTENDED,
            Self::WorkerAttemptAllocated => WORKER_ATTEMPT_ALLOCATED,
            Self::WorkerAttemptFinished => WORKER_ATTEMPT_FINISHED,
            Self::WorkerAttemptInterrupted => WORKER_ATTEMPT_INTERRUPTED,
            Self::WorkerAttemptLaunched => WORKER_ATTEMPT_LAUNCHED,
            Self::WorkerQuestionsInterrupted => WORKER_QUESTIONS_INTERRUPTED,
            Self::WorkerQuestionInterrupted => WORKER_QUESTION_INTERRUPTED,
            Self::WorktreeCleanupError => WORKTREE_CLEANUP_ERROR,
            Self::WorktreeSetupCompleted => WORKTREE_SETUP_COMPLETED,
            Self::WorktreeSetupFailed => WORKTREE_SETUP_FAILED,
            Self::Unknown(value) => value,
        }
    }
}

impl From<&str> for EventKind {
    fn from(value: &str) -> Self {
        match value {
            ATTENTION_FOCUS_SENT => Self::AttentionFocusSent,
            ATTENTION_NOTIFICATION_SENT => Self::AttentionNotificationSent,
            CHECKPOINT_WRITTEN => Self::CheckpointWritten,
            COCKPIT_READY => Self::CockpitReady,
            COCKPIT_WORKER_READY => Self::CockpitWorkerReady,
            FRONTIER_AUTO_ACCEPT_RECORDED => Self::FrontierAutoAcceptRecorded,
            FRONTIER_CLASSIFIED => Self::FrontierClassified,
            IMPLEMENTATION_SUMMARY => Self::ImplementationSummary,
            INTEGRATION_REPAIR_COMPLETED => Self::IntegrationRepairCompleted,
            PARALLEL_LAYER_COMPLETED => Self::ParallelLayerCompleted,
            PARALLEL_LAYER_FAILED => Self::ParallelLayerFailed,
            PARALLEL_LAYER_STARTED => Self::ParallelLayerStarted,
            RUN_CANCEL_REQUESTED => Self::RunCancelRequested,
            RUN_CANCELLED => Self::RunCancelled,
            RUN_COMPLETED => Self::RunCompleted,
            RUN_ERROR => Self::RunError,
            RUN_INCIDENT => Self::RunIncident,
            RUN_STARTED => Self::RunStarted,
            REPLAN_PROPOSAL_DECIDED => Self::ReplanProposalDecided,
            REPLAN_CHECKPOINT_BLOCKED => Self::ReplanCheckpointBlocked,
            SLICE_STARTED => Self::SliceStarted,
            TERMINAL_NOTIFICATION_SENT => Self::TerminalNotificationSent,
            TERMINAL_NOTIFICATION_SKIPPED => Self::TerminalNotificationSkipped,
            TERMINAL_SUMMARY_WRITTEN => Self::TerminalSummaryWritten,
            WORKER_ATTEMPT_FAILURE => Self::WorkerAttemptFailure,
            WORKER_ATTEMPT_TIMEOUT => Self::WorkerAttemptTimeout,
            WORKER_ERROR => Self::WorkerError,
            WORKER_ENVELOPE_RETRY_SUCCEEDED => Self::WorkerEnvelopeRetrySucceeded,
            WORKER_QUESTION_ANSWERED => Self::WorkerQuestionAnswered,
            WORKER_QUESTION_ASKED => Self::WorkerQuestionAsked,
            WORKTREES_CLEANED => Self::WorktreesCleaned,
            ATTENTION_FOCUS_FAILED => Self::AttentionFocusFailed,
            ATTENTION_NOTIFICATION_FAILED => Self::AttentionNotificationFailed,
            ATTENTION_NOTIFICATION_RECORD_FAILED => Self::AttentionNotificationRecordFailed,
            CANDIDATE_FOLLOWUP_SLICE_REPLAN_PROPOSAL_CREATED => {
                Self::CandidateFollowupSliceReplanProposalCreated
            }
            COCKPIT_WORKER_RENAMED => Self::CockpitWorkerRenamed,
            COMPLETION_PUBLICATION_COMMITTED => Self::CompletionPublicationCommitted,
            DAEMON_RECOVERY_CLEANUP_ERROR => Self::DaemonRecoveryCleanupError,
            DAEMON_RECOVERY_COMPLETED => Self::DaemonRecoveryCompleted,
            DAEMON_RECOVERY_STARTED => Self::DaemonRecoveryStarted,
            FINDING_REPLAN_PROPOSAL_CREATED => Self::FindingReplanProposalCreated,
            FRONTIER_AUTO_ACCEPT_SKIPPED => Self::FrontierAutoAcceptSkipped,
            FRONTIER_AUTO_ACCEPT_STOPPED => Self::FrontierAutoAcceptStopped,
            FRONTIER_SLICE_PROMOTED => Self::FrontierSlicePromoted,
            INTEGRATION_GATE_CANCELLED => Self::IntegrationGateCancelled,
            INTEGRATION_MERGE_APPLIED => Self::IntegrationMergeApplied,
            INTEGRATION_MERGE_PREPARED => Self::IntegrationMergePrepared,
            INTEGRATION_MERGE_RECONCILED => Self::IntegrationMergeReconciled,
            INTEGRATION_MERGE_REPREPARED => Self::IntegrationMergeReprepared,
            INVALID_WORKER_OUTPUT => Self::InvalidWorkerOutput,
            MISSION_ENVELOPE_RECORDED => Self::MissionEnvelopeRecorded,
            ORIGIN_NOTIFICATION_TARGET_RECORDED => Self::OriginNotificationTargetRecorded,
            PROGRESS => Self::Progress,
            REPLAN_APPLY_COMPLETED => Self::ReplanApplyCompleted,
            REPLAN_APPLY_INCOMPLETE => Self::ReplanApplyIncomplete,
            REPLAN_APPLY_REFUSED => Self::ReplanApplyRefused,
            REPLAN_APPLY_STARTED => Self::ReplanApplyStarted,
            REPLAN_PROPOSAL_CREATED => Self::ReplanProposalCreated,
            REPAIR_AUTHORITY_PROPOSAL_CREATED => Self::RepairAuthorityProposalCreated,
            RUN_LAUNCH_ACTIVATED => Self::RunLaunchActivated,
            RUN_LAUNCH_COMPENSATED => Self::RunLaunchCompensated,
            RUN_LAUNCH_COMPENSATION_FAILED => Self::RunLaunchCompensationFailed,
            RUN_LAUNCH_COMPLETED => Self::RunLaunchCompleted,
            RUN_LAUNCH_FAILED => Self::RunLaunchFailed,
            RUN_LAUNCH_INTEGRATION_RESOURCES_CREATED => Self::RunLaunchIntegrationResourcesCreated,
            RUN_LAUNCH_INTERRUPTED => Self::RunLaunchInterrupted,
            RUN_LAUNCH_PREPARED => Self::RunLaunchPrepared,
            RUN_LAUNCH_TRANSITIONED => Self::RunLaunchTransitioned,
            RUN_RESUMED => Self::RunResumed,
            SLICE_MERGE_CONFLICT => Self::SliceMergeConflict,
            SLICE_MERGED => Self::SliceMerged,
            SLICE_REPAIR_COMPLETED => Self::SliceRepairCompleted,
            TERMINAL_TRANSITION_INTENDED => Self::TerminalTransitionIntended,
            WORKER_ATTEMPT_ALLOCATED => Self::WorkerAttemptAllocated,
            WORKER_ATTEMPT_FINISHED => Self::WorkerAttemptFinished,
            WORKER_ATTEMPT_INTERRUPTED => Self::WorkerAttemptInterrupted,
            WORKER_ATTEMPT_LAUNCHED => Self::WorkerAttemptLaunched,
            WORKER_QUESTIONS_INTERRUPTED => Self::WorkerQuestionsInterrupted,
            WORKER_QUESTION_INTERRUPTED => Self::WorkerQuestionInterrupted,
            WORKTREE_CLEANUP_ERROR => Self::WorktreeCleanupError,
            WORKTREE_SETUP_COMPLETED => Self::WorktreeSetupCompleted,
            WORKTREE_SETUP_FAILED => Self::WorktreeSetupFailed,
            other => Self::Unknown(other.to_string()),
        }
    }
}

pub(crate) trait TypedEventPayload {
    fn event_kind() -> EventKind;
}

#[derive(Debug, Clone)]
pub(crate) struct WorkflowEvent<T> {
    kind: EventKind,
    payload: T,
}

impl<T> WorkflowEvent<T> {
    pub(crate) fn kind(&self) -> &EventKind {
        &self.kind
    }

    pub(crate) fn payload(&self) -> &T {
        &self.payload
    }
}

impl WorkflowEvent<RunLaunchIntent> {
    fn run_launch(kind: EventKind, payload: RunLaunchIntent) -> Self {
        Self { kind, payload }
    }

    pub(crate) fn run_launch_prepared(payload: RunLaunchIntent) -> Self {
        Self::run_launch(EventKind::RunLaunchPrepared, payload)
    }

    pub(crate) fn run_launch_integration_resources_created(payload: RunLaunchIntent) -> Self {
        Self::run_launch(EventKind::RunLaunchIntegrationResourcesCreated, payload)
    }

    pub(crate) fn run_launch_transitioned(
        target: crate::domain::RunLaunchState,
        payload: RunLaunchIntent,
    ) -> Self {
        let kind = match target {
            crate::domain::RunLaunchState::Activated => EventKind::RunLaunchActivated,
            crate::domain::RunLaunchState::Completed => EventKind::RunLaunchCompleted,
            _ => EventKind::RunLaunchTransitioned,
        };
        Self::run_launch(kind, payload)
    }

    pub(crate) fn run_launch_failed(payload: RunLaunchIntent) -> Self {
        Self::run_launch(EventKind::RunLaunchFailed, payload)
    }

    pub(crate) fn run_launch_compensation(compensated: bool, payload: RunLaunchIntent) -> Self {
        Self::run_launch(
            if compensated {
                EventKind::RunLaunchCompensated
            } else {
                EventKind::RunLaunchCompensationFailed
            },
            payload,
        )
    }
}

impl WorkflowEvent<IntegrationMergeIntent> {
    pub(crate) fn integration_merge_reprepared(payload: IntegrationMergeIntent) -> Self {
        Self {
            kind: EventKind::IntegrationMergeReprepared,
            payload,
        }
    }

    pub(crate) fn integration_merge_prepared(payload: IntegrationMergeIntent) -> Self {
        Self {
            kind: EventKind::IntegrationMergePrepared,
            payload,
        }
    }

    pub(crate) fn integration_merge_applied(payload: IntegrationMergeIntent) -> Self {
        Self {
            kind: EventKind::IntegrationMergeApplied,
            payload,
        }
    }

    pub(crate) fn integration_merge_reconciled(payload: IntegrationMergeIntent) -> Self {
        Self {
            kind: EventKind::IntegrationMergeReconciled,
            payload,
        }
    }
}

impl WorkflowEvent<FrontierAutoAcceptDispositionPayload> {
    pub(crate) fn frontier_auto_accept_skipped(
        payload: FrontierAutoAcceptDispositionPayload,
    ) -> Self {
        Self {
            kind: EventKind::FrontierAutoAcceptSkipped,
            payload,
        }
    }

    pub(crate) fn frontier_auto_accept_stopped(
        payload: FrontierAutoAcceptDispositionPayload,
    ) -> Self {
        Self {
            kind: EventKind::FrontierAutoAcceptStopped,
            payload,
        }
    }
}

impl WorkflowEvent<ReplanApplyFailurePayload> {
    pub(crate) fn replan_apply_refused(payload: ReplanApplyFailurePayload) -> Self {
        Self {
            kind: EventKind::ReplanApplyRefused,
            payload,
        }
    }

    pub(crate) fn replan_apply_incomplete(payload: ReplanApplyFailurePayload) -> Self {
        Self {
            kind: EventKind::ReplanApplyIncomplete,
            payload,
        }
    }
}

impl WorkflowEvent<ReplanProposalEvidencePayload> {
    pub(crate) fn candidate_followup_created(payload: ReplanProposalEvidencePayload) -> Self {
        Self {
            kind: EventKind::CandidateFollowupSliceReplanProposalCreated,
            payload,
        }
    }

    pub(crate) fn finding_replan_created(payload: ReplanProposalEvidencePayload) -> Self {
        Self {
            kind: EventKind::FindingReplanProposalCreated,
            payload,
        }
    }

    pub(crate) fn repair_authority_created(payload: ReplanProposalEvidencePayload) -> Self {
        Self {
            kind: EventKind::RepairAuthorityProposalCreated,
            payload,
        }
    }
}

impl WorkflowEvent<TerminalNotificationPayload> {
    pub(crate) fn terminal_notification_sent(payload: TerminalNotificationPayload) -> Self {
        Self {
            kind: EventKind::TerminalNotificationSent,
            payload,
        }
    }

    pub(crate) fn terminal_notification_skipped(payload: TerminalNotificationPayload) -> Self {
        Self {
            kind: EventKind::TerminalNotificationSkipped,
            payload,
        }
    }
}

impl WorkflowEvent<WorktreeSetupPayload> {
    pub(crate) fn worktree_setup_completed(payload: WorktreeSetupPayload) -> Self {
        Self {
            kind: EventKind::WorktreeSetupCompleted,
            payload,
        }
    }

    pub(crate) fn worktree_setup_failed(payload: WorktreeSetupPayload) -> Self {
        Self {
            kind: EventKind::WorktreeSetupFailed,
            payload,
        }
    }
}

impl WorkflowEvent<ParallelLayerPayload> {
    pub(crate) fn parallel_layer_started(payload: ParallelLayerPayload) -> Self {
        Self {
            kind: EventKind::ParallelLayerStarted,
            payload,
        }
    }

    pub(crate) fn parallel_layer_failed(payload: ParallelLayerPayload) -> Self {
        Self {
            kind: EventKind::ParallelLayerFailed,
            payload,
        }
    }

    pub(crate) fn parallel_layer_completed(payload: ParallelLayerPayload) -> Self {
        Self {
            kind: EventKind::ParallelLayerCompleted,
            payload,
        }
    }
}

impl WorkflowEvent<AttentionDeliveryPayload> {
    pub(crate) fn attention_notification_sent(payload: AttentionDeliveryPayload) -> Self {
        Self {
            kind: EventKind::AttentionNotificationSent,
            payload,
        }
    }

    pub(crate) fn attention_focus_sent(payload: AttentionDeliveryPayload) -> Self {
        Self {
            kind: EventKind::AttentionFocusSent,
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct OriginNotificationTargetRecordedPayload {
    pub path: String,
    pub target_kind: String,
    pub delivery_adapter: String,
    pub delivery_surface: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MissionEnvelopeRecordedPayload {
    pub mission_envelope: MissionEnvelope,
    pub frontier_budget: Option<FrontierBudgetState>,
    pub autonomy_effective: String,
    pub authority: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunStartedPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<Run>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub selected_slices: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_closed_slices: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verify_profile: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify_profiles: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_profile: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_provider: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_reasoning: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_mode: String,
    #[serde(default, skip_serializing_if = "WorkerProfileEvidence::is_empty")]
    pub worker_profile: WorkerProfileEvidence,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worker_evidence_kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worker_evidence_label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub profile_summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub launch_summary: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub profile_source_attribution: BTreeMap<String, String>,
}

impl RunStartedPayload {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        run: &Run,
        selected_slices: Vec<String>,
        skipped_closed_slices: Vec<String>,
        verify_profile: String,
        verify_profiles: Vec<String>,
        agent: impl Into<String>,
        agent_profile: impl Into<String>,
        agent_provider: impl Into<String>,
        agent_model: impl Into<String>,
        agent_reasoning: impl Into<String>,
        agent_mode: impl Into<String>,
        worker_profile: WorkerProfileEvidence,
        profile_summary: impl Into<String>,
        launch_summary: impl Into<String>,
        profile_source_attribution: BTreeMap<String, String>,
    ) -> Self {
        let worker_evidence_kind = worker_profile.worker_evidence_kind.clone();
        let worker_evidence_label = worker_profile.worker_evidence_label.clone();
        Self {
            run: Some(run.clone()),
            selected_slices,
            skipped_closed_slices,
            verify_profile,
            verify_profiles,
            agent: agent.into(),
            agent_profile: agent_profile.into(),
            agent_provider: agent_provider.into(),
            agent_model: agent_model.into(),
            agent_reasoning: agent_reasoning.into(),
            agent_mode: agent_mode.into(),
            worker_profile,
            worker_evidence_kind,
            worker_evidence_label,
            profile_summary: profile_summary.into(),
            launch_summary: launch_summary.into(),
            profile_source_attribution,
        }
    }

    pub(crate) fn from_value(value: &Value) -> Self {
        decode_or_default(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunIncidentPayload {
    #[serde(default = "default_warning", skip_serializing_if = "String::is_empty")]
    pub severity: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failure_kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolution_owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_action_required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fix_commands: Vec<String>,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

impl RunIncidentPayload {
    pub(crate) fn warning(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: "warning".to_string(),
            kind: kind.into(),
            message: message.into(),
            ..Self::default()
        }
    }

    pub(crate) fn error(kind: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: "error".to_string(),
            kind: kind.into(),
            failure_kind: String::new(),
            message: message.into(),
            ..Self::default()
        }
    }

    pub(crate) fn with_extra<T: Serialize>(mut self, key: impl Into<String>, value: T) -> Self {
        self.extra.insert(
            key.into(),
            serde_json::to_value(value).unwrap_or(Value::Null),
        );
        self
    }

    pub(crate) fn with_severity(mut self, value: impl Into<String>) -> Self {
        self.severity = value.into();
        self
    }

    pub(crate) fn with_failure_kind(mut self, value: impl Into<String>) -> Self {
        self.failure_kind = value.into();
        self
    }

    pub(crate) fn with_operator_action_required(mut self, value: bool) -> Self {
        self.operator_action_required = Some(value);
        self
    }

    pub(crate) fn with_retryable(mut self, value: bool) -> Self {
        self.retryable = Some(value);
        self
    }

    pub(crate) fn with_fix_commands(mut self, value: Vec<String>) -> Self {
        self.fix_commands = value;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunErrorPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default, flatten, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, Value>,
}

impl TypedEventPayload for RunErrorPayload {
    fn event_kind() -> EventKind {
        EventKind::RunError
    }
}

impl RunErrorPayload {
    pub(crate) fn new(error: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            extra: BTreeMap::new(),
        }
    }

    pub(crate) fn from_value(value: &Value) -> Self {
        let mut payload: Self = decode_or_else(value, || Self::legacy(value));
        if payload.error.trim().is_empty() {
            payload.error = payload
                .extra
                .get("message")
                .and_then(value_to_text)
                .or_else(|| payload.extra.get("summary").and_then(value_to_text))
                .unwrap_or_default();
        }
        payload
    }

    fn legacy(value: &Value) -> Self {
        Self {
            error: text_field(value, &["error", "message", "summary"]).unwrap_or_default(),
            extra: object_extra(value),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunLaunchInterruptedPayload {
    pub state: crate::domain::RunLaunchState,
    pub terminal_status: RunStatus,
    pub primary_cause: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct SliceMergedPayload {
    pub slice_id: String,
    pub commit_sha: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerAttemptAllocatedPayload {
    pub launch_id: i64,
    pub slice_id: String,
    pub launch_ordinal: usize,
    pub execution_epoch: usize,
    pub worker_retry_ordinal: usize,
    pub repair_ordinal: usize,
    pub envelope_retry_ordinal: usize,
    pub kind: String,
    pub state: String,
    pub output_stem: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerAttemptLaunchedPayload {
    pub launch_id: i64,
    pub slice_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerAttemptFinishedPayload {
    pub launch_id: i64,
    pub slice_id: String,
    pub state: String,
    pub failure_cause: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WorkerAttemptInterruptedPayload {
    pub launch_id: i64,
    pub slice_id: String,
    pub reason: String,
    pub prior_state: String,
    pub terminal_status: RunStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TerminalTransitionIntendedPayload {
    pub status: RunStatus,
    pub error: String,
    pub progress_message: String,
    pub question_interruption_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerQuestionInterruptedPayload {
    pub question_id: String,
    pub slice_id: String,
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_attempt: Option<usize>,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_status: Option<RunStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorktreesCleanedPayload {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunResumedPayload {
    pub execution_epoch: usize,
    pub remaining_slices: Vec<String>,
    pub native_pi_tui_worker: bool,
    pub experimental_pi_tui_worker: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DaemonRecoveryStartedPayload {
    pub reason: String,
    pub launch_intent: Option<RunLaunchIntent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerQuestionsInterruptedPayload {
    pub count: usize,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct DaemonRecoveryCompletedPayload {
    pub status: RunStatus,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunCancelRequestedPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub active: bool,
}

impl RunCancelRequestedPayload {
    pub(crate) fn new(reason: impl Into<String>, active: bool) -> Self {
        Self {
            reason: reason.into(),
            active,
        }
    }

    pub(crate) fn from_value(value: &Value) -> Self {
        decode_or_else(value, || Self {
            reason: text_field(value, &["reason"]).unwrap_or_default(),
            active: bool_field(value, "active").unwrap_or(false),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunCancelledPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

impl RunCancelledPayload {
    pub(crate) fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct RunCompletedPayload {
    pub run_id: String,
}

impl RunCompletedPayload {
    pub(crate) fn new(run_id: impl Into<String>) -> Self {
        Self {
            run_id: run_id.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct CockpitReadyPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub adapter: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
    #[serde(
        default,
        alias = "workspace_label",
        skip_serializing_if = "String::is_empty"
    )]
    pub workspace: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub panes: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_of_truth: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub planner: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct CockpitWorkerReadyPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub adapter: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mode: String,
    #[serde(
        default,
        alias = "workspace_label",
        skip_serializing_if = "String::is_empty"
    )]
    pub workspace: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pane: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub pane_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub launch_stem: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub launch_identity: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub layout_planner: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worker_slot_name: String,
    #[serde(default)]
    pub worker_slot_index: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub worker_region: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_of_truth: String,
}

impl CockpitWorkerReadyPayload {
    pub(crate) fn from_value(value: &Value) -> Self {
        let mut payload: Self = decode_or_else(value, || Self {
            adapter: text_field(value, &["adapter"]).unwrap_or_else(|| "cockpit".to_string()),
            mode: text_field(value, &["mode"]).unwrap_or_default(),
            workspace: text_field(value, &["workspace", "workspace_label"]).unwrap_or_default(),
            pane: text_field(value, &["pane"]).unwrap_or_default(),
            pane_id: text_field(value, &["pane_id"]).unwrap_or_default(),
            slice_id: text_field(value, &["slice_id"]).unwrap_or_else(|| "slice".to_string()),
            attempt: usize_field(value, "attempt"),
            launch_id: value.get("launch_id").and_then(Value::as_i64),
            launch_stem: text_field(value, &["launch_stem"]).unwrap_or_default(),
            launch_identity: usize_field(value, "launch_identity"),
            terminal_id: text_field(value, &["terminal_id"]).unwrap_or_default(),
            agent_name: text_field(value, &["agent_name"]).unwrap_or_default(),
            layout_planner: text_field(value, &["layout_planner"]).unwrap_or_default(),
            worker_slot_name: text_field(value, &["worker_slot_name"]).unwrap_or_default(),
            worker_slot_index: usize_field(value, "worker_slot_index"),
            worker_region: text_field(value, &["worker_region"]).unwrap_or_default(),
            source_of_truth: text_field(value, &["source_of_truth"]).unwrap_or_default(),
        });
        if payload.adapter.trim().is_empty() {
            payload.adapter = "cockpit".to_string();
        }
        if payload.slice_id.trim().is_empty() {
            payload.slice_id = "slice".to_string();
        }
        payload
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct SliceStartedPayload {
    pub slice_id: String,
}

impl SliceStartedPayload {
    pub(crate) fn new(slice_id: impl Into<String>) -> Self {
        Self {
            slice_id: slice_id.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct IntegrationRepairCompletedPayload {
    pub status: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
}

impl IntegrationRepairCompletedPayload {
    pub(crate) fn new(
        status: impl Into<String>,
        summary: impl Into<String>,
        launch_id: i64,
    ) -> Self {
        Self {
            status: status.into(),
            summary: summary.into(),
            launch_id: Some(launch_id),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct TerminalSummaryWrittenPayload {
    pub path: String,
}

impl TerminalSummaryWrittenPayload {
    pub(crate) fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path_to_string(path),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct TerminalNotificationPayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub terminal_status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transition_key: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub adapter: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub surface: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_kind: String,
}

impl TerminalNotificationPayload {
    pub(crate) fn sent(
        terminal_status: impl Into<String>,
        transition_key: impl Into<String>,
        adapter: impl Into<String>,
        surface: impl Into<String>,
        target_kind: impl Into<String>,
    ) -> Self {
        Self {
            status: terminal_status.into(),
            terminal_status: String::new(),
            transition_key: transition_key.into(),
            reason: String::new(),
            adapter: adapter.into(),
            surface: surface.into(),
            target_kind: target_kind.into(),
        }
    }

    pub(crate) fn skipped(
        terminal_status: impl Into<String>,
        transition_key: impl Into<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            status: terminal_status.into(),
            terminal_status: String::new(),
            transition_key: transition_key.into(),
            reason: reason.into(),
            adapter: String::new(),
            surface: String::new(),
            target_kind: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorktreeSetupPayload {
    pub slice_id: String,
    pub attempt: usize,
    pub worktree: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artifact: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<GateResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorktreeCleanupErrorPayload {
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct InvalidWorkerOutputPayload {
    pub slice_id: String,
    pub attempt: usize,
    pub envelope_retry: usize,
    pub parse_error: String,
    pub artifact_path: String,
    pub expected_output_path: String,
    pub raw_invalid_payload: String,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub assistant_tail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct SliceRepairCompletedPayload {
    pub slice_id: String,
    pub attempt: usize,
    pub repair_attempt: usize,
    pub status: String,
    pub trigger_failure_kind: String,
    pub launch_id: i64,
    pub check_path: String,
    pub output_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerEnvelopeRetrySucceededPayload {
    pub slice_id: String,
    pub attempt: usize,
    pub launch_id: i64,
    pub envelope_retry: usize,
    pub output_path: String,
    pub previous_invalid_output: String,
    pub disposition: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerAttemptTimeoutPayload {
    pub phase: String,
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub timeout_seconds: u64,
    pub message: String,
}

impl WorkerAttemptTimeoutPayload {
    pub(crate) fn new(
        phase: impl Into<String>,
        slice_id: impl Into<String>,
        attempt: usize,
        launch_id: Option<i64>,
        timeout_seconds: u64,
        message: impl Into<String>,
    ) -> Self {
        Self {
            phase: phase.into(),
            slice_id: slice_id.into(),
            attempt,
            launch_id,
            timeout_seconds,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerAttemptFailurePayload {
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub envelope_retry: usize,
    pub phase: String,
    pub failure_kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub evidence_path: String,
    pub retry_disposition: String,
    pub repair_disposition: String,
    #[serde(default)]
    pub primary_failure: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secondary_failures: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerErrorPayload {
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    pub error: String,
    #[serde(default)]
    pub primary_failure: Option<String>,
    #[serde(default)]
    pub secondary_failures: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub failure_kind: String,
    #[serde(default)]
    pub retryable: Option<bool>,
    #[serde(default)]
    pub operator_action_required: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerQuestionAskedPayload {
    pub question_id: String,
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
    pub question: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub timeout_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_at: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recommended_answer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recommendation_rationale: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bounded_within_current_slice_or_mission_authority: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reversible: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub fallback_eligible: bool,
    pub answer_command: String,
}

impl WorkerQuestionAskedPayload {
    pub(crate) fn from_question(question: &WorkerQuestion, deadline_at: Option<String>) -> Self {
        Self {
            question_id: question.id.clone(),
            slice_id: question.slice_id.clone(),
            attempt: question.attempt,
            launch_id: question.launch_id,
            question: question.question.clone(),
            options: question.options.clone(),
            timeout_seconds: question.timeout_seconds,
            deadline_at,
            recommended_answer: question.recommended_answer.clone(),
            recommendation_rationale: question.recommendation_rationale.clone(),
            bounded_within_current_slice_or_mission_authority: question
                .bounded_within_current_slice_or_mission_authority,
            reversible: question.reversible,
            fallback_eligible: question.fallback_eligible,
            answer_command: format!(
                "khazad-doom answer {} {} <answer>",
                question.run_id, question.id
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct WorkerQuestionAnsweredPayload {
    pub question_id: String,
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub answer_source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recommended_answer: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub recommendation_rationale: String,
}

impl WorkerQuestionAnsweredPayload {
    pub(crate) fn from_question(
        question: &WorkerQuestion,
        answer: impl Into<String>,
        answer_source: crate::domain::WorkerQuestionAnswerSource,
    ) -> Self {
        Self {
            question_id: question.id.clone(),
            slice_id: question.slice_id.clone(),
            launch_id: question.launch_id,
            answer: answer.into(),
            answer_source: answer_source.as_str().to_string(),
            recommended_answer: question.recommended_answer.clone(),
            recommendation_rationale: question.recommendation_rationale.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct CockpitWorkerRenamedPayload {
    pub pane_id: String,
    pub slice_id: String,
    pub launch_id: Option<i64>,
    pub launch_stem: String,
    pub status: String,
    pub label: String,
    pub adapter: String,
    pub surface: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct AttentionDeliveryPayload {
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub question_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub proposal_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub adapter: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub surface: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_kind: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ParallelLayerPayload {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slices: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
}

impl ParallelLayerPayload {
    pub(crate) fn started(slices: Vec<String>) -> Self {
        Self {
            slices,
            outcomes: Vec::new(),
            summary: String::new(),
        }
    }

    pub(crate) fn completed(slices: Vec<String>, outcomes: Vec<Value>) -> Self {
        Self {
            slices,
            outcomes,
            summary: String::new(),
        }
    }

    pub(crate) fn failed(
        slices: Vec<String>,
        outcomes: Vec<Value>,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            slices,
            outcomes,
            summary: summary.into(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ImplementationSummaryPayload {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub completed_slices: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_gate: Option<GateResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_repair_integration_gate: Option<GateResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_repair: Option<RepairResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_states: Option<WorkflowExitStates>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub verify_profile: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub final_sha: String,
}

impl ImplementationSummaryPayload {
    pub(crate) fn from_value(value: &Value) -> Self {
        decode_or_else(value, || Self {
            completed_slices: value
                .get("completed_slices")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
            integration_gate: object_field(value, "integration_gate"),
            pre_repair_integration_gate: object_field(value, "pre_repair_integration_gate"),
            integration_repair: object_field(value, "integration_repair"),
            exit_states: object_field(value, "exit_states"),
            verify_profile: text_field(value, &["verify_profile"]).unwrap_or_default(),
            final_sha: text_field(value, &["final_sha"]).unwrap_or_default(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FrontierClassifiedPayload {
    pub proposal_id: String,
    pub checkpoint: String,
    pub tier: String,
    pub reason_codes: Vec<String>,
    pub classified_at: chrono::DateTime<chrono::Utc>,
    pub envelope_hash: String,
    pub budget_snapshot: FrontierBudgetState,
    pub autonomy_level: AutonomyLevel,
    pub record_only: bool,
    pub queue_mutated: bool,
    pub slice_mutated: bool,
    pub decision_recorded: bool,
}

impl FrontierClassifiedPayload {
    pub(crate) fn new(
        proposal_id: impl Into<String>,
        checkpoint: impl Into<String>,
        classification: &FrontierClassification,
        record_only: bool,
        decision_recorded: bool,
    ) -> Self {
        Self {
            proposal_id: proposal_id.into(),
            checkpoint: checkpoint.into(),
            tier: classification.tier.clone(),
            reason_codes: classification.reason_codes.clone(),
            classified_at: classification.classified_at,
            envelope_hash: classification.envelope_hash.clone(),
            budget_snapshot: classification.budget_snapshot.clone(),
            autonomy_level: classification.autonomy_level,
            record_only,
            queue_mutated: false,
            slice_mutated: false,
            decision_recorded,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct FrontierAutoAcceptRecordedPayload {
    pub proposal_id: String,
    pub checkpoint: String,
    pub authorizer: String,
    pub source: String,
    pub rationale: String,
    pub tier: String,
    pub reason_codes: Vec<String>,
    pub budget_before: FrontierBudgetState,
    pub budget_after: FrontierBudgetState,
    pub af00_evidence_gate: String,
    pub apply_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReplanProposalCreatedPayload {
    pub proposal_id: String,
    pub state: ReplanProposalState,
    pub risk: String,
    pub source: ReplanProposalSource,
    pub proposed_changes: Vec<ReplanProposedChange>,
    pub decision_commands: Vec<String>,
}

impl ReplanProposalCreatedPayload {
    pub(crate) fn from_proposal(proposal: &ReplanProposal) -> Self {
        Self {
            proposal_id: proposal.id.clone(),
            state: proposal.state,
            risk: proposal.risk.clone(),
            source: proposal.source.clone(),
            proposed_changes: proposal.proposed_changes.clone(),
            decision_commands: proposal.decision_commands.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FrontierAutoAcceptDispositionPayload {
    pub proposal_id: String,
    pub checkpoint: String,
    pub reason: String,
    pub tier: String,
    pub reason_codes: Vec<String>,
    pub state: ReplanProposalState,
}

impl FrontierAutoAcceptDispositionPayload {
    pub(crate) fn new(
        proposal: &ReplanProposal,
        checkpoint: impl Into<String>,
        reason: impl Into<String>,
        tier: impl Into<String>,
        reason_codes: Vec<String>,
    ) -> Self {
        Self {
            proposal_id: proposal.id.clone(),
            checkpoint: checkpoint.into(),
            reason: reason.into(),
            tier: tier.into(),
            reason_codes,
            state: proposal.state,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ReplanProposalEvidencePayload {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub slice_id: String,
    #[serde(default, skip_serializing_if = "is_zero_usize")]
    pub attempt: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft_index: Option<usize>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub draft_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub finding_id: String,
    pub proposal_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repair_output_path: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repair_base: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub repair_head: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unauthorized_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub policy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReplanApplyStartedPayload {
    pub proposal_id: String,
    pub slice_id: String,
    pub checkpoint: String,
    pub queue_before: Vec<String>,
    pub queue_before_hash: String,
    pub integration_head: String,
    pub apply_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReplanApplyFailurePayload {
    pub proposal_id: String,
    pub slice_id: String,
    pub checkpoint: String,
    pub reason: String,
    pub remediation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct FrontierSlicePromotedPayload {
    pub proposal_id: String,
    pub slice_id: String,
    pub parent_slice_id: String,
    pub generation: u64,
    pub checkpoint: String,
    pub commit_sha: String,
    pub queue_before: Vec<String>,
    pub queue_before_hash: String,
    pub queue_after: Vec<String>,
    pub queue_after_hash: String,
    pub appended: bool,
    pub serial_append: bool,
    pub worker_enqueued: bool,
    pub apply_mode: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReplanProposalDecidedPayload {
    pub proposal_id: String,
    pub state: ReplanProposalState,
    pub decision: ReplanDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ReplanCheckpointBlockedPayload {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposal_ids: Vec<String>,
    pub checkpoint: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_commands: Vec<String>,
}

impl ReplanCheckpointBlockedPayload {
    pub(crate) fn new(
        proposal_ids: Vec<String>,
        checkpoint: impl Into<String>,
        message: impl Into<String>,
        decision_commands: Vec<String>,
    ) -> Self {
        Self {
            proposal_ids,
            checkpoint: checkpoint.into(),
            message: message.into(),
            decision_commands,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ReplanNotificationPayload {
    pub schema_version: u64,
    pub kind: String,
    pub run_id: String,
    pub proposal_id: String,
    pub source: ReplanProposalSource,
    pub risk: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proposed_changes: Vec<ReplanProposedChange>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decision_commands: Vec<String>,
    pub source_of_truth: String,
}

impl ReplanNotificationPayload {
    pub(crate) fn new(
        run_id: impl Into<String>,
        proposal_id: impl Into<String>,
        source: ReplanProposalSource,
        risk: impl Into<String>,
        proposed_changes: Vec<ReplanProposedChange>,
        decision_commands: Vec<String>,
    ) -> Self {
        Self {
            schema_version: 1,
            kind: "replan_decision_pending".to_string(),
            run_id: run_id.into(),
            proposal_id: proposal_id.into(),
            source,
            risk: risk.into(),
            proposed_changes,
            decision_commands,
            source_of_truth: "daemon_replan_proposals".to_string(),
        }
    }
}

macro_rules! impl_typed_event_payload {
    ($payload:ty, $kind:expr) => {
        impl TypedEventPayload for $payload {
            fn event_kind() -> EventKind {
                $kind
            }
        }
    };
}

impl_typed_event_payload!(
    OriginNotificationTargetRecordedPayload,
    EventKind::OriginNotificationTargetRecorded
);
impl_typed_event_payload!(
    MissionEnvelopeRecordedPayload,
    EventKind::MissionEnvelopeRecorded
);
impl_typed_event_payload!(RunStartedPayload, EventKind::RunStarted);
impl_typed_event_payload!(crate::domain::RunProgress, EventKind::Progress);
impl_typed_event_payload!(crate::domain::RunCheckpoint, EventKind::CheckpointWritten);
impl_typed_event_payload!(
    crate::domain::MergeConflictReport,
    EventKind::SliceMergeConflict
);
impl_typed_event_payload!(RunIncidentPayload, EventKind::RunIncident);
impl_typed_event_payload!(RunLaunchInterruptedPayload, EventKind::RunLaunchInterrupted);
impl_typed_event_payload!(SliceMergedPayload, EventKind::SliceMerged);
impl_typed_event_payload!(
    WorkerAttemptAllocatedPayload,
    EventKind::WorkerAttemptAllocated
);
impl_typed_event_payload!(
    WorkerAttemptLaunchedPayload,
    EventKind::WorkerAttemptLaunched
);
impl_typed_event_payload!(
    WorkerAttemptFinishedPayload,
    EventKind::WorkerAttemptFinished
);
impl_typed_event_payload!(
    WorkerAttemptInterruptedPayload,
    EventKind::WorkerAttemptInterrupted
);
impl_typed_event_payload!(
    TerminalTransitionIntendedPayload,
    EventKind::TerminalTransitionIntended
);
impl_typed_event_payload!(
    WorkerQuestionInterruptedPayload,
    EventKind::WorkerQuestionInterrupted
);
impl_typed_event_payload!(WorktreesCleanedPayload, EventKind::WorktreesCleaned);
impl_typed_event_payload!(RunResumedPayload, EventKind::RunResumed);
impl_typed_event_payload!(
    DaemonRecoveryStartedPayload,
    EventKind::DaemonRecoveryStarted
);
impl_typed_event_payload!(
    DaemonRecoveryCompletedPayload,
    EventKind::DaemonRecoveryCompleted
);
impl_typed_event_payload!(
    WorkerQuestionsInterruptedPayload,
    EventKind::WorkerQuestionsInterrupted
);
impl_typed_event_payload!(RunCancelRequestedPayload, EventKind::RunCancelRequested);
impl_typed_event_payload!(RunCancelledPayload, EventKind::RunCancelled);
impl_typed_event_payload!(RunCompletedPayload, EventKind::RunCompleted);
impl_typed_event_payload!(CockpitReadyPayload, EventKind::CockpitReady);
impl_typed_event_payload!(CockpitWorkerReadyPayload, EventKind::CockpitWorkerReady);
impl_typed_event_payload!(SliceStartedPayload, EventKind::SliceStarted);
impl_typed_event_payload!(
    IntegrationRepairCompletedPayload,
    EventKind::IntegrationRepairCompleted
);
impl_typed_event_payload!(
    TerminalSummaryWrittenPayload,
    EventKind::TerminalSummaryWritten
);
impl_typed_event_payload!(GateResult, EventKind::IntegrationGateCancelled);
impl_typed_event_payload!(
    crate::artifact::CompletionPublicationReceipt,
    EventKind::CompletionPublicationCommitted
);
impl_typed_event_payload!(WorktreeCleanupErrorPayload, EventKind::WorktreeCleanupError);
impl_typed_event_payload!(InvalidWorkerOutputPayload, EventKind::InvalidWorkerOutput);
impl_typed_event_payload!(SliceRepairCompletedPayload, EventKind::SliceRepairCompleted);
impl_typed_event_payload!(
    WorkerEnvelopeRetrySucceededPayload,
    EventKind::WorkerEnvelopeRetrySucceeded
);
impl_typed_event_payload!(WorkerAttemptTimeoutPayload, EventKind::WorkerAttemptTimeout);
impl_typed_event_payload!(WorkerAttemptFailurePayload, EventKind::WorkerAttemptFailure);
impl_typed_event_payload!(WorkerErrorPayload, EventKind::WorkerError);
impl_typed_event_payload!(WorkerQuestionAskedPayload, EventKind::WorkerQuestionAsked);
impl_typed_event_payload!(
    WorkerQuestionAnsweredPayload,
    EventKind::WorkerQuestionAnswered
);
impl_typed_event_payload!(CockpitWorkerRenamedPayload, EventKind::CockpitWorkerRenamed);
impl_typed_event_payload!(
    ImplementationSummaryPayload,
    EventKind::ImplementationSummary
);
impl_typed_event_payload!(
    crate::domain::ImplementationSummary,
    EventKind::ImplementationSummary
);
impl_typed_event_payload!(FrontierClassifiedPayload, EventKind::FrontierClassified);
impl_typed_event_payload!(
    FrontierAutoAcceptRecordedPayload,
    EventKind::FrontierAutoAcceptRecorded
);
impl_typed_event_payload!(
    ReplanProposalCreatedPayload,
    EventKind::ReplanProposalCreated
);
impl_typed_event_payload!(ReplanApplyStartedPayload, EventKind::ReplanApplyStarted);
impl_typed_event_payload!(
    FrontierSlicePromotedPayload,
    EventKind::FrontierSlicePromoted
);
impl_typed_event_payload!(
    ReplanProposalDecidedPayload,
    EventKind::ReplanProposalDecided
);
impl_typed_event_payload!(
    ReplanCheckpointBlockedPayload,
    EventKind::ReplanCheckpointBlocked
);

pub(crate) fn path_to_string(path: impl AsRef<Path>) -> String {
    path.as_ref().to_string_lossy().to_string()
}

fn decode_or_default<T>(value: &Value) -> T
where
    T: DeserializeOwned + Default,
{
    decode_or_else(value, T::default)
}

fn decode_or_else<T, F>(value: &Value, fallback: F) -> T
where
    T: DeserializeOwned,
    F: FnOnce() -> T,
{
    serde_json::from_value(value.clone()).unwrap_or_else(|_| fallback())
}

fn object_field<T>(value: &Value, key: &str) -> Option<T>
where
    T: DeserializeOwned,
{
    value
        .get(key)
        .and_then(|field| serde_json::from_value(field.clone()).ok())
}

fn text_field(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|key| value.get(*key).and_then(value_to_text))
        .find(|text| !text.trim().is_empty())
}

fn value_to_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.trim().to_string());
    }
    if let Some(number) = value.as_i64() {
        return Some(number.to_string());
    }
    if let Some(number) = value.as_u64() {
        return Some(number.to_string());
    }
    if let Some(number) = value.as_f64() {
        return Some(number.to_string());
    }
    if let Some(flag) = value.as_bool() {
        return Some(flag.to_string());
    }
    None
}

fn bool_field(value: &Value, key: &str) -> Option<bool> {
    value.get(key).and_then(Value::as_bool)
}

fn usize_field(value: &Value, key: &str) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or_default()
}
fn object_extra(value: &Value) -> BTreeMap<String, Value> {
    value
        .as_object()
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default()
}
fn default_warning() -> String {
    "warning".to_string()
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_event_vocabulary_preserves_legacy_and_unknown_kinds() {
        assert_eq!(EventKind::from(RUN_ERROR), EventKind::RunError);
        assert_eq!(EventKind::RunError.as_str(), RUN_ERROR);
        assert_eq!(
            EventKind::from("future_event_kind"),
            EventKind::Unknown("future_event_kind".to_string())
        );
    }

    #[test]
    fn typed_event_payload_binds_kind_at_compile_time() {
        let payload = RunErrorPayload::new("boom");
        assert_eq!(RunErrorPayload::event_kind(), EventKind::RunError);
        assert_eq!(payload.error, "boom");
    }
}
