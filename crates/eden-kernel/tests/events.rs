//! Event observation closure is independent of run cancellation.
use eden_kernel::Events;

#[tokio::test]
async fn close_wakes_all_readers_and_drains_before_end() {
    let events = Events::new(1);
    events.push(0, "one", serde_json::Value::Null);
    let first = events.clone();
    let second = events.clone();
    let a = tokio::spawn(async move { first.after(1).await });
    let b = tokio::spawn(async move { second.after(1).await });
    events.close();
    assert!(a.await.unwrap().is_empty());
    assert!(b.await.unwrap().is_empty());
    assert_eq!(events.after(0).await.len(), 1);
}

#[tokio::test]
async fn expired_cursor_reports_lag_but_attempt_text_survives_until_settlement() {
    let events = Events::new(1);
    events.push(
        7,
        "model_attempt_started",
        serde_json::json!({ "attempt_id": "a" }),
    );
    for _ in 0..9000 {
        events.push(
            7,
            "model_text_delta",
            serde_json::json!({ "attempt_id": "a", "delta": "x" }),
        );
    }
    assert_eq!(events.read_after(0).await.unwrap_err().code, "Lagged");
    let retained = events.snapshot();
    assert_eq!(retained.len(), 8192);
    assert_eq!(retained.last().unwrap().sequence, 9001);
    let attempts = events.take_attempts(7);
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1].payload["delta"].as_str().unwrap().len(), 9000);
    assert!(events.take_attempts(7).is_empty());
}
