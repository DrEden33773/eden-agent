//! The `select!` adapter.
//!
//! rustfmt never enters a brace-delimited macro body, so lowering replaces the
//! whole `tokio::select! { .. }` body with a parenthesized `match` expression
//! that rustfmt does format, and lift restores the brace delimiter and the
//! tokio arm syntax. `biased;`, guards and `else` are part of the grammar even
//! where the current tree does not use them.

use crate::engine::{Emitter, Error, Lifter, end_of, start_of};
use crate::scan;
use proc_macro2::{Delimiter, Group, Spacing, TokenTree};

struct Body {
    /// Byte range of `biased;`, when present.
    biased: Option<(usize, usize)>,
    arms: Vec<Arm>,
}

struct Arm {
    start: usize,
    kind: Kind,
}

enum Kind {
    Else {
        handler: Vec<TokenTree>,
        handler_start: usize,
    },
    Bind {
        pattern: Vec<TokenTree>,
        pattern_end: usize,
        future: Vec<TokenTree>,
        future_start: usize,
        guard: Option<Guard>,
        arrow_start: usize,
        arrow_end: usize,
        handler: Vec<TokenTree>,
        handler_start: usize,
    },
}

struct Guard {
    comma_start: usize,
    comma_end: usize,
    keyword_start: usize,
    keyword_end: usize,
    start: usize,
    tokens: Vec<TokenTree>,
}

/// Replace the macro body with its lowered form.
pub(crate) fn lower_body(em: &mut Emitter, group: &Group, suffix: &str) -> Result<(), Error> {
    let tokens: Vec<TokenTree> = group.stream().into_iter().collect();
    let body = parse_body(em, &tokens)?;
    em.push(&format!("match {}{} {{", scan::SELECT_MATCH, suffix));
    if let Some((biased_start, biased_end)) = body.biased {
        em.copy_to(biased_start);
        em.push(scan::BIASED);
        em.drop_region(em.at, biased_end, "biased marker")?;
        // An arm whose body is not a block needs the separator, and the next
        // source arm has no comma of its own to contribute.
        em.push(" => (),");
    }
    for arm in &body.arms {
        em.copy_to(arm.start);
        match &arm.kind {
            Kind::Else {
                handler,
                handler_start,
            } => {
                em.push(&format!("{} => ", scan::ELSE));
                em.drop_region(em.at, *handler_start, "else marker")?;
                em.emit_tokens(handler, false)?;
            }
            Kind::Bind {
                pattern,
                pattern_end,
                future,
                future_start,
                guard,
                arrow_start,
                arrow_end,
                handler,
                handler_start,
            } => {
                em.push("(");
                em.emit_tokens(pattern, false)?;
                em.copy_to(*pattern_end);
                em.push(if guard.is_some() {
                    ") if __esg("
                } else {
                    ") if __es("
                });
                em.drop_region(em.at, *future_start, "select binding")?;
                em.emit_tokens(future, false)?;
                if let Some(guard) = guard {
                    em.copy_to(guard.comma_start);
                    em.push(", ");
                    em.skip_to(guard.comma_end);
                    em.copy_to(guard.keyword_start);
                    em.drop_region(em.at, guard.keyword_end, "select guard keyword")?;
                    em.copy_to(guard.start);
                    em.emit_tokens(&guard.tokens, false)?;
                }
                em.copy_to(*arrow_start);
                em.push(") => ");
                em.drop_region(em.at, *arrow_end, "select arrow")?;
                em.copy_to(*handler_start);
                em.emit_tokens(handler, false)?;
            }
        }
    }
    let close_end = em.offset_of(group.span_close().end())?;
    em.copy_to(close_end - 1);
    em.push("}");
    Ok(())
}

fn parse_body(em: &Emitter<'_>, tokens: &[TokenTree]) -> Result<Body, Error> {
    let mut at = 0;
    let mut biased = None;
    if let (Some(first), Some(second)) = (tokens.first(), tokens.get(1))
        && scan::is_ident(first, "biased")
        && scan::is_punct(second, ';')
    {
        biased = Some((start_of(&em.index, first)?, end_of(&em.index, second)?));
        at = 2;
    }
    let mut arms = Vec::new();
    while at < tokens.len() {
        if scan::is_comma(&tokens[at]) {
            at += 1;
            continue;
        }
        let start = start_of(&em.index, &tokens[at])?;
        if scan::is_ident(&tokens[at], "else") {
            if !scan::is_arrow(tokens, at + 1) {
                return Err(em.error(start, "expected `else => handler`"));
            }
            let handler_at = at + 3;
            let boundary = handler_extent(em, tokens, handler_at, start)?;
            comma_at(em, tokens, boundary)?;
            arms.push(Arm {
                start,
                kind: Kind::Else {
                    handler_start: start_of(&em.index, &tokens[handler_at])?,
                    handler: tokens[handler_at..boundary].to_vec(),
                },
            });
            at = boundary;
            continue;
        }
        let equals = binding_equals(tokens, at)
            .ok_or_else(|| em.error(start, "expected `pattern = future` before `=>`"))?;
        let pattern = tokens[at..equals].to_vec();
        let Some(last) = pattern.last() else {
            return Err(em.error(start, "a select arm needs a binding pattern"));
        };
        let pattern_end = end_of(&em.index, last)?;
        let mut end = equals + 1;
        while end < tokens.len() && !scan::is_arrow(tokens, end) {
            if scan::is_comma(&tokens[end]) {
                break;
            }
            end += 1;
        }
        if end >= tokens.len() {
            return Err(em.error(start, "expected `=>` after the future"));
        }
        let future_end = end;
        let mut guard = None;
        if scan::is_comma(&tokens[end]) {
            if !matches!(tokens.get(end + 1), Some(token) if scan::is_ident(token, "if")) {
                return Err(em.error(start, "expected `if` after the future separator"));
            }
            let keyword = end + 1;
            let guard_at = end + 2;
            let mut close = guard_at;
            while close < tokens.len() && !scan::is_arrow(tokens, close) {
                close += 1;
            }
            if close >= tokens.len() || close == guard_at {
                return Err(em.error(start, "expected `guard => handler`"));
            }
            guard = Some(Guard {
                comma_start: start_of(&em.index, &tokens[end])?,
                comma_end: end_of(&em.index, &tokens[end])?,
                keyword_start: start_of(&em.index, &tokens[keyword])?,
                keyword_end: end_of(&em.index, &tokens[keyword])?,
                start: start_of(&em.index, &tokens[guard_at])?,
                tokens: tokens[guard_at..close].to_vec(),
            });
            end = close;
        }
        if future_end == equals + 1 {
            return Err(em.error(start, "a select arm needs a future expression"));
        }
        let arrow_start = start_of(&em.index, &tokens[end])?;
        let arrow_end = end_of(&em.index, &tokens[end + 1])?;
        let handler_at = end + 2;
        if handler_at >= tokens.len() {
            return Err(em.error(start, "expected a handler after `=>`"));
        }
        // A block handler is a statement of its own, so tokio allows the next
        // arm to start without a comma; any other handler needs one.
        let boundary = if matches!(&tokens[handler_at], TokenTree::Group(group) if group.delimiter() == Delimiter::Brace)
        {
            handler_at + 1
        } else {
            // The last arm of a body has no separator at all.
            let close = scan::find(tokens, handler_at, |_, token| scan::is_comma(token))
                .unwrap_or(tokens.len());
            if close == handler_at {
                return Err(em.error(start, "expected a handler after `=>`"));
            }
            close
        };
        let _ = comma_at(em, tokens, boundary)?;
        arms.push(Arm {
            start,
            kind: Kind::Bind {
                pattern,
                pattern_end,
                future_start: start_of(&em.index, &tokens[equals + 1])?,
                future: tokens[equals + 1..future_end].to_vec(),
                guard,
                arrow_start,
                arrow_end,
                handler_start: start_of(&em.index, &tokens[handler_at])?,
                handler: tokens[handler_at..boundary].to_vec(),
            },
        });
        at = boundary;
        if at < tokens.len() && scan::is_comma(&tokens[at]) {
            at += 1;
        }
    }
    Ok(Body { biased, arms })
}

/// Where an `else` handler ends: at its block, or at the separating comma.
fn handler_extent(
    em: &Emitter<'_>,
    tokens: &[TokenTree],
    handler_at: usize,
    start: usize,
) -> Result<usize, Error> {
    if handler_at >= tokens.len() {
        return Err(em.error(start, "expected a handler after `else =>`"));
    }
    if matches!(&tokens[handler_at], TokenTree::Group(group) if group.delimiter() == Delimiter::Brace)
    {
        return Ok(handler_at + 1);
    }
    let close =
        scan::find(tokens, handler_at, |_, token| scan::is_comma(token)).unwrap_or(tokens.len());
    if close == handler_at {
        return Err(em.error(start, "expected a handler after `else =>`"));
    }
    Ok(close)
}

fn comma_at(em: &Emitter<'_>, tokens: &[TokenTree], at: usize) -> Result<Option<usize>, Error> {
    match tokens.get(at) {
        Some(token) if scan::is_comma(token) => Ok(Some(start_of(&em.index, token)?)),
        _ => Ok(None),
    }
}

/// The `=` that separates a binding pattern from its future.
fn binding_equals(tokens: &[TokenTree], from: usize) -> Option<usize> {
    let mut at = from;
    while at < tokens.len() {
        if scan::is_punct(&tokens[at], '=') && !scan::is_arrow(tokens, at) {
            let compound = at > 0
                && matches!(&tokens[at - 1], TokenTree::Punct(previous)
                    if previous.spacing() == Spacing::Joint);
            if !compound {
                return Some(at);
            }
        }
        at += 1;
    }
    None
}

/// Index where the handler of the arm starting at `from` ends.
///
/// Only lifting uses this: lowering knows a handler's extent from the source
/// grammar, while a lowered body is always the shape this predicate matches.
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
    if matches!(tokens.get(at), Some(token) if scan::is_ident(token, scan::ELSE) || scan::is_ident(token, scan::BIASED))
        && scan::is_arrow(tokens, at + 1)
    {
        return true;
    }
    matches!(tokens.get(at), Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis)
        && matches!(tokens.get(at + 1), Some(token) if scan::is_ident(token, "if"))
        && matches!(tokens.get(at + 2), Some(token) if scan::is_ident(token, scan::FUTURE) || scan::is_ident(token, scan::GUARD))
        && matches!(tokens.get(at + 3), Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis)
        && scan::is_arrow(tokens, at + 4)
}

// ---------------------------------------------------------------------------
// lifting
// ---------------------------------------------------------------------------

/// One lowered arm, as the formatted text spells it.
struct LiftArm {
    start: usize,
    end: usize,
    comma: Option<usize>,
    biased: bool,
    pattern: Vec<TokenTree>,
    pattern_start: usize,
    future: Vec<TokenTree>,
    future_start: usize,
    guard: Option<Vec<TokenTree>>,
    guard_start: usize,
    handler: Vec<TokenTree>,
    handler_start: usize,
}

/// Rebuild the `select!` body from the arms of a lowered body.
pub(crate) fn lift_body(lf: &mut Lifter<'_>, group: &Group, sentinel: &str) -> Result<(), Error> {
    let open_end = lf.offset_of(group.span_open().end())?;
    let close_start = lf.offset_of(group.span_close().start())?;
    let close_end = lf.offset_of(group.span_close().end())?;
    lf.drop_region(lf.at, open_end, "match scrutinee")?;
    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
    let arms = parse_lift_arms(lf, &inner)?;
    validate(group, sentinel, arms.len(), lf, open_end)?;
    for (index, arm) in arms.iter().enumerate() {
        let keep_separator = index + 1 < arms.len();
        lf.copy_to(arm.start);
        if arm.biased {
            lf.push("biased");
            lf.drop_region(lf.at, arm.end, "biased marker")?;
            lf.push(";");
            continue;
        }
        if arm.pattern.is_empty() {
            lf.drop_region(lf.at, arm.handler_start, "else marker")?;
            lf.push("else => ");
        } else {
            lf.drop_region(lf.at, arm.pattern_start, "pattern parenthesis")?;
            lf.walk(&arm.pattern)?;
            lf.drop_region(lf.at, arm.future_start, "select binding")?;
            // A piece rustfmt moved to its own line is joined back only while
            // the line still fits; its break is otherwise kept, at the
            // indentation the formatted text gave it.
            if lf.overflows(&arm.future, " = ".len()) {
                lf.push(" =");
                lf.push(&break_before(lf, &arm.future));
            } else {
                lf.push(" = ");
            }
            lf.walk(&arm.future)?;
            if let Some(guard) = &arm.guard {
                lf.drop_region(lf.at, arm.guard_start, "select guard marker")?;
                if lf.overflows(guard, ", if ".len()) {
                    lf.push(",");
                    lf.push(&break_before(lf, guard));
                    lf.push("if ");
                } else {
                    lf.push(", if ");
                }
                lf.walk(guard)?;
            }
            lf.drop_region(lf.at, arm.handler_start, "select arrow")?;
            if lf.overflows(&arm.handler, " => ".len()) {
                lf.push(" =>");
                lf.push(&break_before(lf, &arm.handler));
            } else {
                lf.push(" => ");
            }
        }
        lf.walk(&arm.handler)?;
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
    Ok(())
}

fn parse_lift_arms(lf: &Lifter<'_>, tokens: &[TokenTree]) -> Result<Vec<LiftArm>, Error> {
    let mut arms = Vec::new();
    let mut at = 0;
    while at < tokens.len() {
        if scan::is_comma(&tokens[at]) {
            at += 1;
            continue;
        }
        let start = start_of(&lf.index, &tokens[at])?;
        let mut arm = LiftArm {
            start,
            end: 0,
            comma: None,
            biased: false,
            pattern: Vec::new(),
            pattern_start: 0,
            future: Vec::new(),
            future_start: 0,
            guard: None,
            guard_start: 0,
            handler: Vec::new(),
            handler_start: 0,
        };
        let boundary;
        if scan::is_ident(&tokens[at], scan::BIASED) {
            if !scan::is_arrow(tokens, at + 1) {
                return Err(lf.error(start, "expected `__EdenBiased => ()`"));
            }
            arm.biased = true;
            boundary = at + 4;
        } else if scan::is_ident(&tokens[at], scan::ELSE) {
            if !scan::is_arrow(tokens, at + 1) {
                return Err(lf.error(start, "expected `__EdenElse => handler`"));
            }
            let handler_at = at + 3;
            boundary = arm_boundary(tokens, handler_at);
            if boundary == handler_at {
                return Err(lf.error(start, "a select arm cannot have an empty handler"));
            }
            arm.handler_start = start_of(&lf.index, &tokens[handler_at])?;
            arm.handler = tokens[handler_at..boundary].to_vec();
        } else {
            let Some(TokenTree::Group(pattern)) = tokens.get(at) else {
                return Err(lf.error(start, "expected a lowered select arm"));
            };
            if pattern.delimiter() != Delimiter::Parenthesis
                || !matches!(tokens.get(at + 1), Some(token) if scan::is_ident(token, "if"))
            {
                return Err(lf.error(start, "expected a lowered select arm"));
            }
            let Some(TokenTree::Ident(marker)) = tokens.get(at + 2) else {
                return Err(lf.error(start, "expected a lowered select arm"));
            };
            let Some(TokenTree::Group(arguments)) = tokens.get(at + 3) else {
                return Err(lf.error(start, "expected a lowered select arm"));
            };
            if !scan::is_arrow(tokens, at + 4) {
                return Err(lf.error(start, "expected `=>` in a lowered select arm"));
            }
            let pattern_tokens: Vec<TokenTree> = pattern.stream().into_iter().collect();
            let arguments: Vec<TokenTree> = arguments.stream().into_iter().collect();
            let (future, guard) = match marker.to_string().as_str() {
                scan::FUTURE => (arguments, None),
                scan::GUARD => {
                    let split = scan::find(&arguments, 0, |_, token| scan::is_comma(token))
                        .ok_or_else(|| lf.error(start, "expected a future and a guard"))?;
                    (
                        arguments[..split].to_vec(),
                        Some(arguments[split + 1..].to_vec()),
                    )
                }
                _ => return Err(lf.error(start, "expected a `__es` or `__esg` marker")),
            };
            let handler_at = at + 6;
            boundary = arm_boundary(tokens, handler_at);
            if boundary == handler_at {
                return Err(lf.error(start, "a select arm cannot have an empty handler"));
            }
            arm.pattern_start = start_of(
                &lf.index,
                pattern_tokens
                    .first()
                    .ok_or_else(|| lf.error(start, "a select arm needs a pattern"))?,
            )?;
            arm.pattern = pattern_tokens;
            arm.future_start = start_of(
                &lf.index,
                future
                    .first()
                    .ok_or_else(|| lf.error(start, "a select arm needs a future"))?,
            )?;
            arm.future = future;
            arm.guard_start = match guard.as_ref().and_then(|tokens| tokens.first()) {
                Some(token) => start_of(&lf.index, token)?,
                None => 0,
            };
            arm.guard = guard;
            arm.handler_start = start_of(&lf.index, &tokens[handler_at])?;
            arm.handler = tokens[handler_at..boundary].to_vec();
        }
        arm.comma = match tokens.get(boundary) {
            Some(token) if scan::is_comma(token) => Some(start_of(&lf.index, token)?),
            _ => None,
        };
        arm.end = match arm.comma {
            Some(comma) => comma + 1,
            None => match tokens.get(boundary.saturating_sub(1)) {
                Some(token) => end_of(&lf.index, token)?,
                None => start,
            },
        };
        arms.push(arm);
        at = boundary;
        if at < tokens.len() && scan::is_comma(&tokens[at]) {
            at += 1;
        }
    }
    Ok(arms)
}

/// A line break plus the indentation the formatted text gave a piece.
fn break_before(lf: &Lifter<'_>, tokens: &[TokenTree]) -> String {
    format!("\n{}", " ".repeat(lf.piece_indent(tokens)))
}

/// Re-read the arms with a real parser, the way the design requires.
fn validate(
    group: &Group,
    sentinel: &str,
    expected: usize,
    lf: &Lifter<'_>,
    at: usize,
) -> Result<(), Error> {
    let parsed = crate::engine::parse_match(group, sentinel).map_err(|error| {
        lf.error(
            at,
            format!("the lowered select body does not parse: {error}"),
        )
    })?;
    if parsed.arms.len() != expected {
        return Err(lf.error(at, "the lowered select body has an unexpected arm count"));
    }
    for arm in &parsed.arms {
        if matches!(&arm.pat, syn::Pat::Ident(ident) if ident.ident == scan::BIASED)
            || matches!(&arm.pat, syn::Pat::Ident(ident) if ident.ident == scan::ELSE)
        {
            continue;
        }
        if !matches!(&arm.pat, syn::Pat::Paren(_)) {
            return Err(lf.error(
                at,
                "expected a parenthesized pattern in a lowered select arm",
            ));
        }
        let Some((_, guard)) = &arm.guard else {
            return Err(lf.error(at, "expected a `__es(..)` or `__esg(..)` guard"));
        };
        let syn::Expr::Call(call) = &**guard else {
            return Err(lf.error(at, "expected a `__es(..)` or `__esg(..)` guard"));
        };
        let wanted = match &*call.func {
            syn::Expr::Path(path) if path.path.is_ident(scan::GUARD) => 2,
            syn::Expr::Path(path) if path.path.is_ident(scan::FUTURE) => 1,
            _ => return Err(lf.error(at, "expected a `__es(..)` or `__esg(..)` guard")),
        };
        if call.args.len() != wanted {
            return Err(lf.error(at, "a lowered select guard has the wrong argument count"));
        }
    }
    Ok(())
}
