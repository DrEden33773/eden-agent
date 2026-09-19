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
        // The cursor only ever moves onto a character boundary, and a multi-byte
        // character is skipped whole, so indexing the text below is safe.
        let byte = bytes[at];
        if byte == b'/' && bytes.get(at + 1) == Some(&b'/') {
            let end = source[at..].find('\n');
            at = end.map_or(bytes.len(), |end| at + end);
        } else if byte == b'/' && bytes.get(at + 1) == Some(&b'*') {
            at = block_comment(source, at);
        } else if byte == b'b' && bytes.get(at + 1) == Some(&b'r') {
            at = match raw_opening(bytes, at + 2) {
                Some(hashes) => raw_literal(source, at + 2, hashes),
                None => at + 1,
            };
        } else if byte == b'b' && bytes.get(at + 1) == Some(&b'"') {
            at = quoted_literal(source, at + 1, &mut found);
        } else if byte == b'\'' {
            // Before the string check: `'"'` opens a character literal, and a
            // string branch would swallow it and everything after it.
            at = char_literal(bytes, at);
        } else if byte == b'"' {
            at = quoted_literal(source, at, &mut found);
        } else if byte == b'r' {
            at = match raw_opening(bytes, at + 1) {
                Some(hashes) => raw_literal(source, at + 1, hashes),
                None => at + 1,
            };
        } else {
            at += source[at..].chars().next().map_or(1, char::len_utf8);
        }
    }
    found
}

/// Skip a block comment, honouring Rust's nesting.
fn block_comment(source: &str, open: usize) -> usize {
    let bytes = source.as_bytes();
    let mut depth = 0;
    let mut at = open;
    while at < bytes.len() {
        if bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'*') {
            depth += 1;
            at += 2;
        } else if bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/') {
            depth -= 1;
            at += 2;
            if depth == 0 {
                return at;
            }
        } else {
            at += 1;
        }
    }
    bytes.len()
}

/// Skip a character literal whose opening quote is at `quote`.
///
/// `'a'` is a character literal and `'a` is a lifetime, so the closing quote
/// decides. The scan stops at a line break or at a `"`, which is what keeps a
/// lifetime from being read as an unterminated literal: `&'a str` has no closing
/// quote before the next one on some later line, so it is left to the string
/// check that follows.
fn char_literal(bytes: &[u8], quote: usize) -> usize {
    // `'"'` is a literal whose body is the quote that would otherwise look like
    // the start of a string.
    if bytes.get(quote + 1) == Some(&b'"') && bytes.get(quote + 2) == Some(&b'\'') {
        return quote + 3;
    }
    let mut at = quote + 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'\'' => return at + 1,
            b'\n' | b'"' => break,
            _ => at += 1,
        }
    }
    quote + 1
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
                // rustc accepts both LF and CRLF after the backslash, so the
                // rule has to see the same continuations the compiler does.
                if bytes.get(at + 1) == Some(&b'\n')
                    || (bytes.get(at + 1) == Some(&b'\r') && bytes.get(at + 2) == Some(&b'\n'))
                {
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
