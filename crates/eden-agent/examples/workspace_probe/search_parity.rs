//! New search semantics exercised through the installed native search role.
use super::*;
use std::io::Write;

async fn query(
    session: &Session,
    arguments: Value,
) -> Result<c::ToolResult, Box<dyn std::error::Error>> {
    Ok(serde_json::from_value(
        call(
            session,
            r::SEARCH,
            json!(c::ToolRequest {
                cwd: session.cwd().into(),
                call_id: "parity-search".into(),
                name: "grep".into(),
                arguments,
            }),
        )
        .await?,
    )?)
}

pub(super) async fn run(
    session: &Session,
    cwd: &Path,
) -> Result<Value, Box<dyn std::error::Error>> {
    let root = cwd.join("parity-search");
    std::fs::create_dir(&root)?;
    std::fs::write(
        root.join("selected.rs"),
        "one\ntwo\nneedle\nfour\nneedle\nsix\nseven\n",
    )?;
    std::fs::write(root.join("excluded.txt"), vec![b'x'; 11 * 1024 * 1024])?;
    let mut arguments = json!({
        "pattern": "needle",
        "path": "parity-search",
        "include": ["*.rs"],
        "context": 1,
        "limit": 2,
    });
    let mut lines = vec![];
    loop {
        let result = query(session, arguments.clone()).await?;
        let page = result.details;
        assert_eq!(page["complete"], true);
        assert_eq!(page["total_matches"], 2);
        assert_eq!(page["skipped_count"], 0);
        for group in page["groups"].as_array().unwrap() {
            for field in ["matches", "context"] {
                for row in group[field].as_array().unwrap() {
                    lines.push(row["line"].as_u64().unwrap());
                }
            }
        }
        if page["has_more"] == false {
            break;
        }
        arguments["cursor"] = page["cursor"].clone();
    }
    lines.sort_unstable();
    assert_eq!(lines, vec![2, 3, 4, 5, 6]);
    let large = root.join("large.txt");
    let mut file = std::fs::File::create(&large)?;
    let block = vec![b'x'; 1024 * 1024];
    for _ in 0..11 {
        file.write_all(&block)?;
        file.write_all(b"\n")?;
    }
    file.write_all("before\nα needle\nbetween\nω needle\nafter\n".as_bytes())?;
    drop(file);
    let arguments = json!({
        "pattern": r"^\p{Greek} needle$",
        "mode": "regex",
        "path": "parity-search/large.txt",
        "context": 1,
        "limit": 2,
    });
    let result = query(session, arguments.clone()).await?;
    assert_eq!(result.details["complete"], true);
    assert_eq!(result.details["total_matches"], 2);
    assert_eq!(result.details["index"]["direct_stream"], true);
    assert!(result.text.contains("α needle"));
    let mut continuation = arguments.clone();
    continuation["cursor"] = result.details["cursor"].clone();
    let mut changed = continuation.clone();
    changed["include"] = json!(["*.txt"]);
    assert!(
        query(session, changed)
            .await
            .unwrap_err()
            .to_string()
            .contains("different query")
    );
    assert_eq!(
        query(session, continuation.clone()).await?.details["returned"],
        2
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&large)?
        .write_all(b"changed\n")?;
    assert!(
        query(session, continuation)
            .await
            .unwrap_err()
            .to_string()
            .contains("stale")
    );
    let unsupported = query(
        session,
        json!({ "pattern": r"needle\s+after", "mode": "regex", "path": "parity-search/large.txt" }),
    )
    .await?;
    assert_eq!(unsupported.details["complete"], false);
    assert_eq!(
        unsupported.details["skipped"][0]["reason"],
        "multiline_pattern_unsupported_for_large_file"
    );
    for pattern in [r"\Aneedle", r"needle\z", r"(?-m:^needle$)"] {
        let mut arguments = json!({
            "pattern": pattern,
            "mode": "regex",
            "path": "parity-search/selected.rs",
            "max_file_bytes": 1024,
        });
        assert_eq!(
            query(session, arguments.clone()).await?.details["total_matches"],
            0
        );
        arguments["max_file_bytes"] = json!(2);
        let result = query(session, arguments).await?;
        assert_eq!(result.details["complete"], false);
        assert_eq!(
            result.details["skipped"][0]["reason"],
            "file_anchor_unsupported_for_large_file"
        );
    }

    // Only the progress event proves the large-file scan was admitted. No delay
    // stands in for that event, and the role reply is the process exit barrier.
    let mut file = std::fs::File::create(root.join("cancel.txt"))?;
    for _ in 0..128 {
        file.write_all(&block)?;
        file.write_all(b"\n")?;
    }
    drop(file);
    let role = session.role(r::SEARCH)?;
    let cancel = Cancellation::default();
    let signal = cancel.clone();
    let id = session.id();
    let dir = session.cwd().to_owned();
    let mut sequence = session.events().last().map_or(0, |e| e.sequence);
    let task = tokio::spawn(async move {
        role.call(
            Request {
                session_id: id,
                run_id: 101,
                contract: r::SEARCH.into(),
                payload: json!(c::ToolRequest {
                    cwd: dir,
                    call_id: "cancel-large-search".into(),
                    name: "grep".into(),
                    arguments: json!({
                        "pattern": "^x+y$",
                        "mode": "regex",
                        "path": "parity-search/cancel.txt",
                    }),
                }),
            },
            signal,
        )
        .await
    });
    let pid = loop {
        let events = session.events_after(sequence).await;
        sequence = events.last().unwrap().sequence;
        if let Some(event) = events.iter().find(|e| e.kind == "search_stream_started") {
            break event.payload["worker_pid"].clone();
        }
    };
    cancel.cancel();
    assert!(matches!(
        task.await?.outcome,
        eden_protocol::Outcome::Cancelled
    ));
    std::fs::remove_dir_all(root)?;
    Ok(json!({
        "positive_scope_before_skips": true,
        "deduplicated_context_lines": lines,
        "large_unicode_tail_matches": 2,
        "changed_arguments_rejected": true,
        "changed_file_cursor_stale": true,
        "unsupported_multiline_incomplete": true,
        "file_anchors_not_reinterpreted": true,
        "cancelled_after_stream_started": true,
        "stopped_worker_pid": pid,
    }))
}
