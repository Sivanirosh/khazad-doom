mod economics;
mod gate;
mod manager;
mod projection;
mod prompts;
mod schema;
mod shell;

use crate::agent::CancellationToken;
use anyhow::Result;
use std::error::Error;
use std::fmt;

pub use manager::{GithubImportOptions, Manager, ResumeOptions, SliceDraft, StartOptions};
pub(crate) use projection::project_run;
pub use prompts::{integration_repair_prompt, worker_prompt};
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
