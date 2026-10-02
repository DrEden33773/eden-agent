//! Opt-in, content-free timings for local host and terminal performance diagnosis.
use serde_json::json;
use std::{
    io::Write,
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// A measured operation. No request payloads, paths, credentials or model text are recorded.
pub struct Span {
    path: Option<PathBuf>,
    stage: &'static str,
    session: u64,
    run: u64,
    started: Instant,
    records: usize,
    bytes: usize,
}

impl Span {
    /// Record boundaries only when EDEN_LATENCY_TRACE names a diagnostic JSONL file.
    pub fn new(stage: &'static str, session: u64, run: u64) -> Self {
        let span = Self {
            path: std::env::var_os("EDEN_LATENCY_TRACE").map(PathBuf::from),
            stage,
            session,
            run,
            started: Instant::now(),
            records: 0,
            bytes: 0,
        };
        span.write("start");
        span
    }

    /// Include sizes already available to the operation without serializing extra data.
    pub fn sizes(&mut self, records: usize, bytes: usize) {
        self.records = records;
        self.bytes = bytes;
    }

    fn write(&self, edge: &str) {
        let Some(path) = &self.path else { return };
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        else {
            return;
        };
        let mut line = json!({
            "stage": self.stage,
            "edge": edge,
            "pid": std::process::id(),
            "session": self.session,
            "run": self.run,
            "time_ns": SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|time| time.as_nanos())
                .unwrap_or_default(),
            "elapsed_ns": self.started.elapsed().as_nanos(),
            "records": self.records,
            "bytes": self.bytes,
        })
        .to_string()
        .into_bytes();
        line.push(b'\n');
        let _ = file.write_all(&line);
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        self.write("end");
    }
}

/// Recognize the hot services without copying arbitrary application identifiers into diagnostics.
pub fn service_stage(contract: &str) -> &'static str {
    match contract {
        crate::coding::STORE | crate::coding::STORE_ACCESS => "store.rpc",
        crate::coding::CONTEXT => "context.rpc",
        crate::coding::PROVIDER => "provider.rpc",
        crate::coding::QUEUE => "queue.rpc",
        _ => "service.rpc",
    }
}
