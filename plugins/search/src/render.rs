//! Human-readable rendering of a search result payload.
use eden_plugin_sdk::serde_json::{self, Value, json};
/// One path per group; metadata is separate from the compact line stream.
pub fn render(value: &Value) -> String {
    if let Some(candidates) = value.get("candidates") {
        return format!(
            concat!(
                "Exact literal search: complete, 0 matches. Fuzzy candidates follow; these are not ",
                "exact occurrences.\n{}",
            ),
            render(candidates)
        );
    }
    let mut header = json!({
        "mode": value["mode"],
        "case": value["case"],
        "scope": value["scope"]["path"],
        "complete": value["complete"],
        "total_matches": value["total_matches"],
        "returned": value["returned"],
        "returned_matches": value["returned_matches"],
        "context_lines": value["context_lines"],
        "has_more": value["has_more"],
        "index": value["index"],
        "ranking": value["ranking"],
    });
    if let Some(cursor) = value.get("cursor") {
        header["cursor"] = cursor.clone();
    }
    if value["complete"] != true {
        header["matched_so_far"] = value["matched_so_far"].clone();
    }
    if value["skipped_count"].as_u64().unwrap_or(0) > 0 {
        header["skipped_count"] = value["skipped_count"].clone();
    }
    let mut output = serde_json::to_string(&header).expect("JSON value");
    output.push('\n');
    if let Some(groups) = value["groups"].as_array() {
        for group in groups {
            let path = group["path"].as_str().unwrap_or("");
            let quoted = serde_json::to_string(path).expect("path string");
            if group["matches"]
                .as_array()
                .is_some_and(|items| items.iter().all(|item| item.get("line").is_none()))
                && group["context"].as_array().is_none_or(Vec::is_empty)
            {
                output.push_str(&quoted);
                output.push('\n');
                continue;
            }
            output.push_str(&quoted);
            output.push_str(":\n");
            let mut lines: Vec<_> = ["matches", "context"]
                .into_iter()
                .filter_map(|field| group[field].as_array())
                .flatten()
                .collect();
            lines.sort_by_key(|item| item["line"].as_u64());
            for item in lines {
                output.push_str(&format!(
                    "{}{} {}",
                    item["line"],
                    if item["context"] == true { "-" } else { ":" },
                    item["text"].as_str().unwrap_or("")
                ));
                if item["line_truncated"] == true {
                    output.push_str(&format!(
                        " [display truncated; {} bytes in full line; use read]",
                        item["line_bytes"]
                    ));
                }
                output.push('\n');
            }
        }
    }
    if let Some(skipped) = value["skipped"].as_array() {
        for item in skipped {
            output.push_str("Skipped: ");
            output.push_str(&serde_json::to_string(item).expect("skip metadata"));
            output.push('\n');
        }
    }
    output
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn grouped_output_keeps_completeness_and_avoids_repeating_paths() {
        let path = "src/a/long/relative/path/to/component.rs";
        let lines: Vec<_> = (1..=30)
            .map(|line| json!({ "line": line, "text": "needle", "line_truncated": false }))
            .collect();
        let page = json!({
            "mode": "literal",
            "case": "sensitive",
            "scope": { "path": "/project" },
            "complete": true,
            "total_matches": 31,
            "returned": 30,
            "has_more": true,
            "cursor": "next-page",
            "index": { "state": "ready" },
            "skipped_count": 0,
            "skipped": [],
            "groups": [{ "path": path, "matches": lines }],
        });
        let output = render(&page);
        assert_eq!(output.matches(path).count(), 1);
        assert!(output.contains("next-page"));
        assert!(output.contains("\"has_more\":true"));
        assert!(output.contains("30: needle"));
        let repeated = (1..=30)
            .map(|line| format!("{path}:{line}:needle\n"))
            .collect::<String>();
        assert!(output.len() < repeated.len());
    }
}
