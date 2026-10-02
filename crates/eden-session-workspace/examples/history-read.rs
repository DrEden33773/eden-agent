//! Content-free, read-only comparison of strict history reads and actual replay projection.
#[path = "../src/projection.rs"]
mod projection;
use eden_protocol::{coding::StoreView, history};
use std::time::Instant;
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let path = std::path::Path::new(&args[1]);
    let profile = &args[2];
    let start = Instant::now();
    let bytes = std::fs::read(path).unwrap();
    let read = start.elapsed().as_secs_f64();
    let start = Instant::now();
    let summary = eden_protocol::history_read::read_view(
        std::fs::File::open(path).unwrap(),
        eden_protocol::history_read::ReadView::Summary,
        &|| false,
    )
    .unwrap()
    .unwrap();
    let summary_time = start.elapsed().as_secs_f64();
    let summary_size = serde_json::to_vec(&summary.records).unwrap().len();
    let start = Instant::now();
    let scan = history::scan_records(&bytes);
    let scan_time = start.elapsed().as_secs_f64();
    let start = Instant::now();
    let active = history::active_path(&scan.records).unwrap();
    let active_time = start.elapsed().as_secs_f64();
    let start = Instant::now();
    let compact = history::view_records(&active, StoreView::Presentation).unwrap();
    let compact_time = start.elapsed().as_secs_f64();
    let selection = eden_protocol::delivery::Selection {
        attachments: true,
        full_outputs: true,
        thinking: true,
        ..Default::default()
    };
    let raw_views = eden_protocol::presentation::static_views(&active, &selection);
    let compact_views = eden_protocol::presentation::static_views(&compact, &selection);
    let raw_view_bytes = serde_json::to_vec(&raw_views).unwrap();
    let compact_view_bytes = serde_json::to_vec(&compact_views).unwrap();
    let static_equal = raw_view_bytes == compact_view_bytes;
    let mut snap = eden_tui_client::Snapshot {
        reading: None,
        diagnostic: None,
        presentation: eden_protocol::presentation::Snapshot {
            version: eden_protocol::presentation::VERSION,
            session_id: active[0].session_id,
            sequence: 0,
            views: raw_views,
            activity: vec![],
            pending_interactions: vec![],
        },
        state: eden_tui_client::State {
            read_only: true,
            closed: true,
            ..Default::default()
        },
        history: std::sync::Arc::new(active.clone()),
        events: vec![],
        events_cursor: 0,
    };
    let full_updates = projection::Projection::default().apply(&snap, true);
    snap.history = std::sync::Arc::new(compact.clone());
    let compact_updates = projection::Projection::default().apply(&snap, true);
    let presentation_equal = full_updates == compact_updates;
    let update_bytes = serde_json::to_vec(&full_updates).unwrap().len();
    assert!(!full_updates.is_empty());
    assert!(presentation_equal);
    assert!(projection::usage_text(&active) == projection::usage_text(&compact));
    let start = Instant::now();
    let scanned_view = eden_protocol::history_read::read_view(
        bytes.as_slice(),
        eden_protocol::history_read::ReadView::Presentation,
        &|| false,
    )
    .unwrap()
    .unwrap();
    let consumer_scan = start.elapsed().as_secs_f64();
    let consumer = history::active_path(&scanned_view.records).unwrap();
    assert!(
        serde_json::to_value(&consumer).unwrap() == serde_json::to_value(&compact).unwrap(),
        "consumer projection differs"
    );
    let start = Instant::now();
    let full_bytes = serde_json::to_vec(&active).unwrap();
    let full_encode = start.elapsed().as_secs_f64();
    let start = Instant::now();
    let compact_bytes = serde_json::to_vec(&compact).unwrap();
    let compact_encode = start.elapsed().as_secs_f64();
    println!(
        "{}",
        serde_json::json!({
            "profile": profile,
            "presentation_equal": presentation_equal,
            "static_views_equal": static_equal,
            "presentation_bytes": update_bytes,
            "replay_updates": full_updates.len(),
            "history_bytes": bytes.len(),
            "read_seconds": read,
            "consumer_scan_seconds": consumer_scan,
            "consumer_equal": true,
            "summary_seconds": summary_time,
            "summary_bytes": summary_size,
            "full_scan_seconds": scan_time,
            "active_clone_seconds": active_time,
            "compact_projection_seconds": compact_time,
            "full_encode_seconds": full_encode,
            "compact_encode_seconds": compact_encode,
            "full_wire_bytes": full_bytes.len(),
            "compact_wire_bytes": compact_bytes.len(),
            "records": scan.records.len(),
            "diagnostic": scan.diagnostic.is_some(),
        })
    );
}
