//! The one style rule no formatter can enforce on its own.
//!
//! rustfmt never rewrites a literal and `eden-fmt` never changes one either, so a
//! string that is split with a backslash and a newline is accepted by both tools
//! no matter how the author got there. This module reports those literals so the
//! gate can refuse them. Prefer a direct literal; meaningful text groups can
//! use `concat!(...)`. The author chooses a form that preserves the value.
//!
//! The literals come from the lexer this crate already trusts. `proc-macro2`
//! decides what is a literal, a character literal, a lifetime or a comment, and
//! it does so for every spelling Rust has: raw strings with any number of `#`,
//! `b"…"`, `c"…"`, nested block comments, non-ASCII text, CRLF. A hand-written
//! scanner would have to reproduce all of that, and every mistake in it would
//! either refuse a good file or — far worse — let a continuation through.

use crate::engine::tokenize;
use proc_macro2::{Literal, TokenTree};

/// One literal that still uses a backslash line continuation.
pub struct Violation {
    /// Byte offset of the backslash that ends the physical line.
    pub offset: usize,
    /// The literal as the author wrote it, for the diagnostic.
    pub opening: String,
}

/// Every continuation in a source text, in source order.
///
/// A file the lexer cannot read has no literals to report: `format_source`
/// refuses it moments later with the lexer's own message, which is a better
/// diagnostic than anything this rule could add.
pub fn violations(source: &str) -> Vec<Violation> {
    let Ok(tokens) = tokenize(source) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    walk(&tokens, source, &mut found);
    found.sort_by_key(|violation| violation.offset);
    found
}

/// Collect the continuations of every literal in a token tree, groups included.
fn walk(tokens: &[TokenTree], source: &str, found: &mut Vec<Violation>) {
    for token in tokens {
        match token {
            TokenTree::Literal(literal) => inspect(literal, source, found),
            TokenTree::Group(group) => {
                let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                walk(&inner, source, found);
            }
            _ => {}
        }
    }
}

/// Record every continuation inside one literal token.
///
/// The token's own bytes are what get searched, so the prefix question — whether
/// a byte string starts the span at `b` or at the quote — never arises, and a
/// raw string holds no continuation because the lexer already said it is one
/// token of verbatim text.
fn inspect(literal: &Literal, source: &str, found: &mut Vec<Violation>) {
    let range = literal.span().byte_range();
    let Some(text) = source.get(range.clone()) else {
        return;
    };
    if text.starts_with('r') || text.starts_with("br") || text.starts_with("cr") {
        return;
    }
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] != b'\\' {
            at += 1;
            continue;
        }
        // rustc accepts both LF and CRLF after the backslash, and the lexer
        // that produced this token accepts the same two.
        if bytes.get(at + 1) == Some(&b'\n')
            || (bytes.get(at + 1) == Some(&b'\r') && bytes.get(at + 2) == Some(&b'\n'))
        {
            found.push(Violation {
                offset: range.start + at,
                opening: opening_of(&text[..at]),
            });
        }
        at += 2;
    }
}

/// The literal as the author wrote it, shortened so a diagnostic stays one line.
fn opening_of(literal: &str) -> String {
    let mut out: String = literal.chars().take(48).collect();
    if literal.chars().count() > 48 {
        out.push('…');
    }
    out
}
