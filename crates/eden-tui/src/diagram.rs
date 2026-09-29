//! A conservative Mermaid subset: unsupported statements leave the whole source visible.
use std::collections::BTreeMap;
use unicode_width::UnicodeWidthStr;

pub(super) fn render(source: &str, width: usize) -> Option<Vec<String>> {
    let mut statements = source
        .lines()
        .flat_map(|line| line.split(';'))
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("%%"));
    let header = statements.next()?;
    let sequence = header == "sequenceDiagram";
    let vertical = matches!(
        header,
        "flowchart TD" | "flowchart TB" | "graph TD" | "graph TB"
    );
    if !sequence
        && !matches!(
            header,
            "flowchart LR" | "graph LR" | "flowchart TD" | "flowchart TB" | "graph TD" | "graph TB"
        )
    {
        return None;
    }
    let mut nodes = BTreeMap::new();
    let mut edges = vec![];
    for statement in statements {
        if sequence && let Some(participant) = statement.strip_prefix("participant ") {
            let (id, label) = participant
                .split_once(" as ")
                .unwrap_or((participant, participant));
            if !valid_id(id) || label.is_empty() {
                return None;
            }
            nodes.insert(id.to_owned(), label.to_owned());
            continue;
        }
        let operators: &[(&str, &str)] = if sequence {
            &[
                ("-->>", "╌╌▶"),
                ("->>", "──▶"),
                ("-->", "╌╌▶"),
                ("->", "──▶"),
            ]
        } else {
            &[("-->", "──▶"), ("-.->", "╌╌▶"), ("==>", "━━▶")]
        };
        let Some((operator, arrow)) = operators
            .iter()
            .find(|(operator, _)| statement.contains(operator))
        else {
            if sequence {
                return None;
            }
            node(statement, &mut nodes)?;
            continue;
        };
        let (from, rest) = statement.split_once(operator)?;
        let (rest, message) = if sequence {
            let (to, message) = rest.split_once(':')?;
            (to, message.trim())
        } else if let Some(label) = rest.trim().strip_prefix('|') {
            let (message, to) = label.split_once('|')?;
            (to, message)
        } else {
            (rest, "")
        };
        let from = node(from.trim(), &mut nodes)?;
        let to = node(rest.trim(), &mut nodes)?;
        edges.push((from, to, *arrow, message.to_owned()));
    }
    let label = |id: &str| {
        let label = nodes.get(id).map_or(id, String::as_str);
        if sequence {
            label.to_owned()
        } else {
            format!("[{label}]")
        }
    };
    let mut output = vec![];
    if edges.is_empty() {
        output.extend(nodes.keys().map(|id| label(id)));
    } else {
        for (from, to, arrow, message) in &edges {
            let from = label(from);
            let to = label(to);
            let suffix = if message.is_empty() {
                String::new()
            } else {
                format!(": {message}")
            };
            let row = format!("{from} {arrow} {to}{suffix}");
            if !vertical && row.width() <= width {
                output.push(row);
            } else {
                output.push(from);
                output.push(if message.is_empty() {
                    "  │".to_owned()
                } else {
                    format!("  │ {message}")
                });
                output.push("  ▼".to_owned());
                output.push(to);
            }
        }
        // Standalone declarations remain visible alongside connected nodes.
        output.extend(
            nodes
                .keys()
                .filter(|id| {
                    !edges
                        .iter()
                        .any(|(from, to, _, _)| from == *id || to == *id)
                })
                .map(|id| label(id)),
        );
    }
    (!output.is_empty() && output.iter().all(|line| line.width() <= width)).then_some(output)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_alphanumeric() || c == '_')
}
fn node(input: &str, nodes: &mut BTreeMap<String, String>) -> Option<String> {
    let input = input.trim();
    if let Some((id, label)) = input.split_once('[') {
        let label = label.strip_suffix(']')?.trim_matches('"');
        if !valid_id(id) || label.is_empty() || label.contains(['[', ']']) {
            return None;
        }
        nodes.insert(id.to_owned(), label.to_owned());
        Some(id.to_owned())
    } else if valid_id(input) {
        nodes
            .entry(input.to_owned())
            .or_insert_with(|| input.to_owned());
        Some(input.to_owned())
    } else {
        None
    }
}
