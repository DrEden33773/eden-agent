//! Behaviour of the adapter and the invariants the design fixes.

use eden_fmt::{Options, format_source};

fn format(source: &str) -> String {
    format_source(source, &Options::default()).expect("formats")
}

fn failure(source: &str) -> String {
    format_source(source, &Options::default())
        .expect_err("must be refused")
        .to_string()
}

/// Invariant 1: the output is a fixed point of the whole pipeline.
fn assert_idempotent(source: &str) -> String {
    let once = format(source);
    let twice = format(&once);
    assert_eq!(once, twice, "formatting is not idempotent");
    once
}

#[test]
fn objects_collapse_when_they_fit_and_expand_when_they_do_not() {
    assert_eq!(
        assert_idempotent("fn f() {\n    let v = json!({\"a\":1,\"b\":{\"c\":2}});\n}\n"),
        "fn f() {\n    let v = json!({ \"a\": 1, \"b\": { \"c\": 2 } });\n}\n"
    );
    let long = assert_idempotent(
        "fn f() {\n    let v = json!({\"type\":\"object\",\"properties\":{\"command\":{\"type\":\"string\"}},\"required\":[\"command\"],\"additionalProperties\":false});\n}\n",
    );
    assert_eq!(
        long,
        "fn f() {\n    let v = json!({\n        \"type\": \"object\",\n        \"properties\": { \"command\": { \"type\": \"string\" } },\n        \"required\": [\"command\"],\n        \"additionalProperties\": false,\n    });\n}\n"
    );
}

#[test]
fn empty_object_and_arrays_keep_their_shape() {
    assert_eq!(
        assert_idempotent("fn f() {\n    let v = json!({});\n}\n"),
        "fn f() {\n    let v = json!({});\n}\n"
    );
    assert_eq!(
        assert_idempotent("fn f() {\n    let v = json!([{\"a\":1},{\"b\":[1,2,{\"c\":3}]}]);\n}\n"),
        "fn f() {\n    let v = json!([{ \"a\": 1 }, { \"b\": [1, 2, { \"c\": 3 }] }]);\n}\n"
    );
    assert_eq!(
        assert_idempotent("fn f() {\n    let v = json!([\"read\",\"write\"]);\n}\n"),
        "fn f() {\n    let v = json!([\"read\", \"write\"]);\n}\n"
    );
}

#[test]
fn expressions_that_are_already_rust_are_formatted_by_rustfmt() {
    assert_eq!(
        assert_idempotent("fn f() {\n    let v = json!(some_expression( 1,2,3 ));\n}\n"),
        "fn f() {\n    let v = json!(some_expression(1, 2, 3));\n}\n"
    );
    // A struct literal is the whole macro body, so nothing is lowered in it.
    assert_eq!(
        assert_idempotent("fn f() {\n    let v = json!(Request{prompt:1});\n}\n"),
        "fn f() {\n    let v = json!(Request { prompt: 1 });\n}\n"
    );
    // A dynamic key is an expression in the guard position.
    assert_eq!(
        assert_idempotent("fn f() {\n    let v = json!({name: 1, \"a\": key});\n}\n"),
        "fn f() {\n    let v = json!({ name: 1, \"a\": key });\n}\n"
    );
}

#[test]
fn a_qualified_path_and_a_statement_position_keep_their_text() {
    assert_eq!(
        assert_idempotent("fn f() {\n    let v = serde_json::json!({\"x\":1});\n}\n"),
        "fn f() {\n    let v = serde_json::json!({ \"x\": 1 });\n}\n"
    );
    // A statement-position call has no semicolon of its own and must not gain one.
    assert_eq!(
        assert_idempotent(
            "fn f() {\n    let x = 1;\n    select_call! { a = b => c }\n    if x {}\n}\n"
        )
        .lines()
        .count(),
        5
    );
}

#[test]
fn utf8_and_raw_strings_survive() {
    assert_eq!(
        assert_idempotent("fn f() {\n    let v = json!({\"c\":\"一\\nsecond\\n三\\n\"});\n}\n"),
        "fn f() {\n    let v = json!({ \"c\": \"一\\nsecond\\n三\\n\" });\n}\n"
    );
    let raw = assert_idempotent("fn f() {\n    let v = json!(r\"needle\\nnext\");\n}\n");
    assert!(raw.contains("r\"needle\\nnext\""), "{raw}");
}

#[test]
fn comments_inside_a_macro_body_stay_where_they_were() {
    let source = "async fn f() {\n    tokio::select! {\n        biased;\n        // why\n        _ = cancel.cancelled() => value,\n    }\n}\n";
    let formatted = assert_idempotent(source);
    assert!(formatted.contains("// why"), "{formatted}");
    let inside = assert_idempotent("fn f() {\n    let v = json!({\"a\": /* why */ 1});\n}\n");
    assert!(inside.contains("/* why */"), "{inside}");
}

#[test]
fn unknown_grammar_is_refused_rather_than_guessed() {
    let refused = failure("fn f() {\n    let v = json!({\"a\" 1});\n}\n");
    assert!(refused.contains("json"), "{refused}");
    let statements = failure("fn f() {\n    let v = json!({ let x = 1; x });\n}\n");
    assert!(statements.contains("json"), "{statements}");
    let two = failure("fn f() {\n    let v = json!(a, b);\n}\n");
    assert!(two.contains("json"), "{two}");
}

#[test]
fn a_reserved_sentinel_in_the_input_is_refused() {
    let refused = failure("fn f() {\n    let v = json!({ \"a\": __EdenJsonP });\n}\n");
    assert!(refused.contains("reserved"), "{refused}");
    let body = failure("fn f() {\n    let __ejk = 1;\n    let v = json!({\"a\": 1});\n}\n");
    assert!(body.contains("reserved"), "{body}");
}

#[test]
fn no_sentinel_leaks_into_the_result() {
    let formatted =
        assert_idempotent("fn f() {\n    let v = json!({\"a\":[{\"b\":1}],\"c\":{\"d\":2}});\n}\n");
    assert!(!formatted.contains("__Eden"), "{formatted}");
    assert!(!formatted.contains("__ejk"), "{formatted}");
}

#[test]
fn a_comment_inside_a_container_keeps_it_expanded() {
    for source in [
        "fn f() {\n    let v = json!([1, /* keep */ 2]);\n}\n",
        "fn f() {\n    let v = json!({ \"a\": 1 /* keep */ });\n}\n",
        "fn f() {\n    let v = json!({ /* keep */ });\n}\n",
        "fn f() {\n    let v = json!([[1], /* keep */ [2]]);\n}\n",
    ] {
        let formatted = assert_idempotent(source);
        assert!(formatted.contains("/* keep */"), "{formatted}");
    }
}

#[test]
fn a_call_that_continues_with_a_method_stays_an_expression() {
    assert_eq!(
        assert_idempotent("fn f() {\n    let x = 1;\n    json!({ \"a\": 1 }).as_object();\n}\n"),
        "fn f() {\n    let x = 1;\n    json!({ \"a\": 1 }).as_object();\n}\n"
    );
    assert_eq!(
        assert_idempotent(
            "fn f() -> Option<u32> {\n    json!({ \"a\": 1 }).as_u64()?;\n    None\n}\n"
        )
        .lines()
        .count(),
        4
    );
}

#[test]
fn a_literal_that_spans_lines_is_refused_rather_than_rewritten() {
    let refused = failure("fn f() {\n    let v = json!({ \"a\": r#\"line1\nline2\"# });\n}\n");
    assert!(refused.contains("literal"), "{refused}");
}
