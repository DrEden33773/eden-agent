//! Behaviour of the style rule no formatter can enforce by rewriting.

use eden_fmt::{Options, engine::Violation, format_source};

fn violations(source: &str) -> Vec<Violation> {
    eden_fmt::engine::check_source(source)
}

fn positions(source: &str) -> Vec<(usize, usize)> {
    violations(source)
        .iter()
        .map(|violation| (violation.line, violation.column))
        .collect()
}

#[test]
fn a_continuation_is_reported_at_its_own_position() {
    let source = "fn f() {\n    let x = \"one \\\n             two\";\n}\n";
    let found = violations(source);
    assert_eq!(found.len(), 1, "one continuation, one violation");
    assert_eq!(positions(source), vec![(2, 18)]);
    assert!(found[0].opening.starts_with("\"one "));
}

#[test]
fn several_continuations_are_reported_in_source_order() {
    let source = "fn f() {\n    let a = \"x \\\n y\";\n    let b = \"p \\\n q\";\n}\n";
    let found = violations(source);
    assert_eq!(found.len(), 2);
    assert!(found[0].line < found[1].line);
}

#[test]
fn byte_strings_and_their_escapes_are_seen() {
    let source = "fn f() {\n    let b = b\"HTTP/1.1 200 OK\\r\\nX: \\\n         y\";\n}\n";
    let found = violations(source);
    assert_eq!(found.len(), 1);
    assert!(found[0].opening.starts_with("b\"HTTP"));
}

#[test]
fn raw_strings_are_verbatim_and_never_reported() {
    let source = "fn f() {\n    let r = r\"a \\\n b\";\n    let h = r#\"c \\\n d\"#;\n}\n";
    assert!(violations(source).is_empty());
}

#[test]
fn comments_and_a_char_quote_do_not_invent_a_violation() {
    let source = "fn f() {\n    // a trailing backslash \\\n    let q = '\"';\n    /* \\\n       */\n    let ok = \"plain\";\n}\n";
    assert!(violations(source).is_empty());
}

/// The three shapes below defeated the first version of the scanner.
#[test]
fn a_non_ascii_source_is_scanned_without_panicking() {
    let bom = "\u{feff}fn f() {\n    let x = \"ok\";\n}\n";
    assert!(violations(bom).is_empty());
    let identified = "fn f() {\n    let café = 1;\n    let x = \"ok\";\n}\n";
    assert!(violations(identified).is_empty());
    let continued = "fn f() {\n    let café = 1;\n    let x = \"one \\\n two\";\n}\n";
    let found = violations(continued);
    assert_eq!(found.len(), 1, "a continuation after a non-ASCII name");
    assert_eq!(found[0].line, 3);
}

#[test]
fn a_character_literal_does_not_hide_a_later_continuation() {
    let alpha = "fn f() {\n    let c = 'a';\n    let s = \"one \\\n two\";\n}\n";
    let escaped = "fn f() {\n    let c = '\\'';\n    let s = \"one \\\n two\";\n}\n";
    for source in [alpha, escaped] {
        let found = violations(source);
        assert_eq!(found.len(), 1, "one continuation in {source:?}");
        assert_eq!(found[0].line, 3);
    }
}

/// A lifetime must still read as one, or the rule would lose the strings after it.
#[test]
fn a_lifetime_is_not_a_character_literal() {
    let source =
        "fn f<'a>(x: &'a str) -> &'a str {\n    x\n}\nfn g() {\n    let s = \"one \\\n two\";\n}\n";
    let found = violations(source);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].line, 5);
}

#[test]
fn nested_block_comments_are_skipped_whole() {
    // only a comment: nothing to report, and no false positive from the quote
    let commented = "/* outer /* inner */ \"fake \\\n still a comment */\nfn f() {}\n";
    assert!(violations(commented).is_empty());
    // a real continuation after a nested comment must still be seen
    let continued = "/* a /* b */ \"c */\nfn f() {\n    let s = \"one \\\n two\";\n}\n";
    let found = violations(continued);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].line, 3);
}

#[test]
fn a_carriage_return_continuation_is_a_continuation() {
    // rustc accepts a backslash followed by CRLF, so the rule has to as well
    let source = "fn f() {\n    let s = \"one \\\r\n        two\";\n}\n";
    let found = violations(source);
    assert_eq!(found.len(), 1);
    assert_eq!((found[0].line, found[0].column), (2, 18));
}

#[test]
fn an_identifier_that_starts_with_r_or_b_is_not_a_literal() {
    let source = "fn f() {\n    let rust = 1;\n    let both = 2;\n    let s = \"a \\\n b\";\n}\n";
    let found = violations(source);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].line, 4);
}

#[test]
fn a_source_without_a_continuation_is_clean() {
    let source =
        "fn f() {\n    let v = json!({\"a\": 1});\n    let s = concat!(\"a \", \"b\");\n}\n";
    assert!(violations(source).is_empty());
}

#[test]
fn concat_is_the_shape_that_replaces_a_continuation() {
    let continued = "fn f() {\n    let s = \"one \\\n              two\";\n}\n";
    let replaced = "fn f() {\n    let s = concat!(\"one \", \"two\");\n}\n";
    let options = Options::default();
    assert!(violations(replaced).is_empty());
    // Both spellings are fixed points of the formatter, which is why only the
    // style rule can tell them apart.
    assert_eq!(format_source(continued, &options).unwrap(), continued);
    assert_eq!(format_source(replaced, &options).unwrap(), replaced);
}

/// The command refuses a file it must not rewrite, and leaves it alone.
#[test]
fn the_command_refuses_a_continuation_without_touching_the_file() {
    let directory = std::env::temp_dir().join(format!("eden-fmt-style-{}", std::process::id()));
    std::fs::create_dir_all(&directory).expect("temp directory");
    let file = directory.join("claimed.rs");
    let source = "fn f() {\n    let x = \"one \\\n             two\";\n}\n";
    std::fs::write(&file, source).expect("fixture");

    let binary = env!("CARGO_BIN_EXE_eden-fmt");
    for mode in ["check", "write"] {
        let output = std::process::Command::new(binary)
            .arg(mode)
            .arg(&file)
            .output()
            .expect("eden-fmt runs");
        assert_eq!(
            output.status.code(),
            Some(1),
            "{mode} reports a style violation"
        );
        let message = String::from_utf8_lossy(&output.stderr);
        assert!(message.contains("backslash continuation"), "{message}");
        assert!(message.contains("claimed.rs:2:18"), "{message}");
        assert_eq!(
            std::fs::read_to_string(&file).expect("readable"),
            source,
            "{mode} must not rewrite the file"
        );
    }

    // A clean file in the same run is still accepted.
    let clean = directory.join("clean.rs");
    std::fs::write(
        &clean,
        "fn f() {\n    let x = concat!(\"one \", \"two\");\n}\n",
    )
    .expect("fixture");
    let output = std::process::Command::new(binary)
        .arg("check")
        .arg(&clean)
        .output()
        .expect("eden-fmt runs");
    assert_eq!(output.status.code(), Some(0));
    std::fs::remove_dir_all(&directory).ok();
}
