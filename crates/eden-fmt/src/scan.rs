//! Lexically safe location of macro calls, token predicates and sentinels.

use crate::edit::LineIndex;
use proc_macro2::{Delimiter, Group, Spacing, TokenStream, TokenTree};

/// The macro name that `json!` is matched against, whatever path prefixes it.
pub const JSON_NAME: &str = "json";
/// The macro name that `select!` is matched against, whatever path prefixes it.
pub const SELECT_NAME: &str = "select";

/// Scrutinee of a lowered `json!` body; the suffix records the original delimiter.
pub const JSON_MATCH: &str = "__EdenJson";
/// Scrutinee of a lowered `select!` body; the suffix records the original delimiter.
pub const SELECT_MATCH: &str = "__EdenSelect";
/// Key marker of a lowered object member: `_ if __ejk(key) => value`.
pub const KEY: &str = "__ejk";
/// Future marker of a lowered `select!` arm.
pub const FUTURE: &str = "__es";
/// Future and guard marker of a lowered `select!` arm.
pub const GUARD: &str = "__esg";
/// Marker for the `biased;` line of a `select!` body.
pub const BIASED: &str = "__EdenBiased";
/// Marker for the `else => handler` arm of a `select!` body.
pub const ELSE: &str = "__EdenElse";
/// Every identifier lowering may introduce. Input that already contains one is
/// refused, so a lifted value can never be mistaken for a sentinel.
pub const SENTINELS: [&str; 7] = [JSON_MATCH, SELECT_MATCH, KEY, FUTURE, GUARD, BIASED, ELSE];

/// Keywords that start a statement, never a json! object key.
const STATEMENT_KEYWORDS: [&str; 18] = [
    "let", "if", "match", "for", "while", "loop", "return", "unsafe", "async", "move", "use", "fn",
    "const", "static", "struct", "enum", "impl", "trait",
];

/// Sentinel suffix recording how the original macro call was written.
///
/// The first character is the delimiter to restore; a trailing `S` records
/// that the call stood in statement position and lowering had to give the
/// parenthesized form a semicolon of its own, which lifting removes again.
pub fn suffix(delimiter: Delimiter, statement: bool) -> String {
    let delimiter = match delimiter {
        Delimiter::Parenthesis => 'P',
        Delimiter::Brace => 'B',
        Delimiter::Bracket => 'K',
        Delimiter::None => 'N',
    };
    if statement {
        format!("{delimiter}S")
    } else {
        delimiter.to_string()
    }
}

/// Delimiter recorded by a sentinel name, or `None` when it is not a match marker.
pub fn delimiter_of(name: &str, prefix: &str) -> Option<Delimiter> {
    match name.strip_prefix(prefix)?.chars().next()? {
        'P' => Some(Delimiter::Parenthesis),
        'B' => Some(Delimiter::Brace),
        'K' => Some(Delimiter::Bracket),
        _ => None,
    }
}

/// Whether lowering added a statement semicolon after the call.
pub fn statement_of(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix)
        .is_some_and(|rest| rest.ends_with('S'))
}

/// A macro call found in a token slice.
pub struct MacroCall {
    /// Last path segment, e.g. `json` for `serde_json::json!`.
    pub name: String,
    /// Token index where the path starts.
    pub start: usize,
    /// Byte offset where the path starts.
    pub path_start: usize,
    /// Byte offset of `!`.
    pub bang: usize,
    /// The macro's delimited body.
    pub group: Group,
    /// Byte offset just after the opening delimiter.
    pub open_end: usize,
    /// Byte offset of the closing delimiter.
    pub close_start: usize,
    /// Byte offset just after the closing delimiter.
    pub close_end: usize,
    /// Token index just after the group.
    pub next: usize,
}

/// Classify `tokens[at]` as the `!` of a macro call, if it is one.
pub fn macro_call_at(tokens: &[TokenTree], at: usize, index: &LineIndex) -> Option<MacroCall> {
    let TokenTree::Ident(first) = tokens.get(at)? else {
        return None;
    };
    // Match forward from the first path segment, so a walker sees the whole
    // `json!` or `tokio::select!` and never copies half of it as plain tokens.
    let mut name = first;
    let mut cursor = at;
    loop {
        let next = cursor + 1;
        match (tokens.get(next), tokens.get(next + 1), tokens.get(next + 2)) {
            (
                Some(TokenTree::Punct(colon)),
                Some(TokenTree::Punct(second)),
                Some(TokenTree::Ident(segment)),
            ) if colon.as_char() == ':' && second.as_char() == ':' => {
                name = segment;
                cursor = next + 2;
            }
            _ => break,
        }
    }
    let bang_at = cursor + 1;
    let TokenTree::Punct(bang) = tokens.get(bang_at)? else {
        return None;
    };
    if bang.as_char() != '!' || bang.spacing() != Spacing::Alone {
        return None;
    }
    let TokenTree::Group(group) = tokens.get(bang_at + 1)? else {
        return None;
    };
    Some(MacroCall {
        name: name.to_string(),
        start: at,
        path_start: index.offset(tokens[at].span().start())?,
        bang: index.offset(bang.span().start())?,
        group: group.clone(),
        open_end: index.offset(group.span_open().end())?,
        close_start: index.offset(group.span_close().start())?,
        close_end: index.offset(group.span().end())?,
        next: bang_at + 2,
    })
}

/// True when a token is the given punctuation character.
pub fn is_punct(token: &TokenTree, character: char) -> bool {
    matches!(token, TokenTree::Punct(punct) if punct.as_char() == character)
}

/// True when a token is the given identifier.
pub fn is_ident(token: &TokenTree, name: &str) -> bool {
    matches!(token, TokenTree::Ident(ident) if *ident == name)
}

/// True when a token is a comma.
pub fn is_comma(token: &TokenTree) -> bool {
    is_punct(token, ',')
}

/// True when a token opens a statement rather than naming a value.
pub fn is_statement_keyword(token: &TokenTree) -> bool {
    match token {
        TokenTree::Ident(ident) => STATEMENT_KEYWORDS.contains(&ident.to_string().as_str()),
        _ => false,
    }
}

/// The `=>` at `at`, if the two tokens there form one arrow.
pub fn is_arrow(tokens: &[TokenTree], at: usize) -> bool {
    matches!(tokens.get(at), Some(token) if is_punct(token, '='))
        && matches!(tokens.get(at + 1), Some(token) if is_punct(token, '>'))
}

/// True when the two tokens at `at` start a `::` path separator.
pub fn is_path_separator(tokens: &[TokenTree], at: usize) -> bool {
    matches!(tokens.get(at), Some(token) if is_punct(token, ':'))
        && matches!(tokens.get(at + 1), Some(token) if is_punct(token, ':'))
}

/// Index of the first top-level token satisfying `wanted`, at or after `from`.
pub fn find(
    tokens: &[TokenTree],
    from: usize,
    wanted: impl Fn(usize, &TokenTree) -> bool,
) -> Option<usize> {
    (from..tokens.len()).find(|at| wanted(*at, &tokens[*at]))
}

/// The sentinel already present in these tokens, if any.
///
/// Matching is on identifiers, so the same text inside a string literal is not
/// a collision.
pub fn sentinel_present(tokens: &TokenStream) -> Option<String> {
    for token in tokens.clone() {
        match token {
            TokenTree::Ident(ident) => {
                let name = ident.to_string();
                if SENTINELS.contains(&name.as_str()) || name.starts_with("__Eden") {
                    return Some(name);
                }
            }
            TokenTree::Group(group) => {
                if let Some(found) = sentinel_present(&group.stream()) {
                    return Some(found);
                }
            }
            _ => {}
        }
    }
    None
}

/// A sentinel match expression, e.g. `match __EdenJsonP { .. }`.
// Fields and variants state themselves; see docs/development-checks.md#doc-comments.
#[allow(missing_docs)]
pub struct SentinelMatch {
    pub prefix: &'static str,
    pub name: String,
    pub delimiter: Delimiter,
    pub statement: bool,
    pub group: Group,
}

/// Read `match <sentinel> { .. }` from the body of a lowered macro call.
pub fn sentinel_match(group: &Group) -> Option<SentinelMatch> {
    let tokens: Vec<TokenTree> = group.stream().into_iter().collect();
    let [
        TokenTree::Ident(keyword),
        TokenTree::Ident(name),
        TokenTree::Group(body),
    ] = tokens.as_slice()
    else {
        return None;
    };
    if keyword != "match" {
        return None;
    }
    let name = name.to_string();
    for prefix in [JSON_MATCH, SELECT_MATCH] {
        if let Some(delimiter) = delimiter_of(&name, prefix) {
            if body.delimiter() != Delimiter::Brace {
                return None;
            }
            let statement = statement_of(&name, prefix);
            return Some(SentinelMatch {
                prefix,
                name,
                delimiter,
                statement,
                group: body.clone(),
            });
        }
    }
    None
}
