//! JSON value equality between the input and the formatted output.
//!
//! The comparison reads each `json!` body out of the text with its own scanner
//! and compares the two values with `serde_json`, so it does not depend on the
//! adapter it is checking.

use eden_fmt::{Options, format_source};
use serde_json::Value;

/// Bodies of every `json!({ .. })` or `json!([ .. ])` in a text, taken by
/// matching the delimiters it contains.
fn bodies(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(hit) = source[at..].find("json!(") {
        let start = at + hit + "json!(".len();
        at = start;
        let mut stack: Vec<u8> = Vec::new();
        let mut index = start;
        let mut quote = false;
        let mut escaped = false;
        let mut comment = false;
        while index < bytes.len() {
            let byte = bytes[index];
            if quote {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quote = false;
                }
            } else if comment {
                if byte == b'\n' {
                    comment = false;
                }
            } else if byte == b'"' {
                quote = true;
            } else if byte == b'/' && bytes.get(index + 1) == Some(&b'/') {
                comment = true;
            } else if byte == b'{' || byte == b'[' {
                stack.push(byte);
            } else if byte == b'}' || byte == b']' {
                if stack.pop().is_none() {
                    break;
                }
                if stack.is_empty() {
                    found.push(source[start..=index].to_string());
                    break;
                }
            }
            index += 1;
        }
        at = index.max(at);
    }
    found
}

/// A trailing comma is json! syntax rather than JSON, so it is dropped before
/// `serde_json` sees the body. Commas inside strings are left alone.
fn without_trailing_commas(body: &str) -> String {
    let characters: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut index = 0;
    let mut quote = false;
    let mut escaped = false;
    while index < characters.len() {
        let character = characters[index];
        if quote {
            out.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                quote = false;
            }
            index += 1;
            continue;
        }
        if character == '"' {
            quote = true;
            out.push(character);
            index += 1;
            continue;
        }
        if character == ',' {
            let mut ahead = index + 1;
            while characters
                .get(ahead)
                .is_some_and(|next| next.is_whitespace())
            {
                ahead += 1;
            }
            if matches!(characters.get(ahead), Some('}') | Some(']')) {
                index += 1;
                continue;
            }
        }
        out.push(character);
        index += 1;
    }
    out
}

fn parse(body: &str) -> Result<Value, serde_json::Error> {
    serde_json::from_str(&without_trailing_commas(body))
}

fn values(source: &str) -> Vec<Value> {
    bodies(source)
        .iter()
        .map(|body| {
            let cleaned = without_trailing_commas(body);
            serde_json::from_str(&cleaned).unwrap_or_else(|error| panic!("{error}: {cleaned}"))
        })
        .collect()
}

fn compare(source: &str) {
    let formatted = format_source(source, &Options::default()).expect("formats");
    let before = values(source);
    let after = values(&formatted);
    assert_eq!(before, after, "a JSON value changed:\n{formatted}");
    assert!(!before.is_empty(), "the fixture has no json! object");
}

#[test]
fn reordering_keeps_every_json_value_equal() {
    compare(include_str!(
        "fixtures/reordering_keeps_every_json_value_equal-1.rs.txt"
    ));
    compare(
        "fn f() {\n    let v = \
         json!({\"a\":1,\"b\":{\"c\":2,\"d\":[1,2,{\"e\":3}]},\"f\":{}});\n}\n",
    );
    compare("fn f() {\n    let v = json!({\"kind\":\"local\",\"path\":\"a\",\"count\":2});\n}\n");
    compare("fn f() {\n    let v = json!([{\"a\":1},{\"b\":2}]);\n}\n");
    compare("fn f() {\n    let v = json!({\"多字节\":\"值\"});\n}\n");
    compare(
        "fn f() {\n    let v = \
         json!({\"nested\":{\"deep\":{\"deeper\":[true,false,null,1.5]}}});\n}\n",
    );
}

/// With `EDEN_FMT_EQUIVALENCE_BEFORE` pointing at a pre-format copy of the tree,
/// every `json!` object in the repository is compared the same way. A body that
/// is not a JSON value (it holds a Rust expression) is skipped, but a body that
/// stops being one after formatting is an error rather than a skip.
#[test]
fn reordering_keeps_the_repository_json_values_equal() {
    let Some(before_root) = std::env::var_os("EDEN_FMT_EQUIVALENCE_BEFORE") else {
        return;
    };
    let before_root = std::path::PathBuf::from(before_root);
    let after_root = std::env::var_os("EDEN_FMT_EQUIVALENCE_AFTER")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let mut compared = 0usize;
    let mut skipped = 0usize;
    let mut files = 0usize;
    for entry in walk(&before_root) {
        let relative = entry.strip_prefix(&before_root).expect("under the root");
        let after_path = after_root.join(relative);
        let Ok(before_source) = std::fs::read_to_string(&entry) else {
            continue;
        };
        let Ok(after_source) = std::fs::read_to_string(&after_path) else {
            continue;
        };
        let before = bodies(&before_source);
        if before.is_empty() {
            continue;
        }
        let after = bodies(&after_source);
        assert_eq!(
            before.len(),
            after.len(),
            "{} changed its json! object count",
            relative.display()
        );
        for (index, body) in before.iter().enumerate() {
            let Ok(before_value) = parse(body) else {
                skipped += 1;
                continue;
            };
            let after_value = parse(&after[index]).unwrap_or_else(|error| {
                panic!("{}: {error}: {}", relative.display(), after[index])
            });
            assert_eq!(
                before_value,
                after_value,
                "{} changed a JSON value",
                relative.display()
            );
            compared += 1;
        }
        files += 1;
    }
    println!(
        "compared {compared} json! values in {files} files; skipped {skipped} bodies with Rust \
         expressions"
    );
    assert!(
        compared > 0,
        "no json! object was found under {}",
        before_root.display()
    );
}

fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !name.starts_with('.') && name != "target" && name != "node_modules" {
                    stack.push(path);
                }
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}
