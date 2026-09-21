//! Nightly value protection, opaque macro boundaries and failed-write behavior.

use eden_fmt::{Options, format_source};

#[test]
fn continued_literals_are_accepted_and_normalized() {
    let source = "fn f(){ let x=\"one \\\n        two\"; }";
    let output = format_source(source, &Options::default()).unwrap();
    assert!(output.contains("let x = \"one two\";"));
    assert_eq!(format_source(&output, &Options::default()).unwrap(), output);
}

#[test]
fn opaque_macro_spelling_is_protected_even_inside_format() {
    for template in [
        "observe!(VALUE)",
        "format!(\"{}\", observe!(VALUE))",
        "stringify!(VALUE)",
    ] {
        let literal = format!("{:?}", "long ordinary words ".repeat(12));
        let source = format!(
            "fn f() {{ let s = {}; }}",
            template.replace("VALUE", &literal)
        );
        match format_source(&source, &Options::default()) {
            Ok(output) => assert!(output.contains(&literal)),
            Err(error) => assert!(
                error.to_string().contains("protected macro literal"),
                "{error}"
            ),
        }
    }
}

#[test]
fn broken_escape_output_is_rejected_without_writing() {
    let directory =
        std::env::temp_dir().join(format!("eden-nightly-reject-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let file = directory.join("sample.rs");
    // This SSE event used to produce invalid output despite rustfmt exiting 0.
    let source = include_str!("fixtures/sse-escape.rs.txt");
    std::fs::write(&file, source).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_eden-fmt"))
        .args(["write"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), source);
    assert!(String::from_utf8_lossy(&output.stderr).contains("preserving the value"));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn select_long_strings_and_unicode_keep_values_and_settle() {
    let message = "你好 café with enough useful context to wrap safely. ".repeat(6);
    let source =
        format!("async fn f() {{ tokio::select! {{ _ = ready() => consume({message:?}), }} }}");
    let output = format_source(&source, &Options::default()).unwrap();
    assert!(output.contains("\\\n"));
    assert_eq!(format_source(&output, &Options::default()).unwrap(), output);
}

#[test]
fn repaired_protocol_fixture_formats_the_surrounding_statement() {
    let source = "async fn f(){let (base,server)=server(include_str!(\"events.txt\")).await;}";
    let output = format_source(source, &Options::default()).unwrap();
    assert!(output.contains("let (base, server) = server("));
}
