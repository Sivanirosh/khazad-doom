pub(crate) mod attention;
mod cockpit;
mod economics;
pub(crate) mod events;
pub(crate) mod frontier;
mod gate;
mod manager;
mod projection;
mod prompts;
pub(crate) mod read_model;
mod schema;
mod shell;

use crate::agent::CancellationToken;
use anyhow::Result;
use std::error::Error;
use std::fmt;

pub(crate) use cockpit::{
    CockpitOpenFocus, cockpit_mode_transport_arg, open_default_run_cockpit_for_operator,
    workspace_label_for_run as cockpit_workspace_label_for_run,
};
pub use manager::{GithubImportOptions, Manager, ResumeOptions, SliceDraft, StartOptions};
pub(crate) use projection::project_gate_pane;
pub use prompts::{
    integration_repair_prompt, slice_repair_prompt, worker_envelope_retry_prompt, worker_prompt,
};
pub(crate) use read_model::{RunReadModel, RunReadModelBuilder, RunReadModelOptions};
pub use schema::{REPAIR_RESULT_SCHEMA, WORKER_RESULT_SCHEMA};

pub(crate) fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(CancelledError::new("run cancelled").into())
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct CancelledError {
    reason: String,
}

impl CancelledError {
    pub(crate) fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl fmt::Display for CancelledError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl Error for CancelledError {}
