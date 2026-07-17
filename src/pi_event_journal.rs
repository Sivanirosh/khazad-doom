use crate::pi_contract;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};

pub const PI_EVENT_RELAY_ARG: &str = "__pi-event-relay";
pub const WORKER_OUTPUT_LIMIT_FAILURE_KIND: &str = "worker_output_limit_exceeded";
pub const PI_EVENT_JOURNAL_VERSION: u64 = 1;

#[derive(Debug, Clone, Default, Serialize)]
pub struct PiEventJournalStats {
    pub schema_version: u64,
    pub compaction_version: u64,
    pub source_bytes: u64,
    pub source_lines: usize,
    pub stored_bytes: u64,
    pub stored_lines: usize,
    pub compacted_events: usize,
    pub storage_bytes_saved: u64,
    pub source_sha256: String,
    pub stored_sha256: String,
    pub output_limit_bytes: u64,
    pub output_limit_exceeded: bool,
}

pub struct PiEventJournalWriter<W> {
    output: W,
    pending: Vec<u8>,
    max_bytes: u64,
    source_bytes: u64,
    source_lines: usize,
    stored_bytes: u64,
    stored_lines: usize,
    compacted_events: usize,
    storage_bytes_saved: u64,
    source_hash: Sha256,
    stored_hash: Sha256,
    finished: bool,
    output_limit_exceeded: bool,
}

impl<W: Write> PiEventJournalWriter<W> {
    pub fn new(output: W, max_bytes: u64) -> Self {
        Self {
            output,
            pending: Vec::new(),
            max_bytes,
            source_bytes: 0,
            source_lines: 0,
            stored_bytes: 0,
            stored_lines: 0,
            compacted_events: 0,
            storage_bytes_saved: 0,
            source_hash: Sha256::new(),
            stored_hash: Sha256::new(),
            finished: false,
            output_limit_exceeded: false,
        }
    }

    pub fn ingest(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.finished {
            return Err(io::Error::other("Pi event journal already finished"));
        }
        self.source_bytes = self.source_bytes.saturating_add(bytes.len() as u64);
        self.source_hash.update(bytes);
        self.source_lines = self
            .source_lines
            .saturating_add(bytes.iter().filter(|byte| **byte == b'\n').count());
        let mut remaining = bytes;
        while let Some(newline) = remaining.iter().position(|byte| *byte == b'\n') {
            self.extend_pending(&remaining[..newline])?;
            self.write_pending_line(true)?;
            remaining = &remaining[newline + 1..];
        }
        self.extend_pending(remaining)
    }

    pub fn finish(&mut self) -> io::Result<PiEventJournalStats> {
        if !self.finished {
            if !self.pending.is_empty() {
                self.source_lines = self.source_lines.saturating_add(1);
                self.write_pending_line(false)?;
            }
            self.output.flush()?;
            self.finished = true;
        }
        Ok(self.stats())
    }

    pub fn stats(&self) -> PiEventJournalStats {
        PiEventJournalStats {
            schema_version: 1,
            compaction_version: PI_EVENT_JOURNAL_VERSION,
            source_bytes: self.source_bytes,
            source_lines: self.source_lines,
            stored_bytes: self.stored_bytes,
            stored_lines: self.stored_lines,
            compacted_events: self.compacted_events,
            storage_bytes_saved: self.storage_bytes_saved,
            source_sha256: hex::encode(self.source_hash.clone().finalize()),
            stored_sha256: hex::encode(self.stored_hash.clone().finalize()),
            output_limit_bytes: self.max_bytes,
            output_limit_exceeded: self.output_limit_exceeded,
        }
    }

    fn extend_pending(&mut self, bytes: &[u8]) -> io::Result<()> {
        let line_bytes = self.pending.len().saturating_add(bytes.len()) as u64;
        if line_bytes > self.max_bytes {
            self.output_limit_exceeded = true;
            return Err(output_limit_error(self.max_bytes));
        }
        self.pending.extend_from_slice(bytes);
        Ok(())
    }

    fn write_pending_line(&mut self, terminated: bool) -> io::Result<()> {
        let mut stored = pi_contract::canonical_event_journal_line(&self.pending);
        let compacted = stored.is_some();
        let line = stored.as_deref_mut().unwrap_or(&mut self.pending);
        let terminated_bytes = usize::from(terminated);
        let write_bytes = line.len().saturating_add(terminated_bytes) as u64;
        if self.stored_bytes.saturating_add(write_bytes) > self.max_bytes {
            self.output_limit_exceeded = true;
            return Err(output_limit_error(self.max_bytes));
        }
        self.output.write_all(line)?;
        self.stored_hash.update(&*line);
        if terminated {
            self.output.write_all(b"\n")?;
            self.stored_hash.update(b"\n");
        }
        let source_line_bytes = self.pending.len().saturating_add(terminated_bytes) as u64;
        self.stored_lines = self.stored_lines.saturating_add(1);
        self.stored_bytes = self.stored_bytes.saturating_add(write_bytes);
        self.storage_bytes_saved = self
            .storage_bytes_saved
            .saturating_add(source_line_bytes.saturating_sub(write_bytes));
        if compacted {
            self.compacted_events = self.compacted_events.saturating_add(1);
        }
        self.pending.clear();
        Ok(())
    }
}

fn output_limit_error(max_bytes: u64) -> io::Error {
    io::Error::other(format!(
        "{WORKER_OUTPUT_LIMIT_FAILURE_KIND}: canonical Pi event journal exceeded {max_bytes} bytes"
    ))
}

#[derive(Debug)]
pub struct PiEventJournalError {
    source: io::Error,
    stats: Box<PiEventJournalStats>,
}

impl PiEventJournalError {
    pub fn stats(&self) -> &PiEventJournalStats {
        &self.stats
    }
}

impl std::fmt::Display for PiEventJournalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(formatter)
    }
}

impl std::error::Error for PiEventJournalError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

pub fn relay<R: Read, W: Write>(
    mut input: R,
    output: W,
    max_bytes: u64,
) -> Result<PiEventJournalStats, PiEventJournalError> {
    let mut journal = PiEventJournalWriter::new(output, max_bytes);
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let read = input
            .read(&mut chunk)
            .map_err(|source| PiEventJournalError {
                source,
                stats: Box::new(journal.stats()),
            })?;
        if read == 0 {
            break;
        }
        journal
            .ingest(&chunk[..read])
            .map_err(|source| PiEventJournalError {
                source,
                stats: Box::new(journal.stats()),
            })?;
    }
    journal.finish().map_err(|source| PiEventJournalError {
        source,
        stats: Box::new(journal.stats()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn compacts_only_cumulative_message_update_snapshots() {
        let update = json!({
            "type": "message_update",
            "assistantMessageEvent": {"type": "toolcall_delta", "delta": "x", "partial": {"large": "x".repeat(4096)}},
            "message": {"large": "x".repeat(4096)}
        });
        let terminal = json!({"type": "message_end", "message": {"large": "x".repeat(4096)}});
        let malformed = b"{not-json}";
        let mut source = Vec::new();
        for _ in 0..100 {
            writeln!(source, "{update}").unwrap();
        }
        writeln!(source, "{terminal}").unwrap();
        source.extend_from_slice(malformed);
        source.push(b'\n');

        let mut stored = Vec::new();
        let stats = relay(source.as_slice(), &mut stored, 1024 * 1024).unwrap();
        assert!(stored.len() < 16 * 1024);
        assert_eq!(stats.compacted_events, 100);
        assert_eq!(stats.source_lines, 102);
        let lines = String::from_utf8(stored).unwrap();
        let mut parsed = lines.lines();
        let compact: Value = serde_json::from_str(parsed.next().unwrap()).unwrap();
        assert!(compact.get("message").is_none());
        assert!(compact["assistantMessageEvent"].get("partial").is_none());
        let terminal_line = lines.lines().nth(100).unwrap();
        assert_eq!(terminal_line, terminal.to_string());
        assert_eq!(lines.lines().nth(101).unwrap(), "{not-json}");
    }

    #[test]
    fn rejects_before_writing_a_line_that_exceeds_the_budget() {
        let source = format!(
            "{}\n",
            json!({"type": "message_end", "message": "x".repeat(1024)})
        );
        let mut stored = Vec::new();
        let error = relay(source.as_bytes(), &mut stored, 128).unwrap_err();
        assert!(error.to_string().contains(WORKER_OUTPUT_LIMIT_FAILURE_KIND));
        assert!(stored.is_empty());
    }
}
