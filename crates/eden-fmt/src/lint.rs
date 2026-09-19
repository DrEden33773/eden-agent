//! The one style rule no formatter can enforce on its own.
//!
//! rustfmt never rewrites a literal and `eden-fmt` never changes one either, so a
//! string that is split with a backslash and a newline is accepted by both tools
//! no matter how the author got there. This module reports those literals so the
//! gate can refuse them: the text has to be carried by `concat!(...)` or by a raw
//! string laid out by meaning, which is a decision only the author can make.
//!
//! Scanning is lexical on purpose. A macro body is not a Rust expression, so the
//! rule must see a continuation wherever it appears, and a violation has to be a
//! position in the author's file rather than in any lowered text.

/// One literal that still uses a backslash line continuation.
pub struct Violation {
    /// Byte offset of the backslash that ends the physical line.
    pub offset: usize,
    /// The literal's opening, for the diagnostic.
    pub opening: String,
}

/// Every continuation in a source text, in source order.
///
/// The scan is lexical rather than token-based: `r"..."` and `br#"..."#` bodies
/// are verbatim, `//` and `/* */` are skipped, a character literal such as `'\"'`
/// does not open a string, and `b` may equally be the start of an identifier. A
/// byte string can only ever contain `\` while the literal is still open, so a
/// stray quote cannot invent a continuation claim on its own.
pub fn violations(source: &str) -> Vec<Violation> {
    let bytes = source.as_bytes();
    let mut found = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let rest = &source[at..];
        if rest.starts_with("//") {
            at = rest.find('\n').map_or(bytes.len(), |end| at + end);
        } else if let Some(body) = rest.strip_prefix("/*") {
            at = body.find("*/").map_or(bytes.len(), |end| at + 2 + end + 2);
        } else if rest.starts_with("br") {
            at = match raw_opening(bytes, at + 2) {
                Some(hashes) => raw_literal(source, at + 2, hashes),
                None => at + 1,
            };
        } else if rest.starts_with("b\"") {
            at = quoted_literal(source, at + 1, &mut found);
        } else if rest.starts_with('\'') {
            at = char_literal(bytes, at);
        } else if rest.starts_with('"') {
            at = quoted_literal(source, at, &mut found);
        } else if rest.starts_with('r') {
            at = match raw_opening(bytes, at + 1) {
                Some(hashes) => raw_literal(source, at + 1, hashes),
                None => at + 1,
            };
        } else {
            at += 1;
        }
    }
    found
}

/// Skip a character literal whose opening quote is at `quote`.
///
/// A quote can only open one when it is not a lifetime, which the following
/// character decides: letters start a lifetime such as `'a`, everything else —
/// including an escaped quote — starts a literal.
fn char_literal(bytes: &[u8], quote: usize) -> usize {
    match bytes.get(quote + 1) {
        None => bytes.len(),
        Some(b'\\') => {
            let mut at = quote + 2;
            while at < bytes.len() {
                if bytes[at] == b'\'' {
                    return at + 1;
                }
                at += 1;
            }
            bytes.len()
        }
        Some(&next) if next.is_ascii_alphabetic() || next == b'_' => quote + 1,
        Some(_) => {
            let mut at = quote + 2;
            while at < bytes.len() {
                if bytes[at] == b'\'' {
                    return at + 1;
                }
                at += 1;
            }
            bytes.len()
        }
    }
}

/// Number of `#` between `r` and the opening quote, when one follows.
fn raw_opening(bytes: &[u8], after: usize) -> Option<usize> {
    let mut hashes = 0;
    while bytes.get(after + hashes) == Some(&b'#') {
        hashes += 1;
    }
    (bytes.get(after + hashes) == Some(&b'"')).then_some(hashes)
}

/// Skip a raw literal; its body is verbatim, so it can hold no continuation.
fn raw_literal(source: &str, quote: usize, hashes: usize) -> usize {
    let terminator = format!("\"{}", "#".repeat(hashes));
    source[quote + 1..]
        .find(&terminator)
        .map_or(source.len(), |end| quote + 1 + end + terminator.len())
}

/// Skip one quoted literal whose opening quote is at `quote`, recording what it holds.
fn quoted_literal(source: &str, quote: usize, found: &mut Vec<Violation>) -> usize {
    let bytes = source.as_bytes();
    let start = if bytes.get(quote.wrapping_sub(1)) == Some(&b'b') {
        quote - 1
    } else {
        quote
    };
    let mut at = quote + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => {
                if bytes.get(at + 1) == Some(&b'\n') {
                    found.push(Violation {
                        offset: at,
                        opening: quote_of(&source[start..at]),
                    });
                }
                at += 2;
            }
            b'"' => return at + 1,
            _ => at += 1,
        }
    }
    bytes.len()
}

/// Opening of a literal, shortened so a diagnostic stays one line.
fn quote_of(literal: &str) -> String {
    let mut out: String = literal.chars().take(48).collect();
    if literal.chars().count() > 48 {
        out.push('…');
    }
    out
}
