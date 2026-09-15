use super::*;
use std::collections::BTreeMap;

pub(crate) fn statuses(records: &[Record]) -> BTreeMap<u64, (&str, &Record)> {
    let mut states = BTreeMap::new();
    for record in records {
        if matches!(
            record.kind.as_str(),
            "queue_accepted" | "queue_delivered" | "queue_consumed" | "queue_returned"
        ) && let Some(id) = record.payload.get("id").and_then(Value::as_u64)
        {
            states.insert(id, (record.kind.as_str(), record));
        }
    }
    states
}
pub(crate) fn pending(records: &[Record]) -> Result<Vec<QueueEntry>, Fault> {
    let (_, branch) = eden_plugin_sdk::protocol::history::branch_state(records)?;
    statuses(records)
        .values()
        .filter(|(state, _)| matches!(*state, "queue_accepted" | "queue_returned"))
        .map(|(_, record)| decode(&record.payload))
        .filter(|entry| entry.as_ref().map_or(true, |e| e.branch == branch))
        .collect()
}
fn decode(value: &Value) -> Result<QueueEntry, Fault> {
    serde_json::from_value(value.clone())
        .map_err(|error| Fault::new("PersistenceFailure", "queue", error.to_string()))
}
pub(crate) async fn queue(
    request: QueueRequest,
    cx: CallContext,
) -> Result<Vec<QueueEntry>, Fault> {
    let history: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
    let entries = pending(&history.records)?;
    match request {
        QueueRequest::Inspect => Ok(entries),
        QueueRequest::Enqueue { kind, content } => {
            if !["steering", "follow_up"].contains(&kind.as_str()) {
                return Err(Fault::new("InvalidInput", "queue", "unknown queue kind"));
            }
            let entry = QueueEntry {
                id: history.sequence + 1,
                branch: history.active_branch,
                kind,
                content,
            };
            append(&cx, "queue_accepted", json!(entry)).await?;
            cx.emit("queue_accepted", json!(entry))?;
            Ok(vec![entry])
        }
        QueueRequest::Configure {
            steering,
            follow_up,
        } => {
            if !["one", "all"].contains(&steering.as_str())
                || !["one", "all"].contains(&follow_up.as_str())
            {
                return Err(Fault::new(
                    "InvalidInput",
                    "queue",
                    "delivery mode must be one or all",
                ));
            }
            append(
                &cx,
                "queue_config",
                json!({"steering":steering,"follow_up":follow_up}),
            )
            .await?;
            Ok(entries)
        }
        QueueRequest::Take { kind } => {
            let all = history
                .records
                .iter()
                .rev()
                .find(|record| record.kind == "queue_config")
                .and_then(|record| record.payload.get(&kind))
                .and_then(Value::as_str)
                == Some("all");
            let selected: Vec<_> = entries
                .into_iter()
                .filter(|entry| entry.kind == kind)
                .take(if all { usize::MAX } else { 1 })
                .collect();
            if !selected.is_empty() {
                let _: StoreReply = cx
                    .call(
                        STORE,
                        &StoreRequest::AppendBatch {
                            run_id: cx.run_id(),
                            entries: selected
                                .iter()
                                .map(|entry| RecordDraft {
                                    kind: "queue_delivered".into(),
                                    payload: json!(entry),
                                })
                                .collect(),
                        },
                    )
                    .await?;
                for entry in &selected {
                    cx.emit("queue_delivered", json!(entry))?;
                }
            }
            Ok(selected)
        }
        QueueRequest::Consume { .. } => Err(Fault::new(
            "InvalidInput",
            "queue",
            "consumption must commit atomically with a complete model response",
        )),
        QueueRequest::Restore => {
            let returned: Vec<_> = statuses(&history.records)
                .values()
                .filter(|(state, _)| *state == "queue_delivered")
                .map(|(_, record)| decode(&record.payload))
                .collect::<Result<_, _>>()?;
            if !returned.is_empty() {
                let _: StoreReply = cx
                    .call(
                        STORE,
                        &StoreRequest::AppendBatch {
                            run_id: cx.run_id(),
                            entries: returned
                                .iter()
                                .map(|entry| RecordDraft {
                                    kind: "queue_returned".into(),
                                    payload: json!(entry),
                                })
                                .collect(),
                        },
                    )
                    .await?;
                for entry in &returned {
                    cx.emit("queue_returned", json!(entry))?;
                }
            }
            let history: StoreReply = cx.call(STORE, &StoreRequest::Read).await?;
            pending(&history.records)
        }
    }
}
