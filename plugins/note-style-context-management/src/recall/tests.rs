use super::*;
use eden_plugin_sdk::{protocol::coding::Record, serde_json::json};

fn history() -> StoreReply {
    let payloads = [
        json!({ "text": "rare.*精确信息\\\"" }),
        json!({ "secret": "sibling" }),
        json!({ "target": 1, "branch": "alternate" }),
        json!({ "text": "rare.* tail" }),
    ];
    StoreReply {
        session_id: 7,
        sequence: 4,
        active_head: Some(4),
        active_branch: "alternate".into(),
        records: payloads
            .into_iter()
            .enumerate()
            .map(|(i, payload)| Record {
                schema_version: 2,
                session_id: 7,
                sequence: i as u64 + 1,
                run_id: 1,
                parent_id: if i == 0 { None } else { Some(1) },
                branch: if i < 2 { "main" } else { "alternate" }.into(),
                kind: if i == 2 { "branch_selected" } else { "user" }.into(),
                payload,
            })
            .collect(),
    }
}
fn search(literal: &str) -> RecallRequest {
    RecallRequest {
        query: RecallQuery::Search {
            literal: literal.into(),
            from_sequence: None,
            through_sequence: None,
        },
        max_bytes: 8192,
        max_records: 16,
        cursor: None,
    }
}
#[test]
fn search_excludes_siblings_and_treats_regex_characters_literally() {
    let found = recall(history(), search("rare.*")).unwrap();
    assert_eq!(
        found.chunks.iter().map(|c| c.sequence).collect::<Vec<_>>(),
        vec![1, 4]
    );
    assert_eq!(found.chunks[0].branch, "main");
    let missing = recall(history(), search("sibling")).unwrap();
    assert!(missing.chunks.is_empty());
    assert!(!missing.truncated);
    assert!(missing.next_cursor.is_none());
}
#[test]
fn byte_pages_reconstruct_original_unicode_without_skips() {
    let expected = history().records[0].payload.to_string();
    let mut request = RecallRequest {
        query: RecallQuery::Read { sequence: 1 },
        max_bytes: 4,
        ..search("")
    };
    let mut text = String::new();
    loop {
        let page = recall(history(), request.clone()).unwrap();
        let chunk = &page.chunks[0];
        assert_eq!(chunk.byte_start, text.len());
        assert!(chunk.text.len() <= 4);
        text.push_str(&chunk.text);
        if !page.truncated {
            break;
        }
        request.cursor = page.next_cursor;
    }
    assert_eq!(text, expected);
}
#[test]
fn record_pages_and_inclusive_ranges_preserve_sequence() {
    let mut request = search("rare");
    request.max_records = 1;
    let first = recall(history(), request.clone()).unwrap();
    assert!(first.truncated);
    request.cursor = first.next_cursor;
    let second = recall(history(), request).unwrap();
    assert_eq!(second.chunks[0].sequence, 4);
    assert!(!second.truncated);
    let mut bounded = search("rare");
    bounded.query = RecallQuery::Search {
        literal: "rare".into(),
        from_sequence: Some(4),
        through_sequence: Some(4),
    };
    assert_eq!(recall(history(), bounded).unwrap().chunks[0].sequence, 4);
}
#[test]
fn cursor_rejects_changed_query_session_branch_head_and_revision() {
    let mut request = search("rare");
    request.max_bytes = 4;
    request.cursor = recall(history(), request.clone()).unwrap().next_cursor;
    let mut changed = request.clone();
    changed.query = RecallQuery::Read { sequence: 1 };
    assert_eq!(recall(history(), changed).unwrap_err().code, "StaleCursor");
    let mut other = history();
    other.session_id = 8;
    for record in &mut other.records {
        record.session_id = 8;
    }
    assert_eq!(
        recall(other, request.clone()).unwrap_err().code,
        "StaleCursor"
    );
    let mut other = history();
    other.records[3].branch = "another".into();
    other.active_branch = "another".into();
    assert_eq!(
        recall(other, request.clone()).unwrap_err().code,
        "StaleCursor"
    );
    let mut other = history();
    other.records.pop();
    other.sequence = 3;
    other.active_head = Some(1);
    assert_eq!(
        recall(other, request.clone()).unwrap_err().code,
        "StaleCursor"
    );
    let mut other = history();
    let mut navigation = other.records[2].clone();
    navigation.sequence = 5;
    navigation.parent_id = Some(4);
    navigation.payload = json!({ "target": 4, "branch": "alternate" });
    other.records.push(navigation);
    other.sequence = 5;
    assert_eq!(recall(other, request).unwrap_err().code, "StaleCursor");
}
#[test]
fn invalid_limits_cursor_and_off_branch_read_are_diagnostic() {
    let mut request = search("rare");
    request.max_bytes = 0;
    assert_eq!(recall(history(), request).unwrap_err().code, "InvalidInput");
    let mut request = search("rare");
    request.cursor = Some("garbage".into());
    assert_eq!(
        recall(history(), request).unwrap_err().code,
        "InvalidCursor"
    );
    let request = RecallRequest {
        query: RecallQuery::Read { sequence: 2 },
        ..search("")
    };
    assert_eq!(
        recall(history(), request).unwrap_err().code,
        "RecordNotFound"
    );
}

#[test]
fn service_and_tool_only_route_store_reads_even_for_historical_tool_intents() {
    use eden_plugin_sdk::{
        Cancellation,
        abi::{Bytes, HostApi, Reply},
        local::LocalInstance,
        protocol::{Outcome, Request, Terminal},
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Host {
        calls: AtomicUsize,
        unexpected: AtomicUsize,
    }
    unsafe extern "C" fn request(context: usize, bytes: Bytes, reply: Reply) -> u64 {
        // SAFETY: The test keeps its boxed Host alive until LocalInstance::stop completes.
        let host = unsafe { &*(context as *const Host) };
        // SAFETY: Host callbacks borrow the SDK request span for this call only.
        let request: Request = match unsafe { bytes.decode() } {
            Ok(value) => value,
            Err(error) => {
                // SAFETY: This accepted callback consumes its completion exactly once.
                unsafe { reply.send(&Terminal::failed(fault("InvalidInput", error.to_string()))) };
                return 1;
            }
        };
        host.calls.fetch_add(1, Ordering::SeqCst);
        let terminal =
            if request.contract == STORE && request.payload == json!({ "operation": "read" }) {
                let mut store = history();
                store.records[0].kind = "tool_intent".into();
                store.records[0].payload = json!({
                    "name": "bash",
                    "arguments": "do not run this historical command",
                });
                Terminal {
                    outcome: Outcome::Completed(serde_json::to_value(store).unwrap()),
                    cleanup_errors: vec![],
                    partial_result: None,
                }
            } else {
                host.unexpected.fetch_add(1, Ordering::SeqCst);
                Terminal::failed(fault("UnexpectedCall", request.contract))
            };
        // SAFETY: The SDK supplies a live completion owned by this accepted callback.
        unsafe { reply.send(&terminal) };
        1
    }
    unsafe extern "C" fn cancel(_: usize, _: u64) {}
    unsafe extern "C" fn event(_: usize, _: Bytes) {}
    eden_plugin_sdk::tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(async {
            let host = Box::new(Host {
                calls: AtomicUsize::new(0),
                unexpected: AtomicUsize::new(0),
            });
            // SAFETY: All callbacks are static; host stays live through stop and each reply is synchronous.
            let instance = unsafe {
                LocalInstance::new(
                    crate::create(serde_json::Value::Null).unwrap(),
                    HostApi {
                        context: (&*host as *const Host) as usize,
                        request,
                        cancel,
                        event,
                    },
                )
            };
            let input = RecallRequest {
                query: RecallQuery::Read { sequence: 1 },
                ..search("")
            };
            for contract in [RECALL, TOOL] {
                let payload = if contract == TOOL {
                    serde_json::to_value(ToolRequest {
                        cwd: "/unused".into(),
                        call_id: "call".into(),
                        name: "history_recall".into(),
                        arguments: serde_json::to_value(&input).unwrap(),
                    })
                    .unwrap()
                } else {
                    serde_json::to_value(&input).unwrap()
                };
                let value = instance
                    .call(
                        Request {
                            execution: None,
                            session_id: 7,
                            run_id: 2,
                            contract: contract.into(),
                            payload,
                        },
                        Cancellation::default(),
                    )
                    .await
                    .into_result()
                    .unwrap();
                let page: RecallReply = serde_json::from_value(if contract == TOOL {
                    value["details"].clone()
                } else {
                    value
                })
                .unwrap();
                assert_eq!(page.chunks[0].kind, "tool_intent");
                assert!(page.chunks[0].text.contains("do not run"));
            }
            instance.stop().await.unwrap();
            assert_eq!(host.calls.load(Ordering::SeqCst), 2);
            assert_eq!(host.unexpected.load(Ordering::SeqCst), 0);
        });
}

#[test]
fn continuation_keeps_original_snapshot_across_tool_and_model_appends() {
    let mut input = search("rare");
    input.max_records = 1;
    input.cursor = recall(history(), input.clone()).unwrap().next_cursor;
    let mut appended = history();
    let mut record = appended.records[3].clone();
    record.sequence = 5;
    record.parent_id = Some(4);
    record.kind = "tool_result".into();
    record.payload = json!({ "text": "rare new result excluded from snapshot" });
    appended.records.push(record);
    appended.sequence = 5;
    appended.active_head = Some(5);
    let page = recall(appended, input).unwrap();
    assert_eq!(page.active_head, Some(4));
    assert_eq!(page.revision, 4);
    assert_eq!(
        page.chunks.iter().map(|c| c.sequence).collect::<Vec<_>>(),
        vec![4]
    );
    assert!(!page.truncated);
}
