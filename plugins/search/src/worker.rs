//! The search worker's own process entry point and request loop.
mod engine;
mod history;
mod scope;
mod stream;
use std::io::{BufRead, Write};
fn main() {
    let mut engine = engine::Engine::with_progress(|stage| {
        let mut output = std::io::stdout();
        if writeln!(
            output,
            "{}",
            serde_json::json!({ "progress": stage, "worker_pid": std::process::id() })
        )
        .and_then(|_| output.flush())
        .is_err()
        {
            std::process::exit(0);
        }
    });
    let mut output = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let result = line.map_err(|e| e.to_string()).and_then(|line| {
            let request = serde_json::from_str(&line).map_err(|e| e.to_string())?;
            engine.query(&request)
        });
        let reply = match result {
            Ok(value) => serde_json::json!({ "result": value }),
            Err(error) => serde_json::json!({ "error": error }),
        };
        if serde_json::to_writer(&mut output, &reply).is_err()
            || writeln!(output).is_err()
            || output.flush().is_err()
        {
            break;
        }
    }
    // All FFF threads are contained in this process. The plugin owns and waits
    // for its exit, including when a native scan does not observe cancellation.
    std::process::exit(0);
}
