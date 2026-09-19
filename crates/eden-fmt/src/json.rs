//! The `json!` adapter.
//!
//! Lowering rewrites only the object syntax: `{ "key": value, .. }` becomes
//! `match __EdenJson<P|B|S> { _ if __ejk(key) => value, .. }`. Arrays and every
//! other expression are already valid Rust and are copied through, still with
//! their nested bodies lowered. Lifting reads the arms back from the formatted
//! text by construction position and never matches on the shape of an output.

use crate::edit::LineIndex;
use crate::engine::{Emitter, Error, Lifter, end_of, start_of, unlocated};
use crate::scan;
use proc_macro2::{Delimiter, Group, Spacing, TokenTree};

/// A json! value, as far as lowering has to understand it.
enum Node {
    Object(Object),
    Array(Array),
    /// Any other expression: copied through, nested target macros still lowered.
    Opaque(Vec<TokenTree>),
}

struct Object {
    open_start: usize,
    open_end: usize,
    close_start: usize,
    close_end: usize,
    sentinel: String,
    members: Vec<Member>,
}

struct Member {
    key: Vec<TokenTree>,
    key_start: usize,
    /// Byte offset of the `:` that separates the key from its value.
    colon_start: usize,
    value: Node,
}

struct Array {
    open_end: usize,
    close_end: usize,
    elements: Vec<Node>,
}

/// Replace the macro body with its lowered form.
pub(crate) fn lower_body(em: &mut Emitter, group: &Group, suffix: &str) -> Result<(), Error> {
    let body: Vec<TokenTree> = group.stream().into_iter().collect();
    let origin = em.offset_of(group.span_open().start())?;
    let (node, next) = parse_value(&em.index, &body, 0, origin, suffix)?;
    let trailing = next + 1 == body.len() && body.get(next).is_some_and(scan::is_comma);
    if next != body.len() && !trailing {
        return Err(em.error(origin, "json! takes exactly one value"));
    }
    emit_node(em, &node)
}

/// Read `{ .. }` as a member list, or explain why it is not one.
fn parse_object(index: &LineIndex<'_>, group: &Group, sentinel: &str) -> Result<Node, Error> {
    let origin = index
        .offset(group.span_open().start())
        .ok_or_else(unlocated)?;
    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
    if inner.iter().any(|token| scan::is_punct(token, ';')) {
        return Err(Error::grammar(
            index,
            origin,
            "a json object cannot contain a statement",
        ));
    }
    let mut members = Vec::new();
    let mut at = 0;
    while at < inner.len() {
        let here = index.offset(inner[at].span().start()).unwrap_or(origin);
        if scan::is_comma(&inner[at]) {
            return Err(Error::grammar(
                index,
                here,
                "a json member cannot start with `,`",
            ));
        }
        if scan::is_statement_keyword(&inner[at]) {
            return Err(Error::grammar(
                index,
                here,
                "a statement cannot be a json key",
            ));
        }
        let colon = colon_at(&inner, at)
            .ok_or_else(|| Error::grammar(index, here, "expected `\"key\": value`"))?;
        if colon == at {
            return Err(Error::grammar(index, here, "a json key cannot be empty"));
        }
        let (value, next) = parse_value(index, &inner, colon + 1, here, sentinel)?;
        members.push(Member {
            key: inner[at..colon].to_vec(),
            key_start: start_of(index, &inner[at])?,
            colon_start: start_of(index, &inner[colon])?,
            value,
        });
        at = next;
        if at < inner.len() {
            if scan::is_comma(&inner[at]) {
                at += 1;
            } else {
                let here = index.offset(inner[at].span().start()).unwrap_or(origin);
                return Err(Error::grammar(
                    index,
                    here,
                    "expected `,` or `}` after a json member",
                ));
            }
        }
    }
    Ok(Node::Object(Object {
        open_start: index
            .offset(group.span_open().start())
            .ok_or_else(unlocated)?,
        open_end: index
            .offset(group.span_open().end())
            .ok_or_else(unlocated)?,
        close_start: index
            .offset(group.span_close().start())
            .ok_or_else(unlocated)?,
        close_end: index
            .offset(group.span_close().end())
            .ok_or_else(unlocated)?,
        sentinel: sentinel.to_string(),
        members,
    }))
}

/// Read `[ .. ]` as a list of json values.
fn parse_array(index: &LineIndex<'_>, group: &Group, sentinel: &str) -> Result<Node, Error> {
    let origin = index
        .offset(group.span_open().start())
        .ok_or_else(unlocated)?;
    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
    if inner.iter().any(|token| scan::is_punct(token, ';')) {
        return Err(Error::grammar(
            index,
            origin,
            "a repeated array is not a json array",
        ));
    }
    let mut elements = Vec::new();
    let mut at = 0;
    while at < inner.len() {
        let here = index.offset(inner[at].span().start()).unwrap_or(origin);
        if scan::is_comma(&inner[at]) {
            return Err(Error::grammar(
                index,
                here,
                "a json element cannot start with `,`",
            ));
        }
        let (node, next) = parse_value(index, &inner, at, here, sentinel)?;
        elements.push(node);
        at = next;
        if at < inner.len() {
            if scan::is_comma(&inner[at]) {
                at += 1;
            } else {
                let here = index.offset(inner[at].span().start()).unwrap_or(origin);
                return Err(Error::grammar(
                    index,
                    here,
                    "expected `,` or `]` after a json element",
                ));
            }
        }
    }
    Ok(Node::Array(Array {
        open_end: index
            .offset(group.span_open().end())
            .ok_or_else(unlocated)?,
        close_end: index
            .offset(group.span_close().end())
            .ok_or_else(unlocated)?,
        elements,
    }))
}

/// Parse one value, stopping at the `,` that ends it in this context.
fn parse_value(
    index: &LineIndex<'_>,
    tokens: &[TokenTree],
    at: usize,
    origin: usize,
    sentinel: &str,
) -> Result<(Node, usize), Error> {
    let Some(token) = tokens.get(at) else {
        return Err(Error::grammar(index, origin, "expected a json value"));
    };
    match token {
        TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => {
            Ok((parse_object(index, group, sentinel)?, at + 1))
        }
        TokenTree::Group(group) if group.delimiter() == Delimiter::Bracket => {
            // `[x; n]` is a Rust repeat expression, not a json array.
            match parse_array(index, group, sentinel) {
                Ok(node) => Ok((node, at + 1)),
                Err(_) => Ok((Node::Opaque(vec![token.clone()]), at + 1)),
            }
        }
        _ => {
            let end = terminator(tokens, at);
            Ok((Node::Opaque(tokens[at..end].to_vec()), end))
        }
    }
}

/// Index of the `,` that ends the value starting at `at`.
fn terminator(tokens: &[TokenTree], at: usize) -> usize {
    scan::find(tokens, at, |_, token| scan::is_comma(token)).unwrap_or(tokens.len())
}

/// Index of the `:` that separates a key from its value, skipping `::`.
fn colon_at(tokens: &[TokenTree], from: usize) -> Option<usize> {
    let mut at = from;
    while at < tokens.len() {
        if scan::is_punct(&tokens[at], ':') {
            if scan::is_path_separator(tokens, at) {
                at += 2;
                continue;
            }
            if at > 0
                && matches!(&tokens[at - 1], TokenTree::Punct(previous)
                    if previous.as_char() == ':' && previous.spacing() == Spacing::Joint)
            {
                at += 1;
                continue;
            }
            return Some(at);
        }
        at += 1;
    }
    None
}

fn emit_node(em: &mut Emitter<'_>, node: &Node) -> Result<(), Error> {
    match node {
        Node::Object(object) => {
            em.copy_to(object.open_start);
            em.drop_region(object.open_start, object.open_end, "json object brace")?;
            em.push(&format!("match {}{} {{", scan::JSON_MATCH, object.sentinel));
            for member in &object.members {
                em.copy_to(member.key_start);
                em.push(&format!("_ if {}(", scan::KEY));
                em.emit_tokens(&member.key, false)?;
                // Anything between the key and its colon stays inside the guard
                // call, so a comment there is preserved rather than dropped.
                em.copy_to(member.colon_start);
                em.drop_region(em.at, member.colon_start + 1, "json key colon")?;
                em.push(") => ");
                emit_node(em, &member.value)?;
            }
            em.copy_to(object.close_start);
            em.drop_region(object.close_start, object.close_end, "json object brace")?;
            em.push("}");
        }
        Node::Array(array) => {
            em.copy_to(array.open_end);
            for element in &array.elements {
                emit_node(em, element)?;
            }
            em.copy_to(array.close_end);
        }
        Node::Opaque(tokens) => em.emit_tokens(tokens, false)?,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// lifting
// ---------------------------------------------------------------------------

/// One `_ if __ejk(key) => value` arm, as the formatted text spells it.
struct Arm {
    start: usize,
    comma: Option<usize>,
    key_start: usize,
    key_end: usize,
    guard_close: usize,
    arrow_end: usize,
    value: Vec<TokenTree>,
    value_start: usize,
}

/// Rebuild `{ .. }` from the arms of a lowered object.
pub(crate) fn lift_body(lf: &mut Lifter<'_>, group: &Group, sentinel: &str) -> Result<(), Error> {
    let open_end = lf.offset_of(group.span_open().end())?;
    let close_start = lf.offset_of(group.span_close().start())?;
    let close_end = lf.offset_of(group.span_close().end())?;
    lf.drop_region(lf.at, open_end, "match scrutinee")?;
    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
    let arms = parse_arms(lf, &inner)?;
    validate(group, sentinel, arms.len(), lf, open_end)?;

    if let Some(inline) = inline_object(lf, open_end, close_start, &arms)
        && (lf.inline_only || lf.column + inline.chars().count() < lf.options.max_width)
    {
        lf.push(&inline);
        lf.skip_to(close_end);
        return Ok(());
    }
    if lf.inline_only {
        return Err(lf.error(open_end, "this object cannot be written on one line"));
    }

    lf.push("{");
    for (index, arm) in arms.iter().enumerate() {
        let keep_separator = index + 1 < arms.len();
        let key_separator = trivia(&lf.text[arm.key_end..arm.guard_close]);
        let value_separator = trivia_value(&lf.text[arm.arrow_end..arm.value_start]);
        // rustfmt put the value on its own line because the lowered arm did not
        // fit. Joining it back is only an improvement while the member line
        // still fits; otherwise that break is kept.
        let broken = lf.text[arm.arrow_end..arm.value_start].contains('\n');
        let keep_break =
            broken && lf.overflows(&arm.value, key_separator.chars().count() + ": ".len());
        let kept_break = lf.text[arm.arrow_end..arm.value_start].to_string();
        lf.copy_to(arm.start);
        lf.drop_region(lf.at, arm.key_start, "key guard")?;
        lf.copy_to(arm.key_end);
        lf.push(&key_separator);
        if keep_break {
            lf.push(":");
            lf.push(&kept_break);
        } else {
            lf.push(": ");
            lf.push(&value_separator);
        }
        // Move the source cursor onto the value: the two separators above were
        // already captured and written, so the scaffolding between them is not.
        lf.skip_to(arm.guard_close);
        lf.drop_region(lf.at, arm.arrow_end, "arm arrow")?;
        lf.skip_to(arm.value_start);
        lift_value(lf, &arm.value)?;
        match arm.comma {
            Some(comma) => {
                lf.copy_to(comma);
                lf.skip_to(comma + 1);
                lf.push(",");
            }
            None if keep_separator => lf.push(","),
            None => {}
        }
    }
    lf.copy_to(close_start);
    lf.skip_to(close_end);
    lf.push("}");
    Ok(())
}

/// Split the arms of a lowered object, all of which lowering wrote itself.
fn parse_arms(lf: &Lifter<'_>, tokens: &[TokenTree]) -> Result<Vec<Arm>, Error> {
    let shape = "expected `_ if __ejk(key) => value`";
    let mut arms = Vec::new();
    let mut at = 0;
    while at < tokens.len() {
        if scan::is_comma(&tokens[at]) {
            at += 1;
            continue;
        }
        let start = start_of(&lf.index, &tokens[at])?;
        if !scan::is_ident(&tokens[at], "_")
            || !matches!(tokens.get(at + 1), Some(token) if scan::is_ident(token, "if"))
            || !matches!(tokens.get(at + 2), Some(token) if scan::is_ident(token, scan::KEY))
        {
            return Err(lf.error(start, shape));
        }
        let Some(TokenTree::Group(guard)) = tokens.get(at + 3) else {
            return Err(lf.error(start, shape));
        };
        if guard.delimiter() != Delimiter::Parenthesis || !scan::is_arrow(tokens, at + 4) {
            return Err(lf.error(start, shape));
        }
        let key: Vec<TokenTree> = guard.stream().into_iter().collect();
        let (Some(first), Some(last)) = (key.first(), key.last()) else {
            return Err(lf.error(start, "a json key cannot be empty"));
        };
        let value_at = at + 6;
        let boundary = arm_boundary(tokens, value_at);
        if boundary == value_at {
            return Err(lf.error(start, "a json member cannot have an empty value"));
        }
        let comma = match tokens.get(boundary) {
            Some(token) if scan::is_comma(token) => Some(start_of(&lf.index, token)?),
            _ => None,
        };
        arms.push(Arm {
            start,
            comma,
            key_start: start_of(&lf.index, first)?,
            key_end: end_of(&lf.index, last)?,
            guard_close: lf.offset_of(guard.span_close().start())?,
            arrow_end: end_of(&lf.index, &tokens[at + 5])?,
            value_start: start_of(&lf.index, &tokens[value_at])?,
            value: tokens[value_at..boundary].to_vec(),
        });
        at = boundary;
        if at < tokens.len() && scan::is_comma(&tokens[at]) {
            at += 1;
        }
    }
    Ok(arms)
}

/// Index where the value of the arm starting at `from` ends.
fn arm_boundary(tokens: &[TokenTree], from: usize) -> usize {
    let mut at = from;
    while at < tokens.len() {
        if scan::is_comma(&tokens[at]) || is_arm_start(tokens, at) {
            return at;
        }
        at += 1;
    }
    tokens.len()
}

fn is_arm_start(tokens: &[TokenTree], at: usize) -> bool {
    matches!(tokens.get(at), Some(token) if scan::is_ident(token, "_"))
        && matches!(tokens.get(at + 1), Some(token) if scan::is_ident(token, "if"))
        && matches!(tokens.get(at + 2), Some(token) if scan::is_ident(token, scan::KEY))
        && matches!(tokens.get(at + 3), Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis)
}

/// Re-read the arms with a real parser, the way the design requires.
fn validate(
    group: &Group,
    sentinel: &str,
    expected: usize,
    lf: &Lifter<'_>,
    at: usize,
) -> Result<(), Error> {
    let parsed = crate::engine::parse_match(group, sentinel)
        .map_err(|error| lf.error(at, format!("the lowered object does not parse: {error}")))?;
    if parsed.arms.len() != expected {
        return Err(lf.error(at, "the lowered object has an unexpected arm count"));
    }
    for arm in &parsed.arms {
        if !matches!(&arm.pat, syn::Pat::Wild(_)) {
            return Err(lf.error(at, "expected a `_` pattern in a lowered object"));
        }
        let Some((_, guard)) = &arm.guard else {
            return Err(lf.error(at, "expected a `__ejk(..)` guard in a lowered object"));
        };
        let syn::Expr::Call(call) = &**guard else {
            return Err(lf.error(at, "expected a `__ejk(..)` guard in a lowered object"));
        };
        if !matches!(&*call.func, syn::Expr::Path(path) if path.path.is_ident(scan::KEY)) {
            return Err(lf.error(at, "expected a `__ejk(..)` guard in a lowered object"));
        }
        if call.args.len() != 1 {
            return Err(lf.error(at, "`__ejk` takes exactly one key"));
        }
    }
    Ok(())
}

/// The object written on one line, when every part of it can be.
/// True when every region an inline rendering would drop is layout only.
fn dropped_are_clear(
    lf: &Lifter<'_>,
    open_end: usize,
    close_start: usize,
    pieces: &[std::ops::Range<usize>],
) -> bool {
    let mut cursor = open_end;
    for piece in pieces {
        if piece.start < cursor
            || piece.end > close_start
            || crate::edit::has_comment(&lf.text[cursor..piece.start])
        {
            return false;
        }
        cursor = piece.end;
    }
    cursor <= close_start && !crate::edit::has_comment(&lf.text[cursor..close_start])
}

/// The byte range of a token slice.
fn piece_range(lf: &Lifter<'_>, tokens: &[TokenTree]) -> Option<std::ops::Range<usize>> {
    let first = tokens.first()?;
    let last = tokens.last()?;
    Some(start_of(&lf.index, first).ok()?..end_of(&lf.index, last).ok()?)
}

fn inline_object(
    lf: &Lifter<'_>,
    open_end: usize,
    close_start: usize,
    arms: &[Arm],
) -> Option<String> {
    // The keys, the values and every region between them and around them must
    // be layout only: anything else is kept by leaving the object expanded.
    let mut pieces: Vec<std::ops::Range<usize>> = Vec::new();
    for arm in arms {
        let key = lf.text.get(arm.key_start..arm.key_end)?;
        if key.contains('\n') || crate::edit::has_comment(key) {
            return None;
        }
        pieces.push(arm.key_start..arm.key_end);
        pieces.push(piece_range(lf, &arm.value)?);
    }
    if !dropped_are_clear(lf, open_end, close_start, &pieces) {
        return None;
    }
    if arms.is_empty() {
        return Some("{}".to_string());
    }
    let mut out = String::from("{");
    for (index, arm) in arms.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push(' ');
        out.push_str(&lf.text[arm.key_start..arm.key_end]);
        out.push_str(": ");
        out.push_str(&inline_piece(lf, &arm.value)?);
    }
    out.push_str(" }");
    Some(out)
}

/// Lift a value in place, giving a json array the same treatment as an object:
/// one line when the folded form fits, the formatted layout otherwise.
pub(crate) fn lift_value(lf: &mut Lifter<'_>, tokens: &[TokenTree]) -> Result<(), Error> {
    if let [TokenTree::Group(group)] = tokens
        && group.delimiter() == Delimiter::Bracket
        && let Some(elements) = elements_of(group)
        && let Some(inline) = inline_array(lf, group, &elements)
        && (lf.inline_only || lf.column + inline.chars().count() < lf.options.max_width)
    {
        lf.push(&inline);
        lf.skip_to(end_of(&lf.index, &TokenTree::Group(group.clone()))?);
        return Ok(());
    }
    lf.walk(tokens)
}

/// Split a bracketed group into json array elements, or `None` when it is not one.
fn elements_of(group: &Group) -> Option<Vec<Vec<TokenTree>>> {
    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
    if inner.iter().any(|token| scan::is_punct(token, ';')) {
        return None;
    }
    let mut elements = Vec::new();
    let mut at = 0;
    while at < inner.len() {
        if scan::is_comma(&inner[at]) {
            return None;
        }
        let end = scan::find(&inner, at, |_, token| scan::is_comma(token)).unwrap_or(inner.len());
        elements.push(inner[at..end].to_vec());
        at = end;
        if at < inner.len() {
            at += 1;
        }
    }
    Some(elements)
}

fn inline_array(lf: &Lifter<'_>, group: &Group, elements: &[Vec<TokenTree>]) -> Option<String> {
    let open_end = lf.offset_of(group.span_open().end()).ok()?;
    let close_start = lf.offset_of(group.span_close().start()).ok()?;
    let pieces: Vec<std::ops::Range<usize>> = elements
        .iter()
        .map(|element| piece_range(lf, element))
        .collect::<Option<Vec<_>>>()?;
    if !dropped_are_clear(lf, open_end, close_start, &pieces) {
        return None;
    }
    let mut out = String::from("[");
    for (index, element) in elements.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        out.push_str(&inline_piece(lf, element)?);
    }
    out.push(']');
    Some(out)
}

/// Lift a value without emitting, to learn whether it fits on one line.
fn inline_piece(lf: &Lifter<'_>, tokens: &[TokenTree]) -> Option<String> {
    if let [TokenTree::Group(group)] = tokens
        && group.delimiter() == Delimiter::Bracket
        && let Some(elements) = elements_of(group)
    {
        return inline_array(lf, group, &elements);
    }
    let first = tokens.first()?;
    let mut scratch = Lifter::scratch(lf);
    scratch.at = start_of(&lf.index, first).ok()?;
    scratch.walk(tokens).ok()?;
    (!scratch.out.contains('\n')).then_some(scratch.out)
}

/// Layout that lowering wrote around a value, normalised for the DSL.
fn trivia_value(text: &str) -> String {
    let trimmed = text.trim().strip_suffix(',').unwrap_or(text.trim()).trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(" {trimmed} ")
    }
}

/// Layout that lowering wrote between two parts, normalised for the DSL.
fn trivia(text: &str) -> String {
    let trimmed = text.trim();
    let trimmed = trimmed.strip_suffix(',').unwrap_or(trimmed).trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        format!(" {trimmed}")
    }
}
