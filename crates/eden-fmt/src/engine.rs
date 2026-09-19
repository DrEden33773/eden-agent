//! The `lower -> rustfmt -> lift` pipeline, its two rewriters and the CLI runner.

use crate::edit::{LineIndex, advance_column, has_comment};
use crate::json;
use crate::scan::{self, MacroCall};
use crate::select;
use proc_macro2::{Delimiter, Group, LineColumn, TokenStream, TokenTree};
use std::fmt;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;

/// rustfmt settings shared by lowering, lifting and the rustfmt call itself.
#[derive(Clone, Debug)]
pub struct Options {
    pub edition: String,
    pub style_edition: String,
    pub max_width: usize,
    pub rustfmt: String,
    pub jobs: usize,
    pub verify: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            edition: "2024".to_string(),
            style_edition: "2024".to_string(),
            max_width: 100,
            rustfmt: "rustfmt".to_string(),
            jobs: 1,
            verify: true,
        }
    }
}

/// Every way formatting one file can fail. A failure never modifies the file.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Tokens(String),
    Grammar {
        line: usize,
        column: usize,
        message: String,
    },
    Rustfmt(String),
    Verification {
        line: usize,
        message: String,
    },
}

impl Error {
    /// Name the macro a diagnostic came from, keeping its position.
    fn in_macro(self, name: &str) -> Self {
        match self {
            Error::Grammar {
                line,
                column,
                message,
            } => Error::Grammar {
                line,
                column,
                message: format!("`{name}!`: {message}"),
            },
            other => other,
        }
    }

    pub fn grammar(index: &LineIndex, offset: usize, message: impl Into<String>) -> Self {
        Error::Grammar {
            line: index.line(offset),
            column: index.column(offset),
            message: message.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(error) => write!(formatter, "{error}"),
            Error::Tokens(message) => write!(formatter, "not a Rust token stream: {message}"),
            Error::Grammar {
                line,
                column,
                message,
            } => {
                write!(formatter, "{line}:{}: {message}", column + 1)
            }
            Error::Rustfmt(message) => write!(formatter, "rustfmt failed: {message}"),
            Error::Verification { line, message } => {
                write!(
                    formatter,
                    "{line}: formatting is not a fixed point: {message}"
                )
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Error::Io(error)
    }
}

/// How many times the pipeline may re-run before a text has to be a fixed point.
const ROUNDS: usize = 8;

/// Format one Rust source text, macro bodies included.
///
/// A lowered macro body is wider than the DSL it stands for, so rustfmt breaks
/// the code *around* it more than the finished file needs. The result is
/// therefore the rustfmt output for the file as it stands, with only the macro
/// bodies taken from the lowered round; the pipeline repeats until that text is
/// a fixed point of both rustfmt and this formatter, and refuses to answer when
/// it never is.
pub fn format_source(source: &str, options: &Options) -> Result<String, Error> {
    let mut current = source.to_string();
    for _ in 0..ROUNDS {
        let baseline = rustfmt(&current, options)?;
        let (result, macros) = lower_ranges(&baseline, options)?;
        let lowered = rustfmt(&result, options)?;
        let (lifted, pieces) = lift_pieces(&lowered, options)?;
        // rustfmt re-indents an opaque macro body to the indentation it tracks
        // at the call, so the spliced text goes through it once more; that pass
        // is what makes the answer stable for `cargo fmt` as well.
        let spliced = splice(&baseline, &lifted, &macros, &pieces)?;
        let next = rustfmt(&spliced, options)?;
        // The first round's rustfmt already re-indented any opaque body the
        // input carried, so the comparison starts at the input rather than at
        // the splice: a literal's text is one token and rustfmt never rewrites
        // one on purpose.
        if words(&current)? != words(&next)? {
            return Err(Error::Verification {
                line: 1,
                message: "rustfmt changed a literal or a name inside a macro body".to_string(),
            });
        }
        if next == current {
            return Ok(next);
        }
        current = next;
    }
    Err(Error::Verification {
        line: 1,
        message: format!("formatting did not settle after {ROUNDS} rounds"),
    })
}

/// Keep the macro ranges that no other range contains.
///
/// A macro nested in another one is rewritten as part of its parent, so only
/// the outermost invocations describe the regions the splice replaces.
fn outermost(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by(|left, right| left.start.cmp(&right.start).then(right.end.cmp(&left.end)));
    let mut kept: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        let nested = kept.last().is_some_and(|last| range.end <= last.end);
        if !nested {
            kept.push(range);
        }
    }
    kept
}

/// Replace each macro invocation of `baseline` with the body lifting produced.
fn splice(
    baseline: &str,
    lifted: &str,
    macros: &[Range<usize>],
    pieces: &[Range<usize>],
) -> Result<String, Error> {
    if macros.len() != pieces.len() {
        return Err(Error::Verification {
            line: 1,
            message: format!(
                "{} macro bodies were lowered but {} were lifted",
                macros.len(),
                pieces.len()
            ),
        });
    }
    let mut out = String::with_capacity(baseline.len());
    let mut previous = 0;
    for (range, piece) in macros.iter().zip(pieces) {
        if range.start < previous || range.end > baseline.len() {
            return Err(Error::Verification {
                line: 1,
                message: "lowering reported overlapping macro ranges".to_string(),
            });
        }
        out.push_str(&baseline[previous..range.start]);
        out.push_str(&lifted[piece.clone()]);
        previous = range.end;
    }
    out.push_str(&baseline[previous..]);
    Ok(out)
}

/// Replace every `json!` / `select!` body with a rustfmt-readable expression.
pub fn lower_source(source: &str, options: &Options) -> Result<String, Error> {
    lower_ranges(source, options).map(|(text, _)| text)
}

/// Lowering plus the byte range of every macro invocation it rewrote.
pub fn lower_ranges(source: &str, options: &Options) -> Result<(String, Vec<Range<usize>>), Error> {
    let index = LineIndex::new(source);
    let stream = TokenStream::from_str(source).map_err(|error| Error::Tokens(error.to_string()))?;
    if let Some(name) = scan::sentinel_present(&stream) {
        let offset = source.find(&name).unwrap_or(0);
        return Err(Error::grammar(
            &index,
            offset,
            format!("`{name}` is reserved by eden-fmt"),
        ));
    }
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut emitter = Emitter::new(source, options);
    emitter.emit_tokens(&tokens, true)?;
    emitter.copy_to(source.len());
    Ok((emitter.out, outermost(emitter.macros)))
}

/// Restore every lowered body in a formatted text.
pub fn lift_source(formatted: &str, options: &Options) -> Result<String, Error> {
    lift_pieces(formatted, options).map(|(text, _)| text)
}

/// Lifting plus the byte range of every macro body it produced.
pub fn lift_pieces(
    formatted: &str,
    options: &Options,
) -> Result<(String, Vec<Range<usize>>), Error> {
    let tokens = tokenize(formatted)?;
    let mut lifter = Lifter::new(formatted, options);
    lifter.walk(&tokens)?;
    lifter.copy_to(formatted.len());
    Ok((lifter.out, outermost(lifter.pieces)))
}

/// Every identifier and literal a text contains, in source order.
///
/// Whitespace and separators are dropped, so a re-indented or re-commaed text
/// compares equal while a changed literal does not.
fn words(text: &str) -> Result<Vec<String>, Error> {
    fn walk(tokens: TokenStream, out: &mut Vec<String>) {
        for token in tokens {
            match token {
                TokenTree::Ident(ident) => out.push(ident.to_string()),
                TokenTree::Literal(literal) => out.push(literal.to_string()),
                TokenTree::Group(group) => walk(group.stream(), out),
                TokenTree::Punct(_) => {}
            }
        }
    }
    let stream = TokenStream::from_str(text).map_err(|error| Error::Tokens(error.to_string()))?;
    let mut out = Vec::new();
    walk(stream, &mut out);
    Ok(out)
}

pub(crate) fn tokenize(source: &str) -> Result<Vec<TokenTree>, Error> {
    TokenStream::from_str(source)
        .map(|stream| stream.into_iter().collect())
        .map_err(|error| Error::Tokens(error.to_string()))
}

pub(crate) fn rustfmt(text: &str, options: &Options) -> Result<String, Error> {
    let mut child = Command::new(&options.rustfmt)
        .arg("--edition")
        .arg(&options.edition)
        .arg("--config")
        .arg(format!(
            "style_edition={},max_width={}",
            options.style_edition, options.max_width
        ))
        .arg("--emit")
        .arg("stdout")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(text.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(Error::Rustfmt(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| Error::Tokens(error.to_string()))
}

/// Delimiters of a macro body, as text.
pub(crate) fn delimiters(delimiter: Delimiter) -> Option<(&'static str, &'static str)> {
    match delimiter {
        Delimiter::Parenthesis => Some(("(", ")")),
        Delimiter::Brace => Some(("{", "}")),
        Delimiter::Bracket => Some(("[", "]")),
        Delimiter::None => None,
    }
}

pub(crate) fn start_of(index: &LineIndex, token: &TokenTree) -> Result<usize, Error> {
    index.offset(token.span().start()).ok_or_else(unlocated)
}

pub(crate) fn end_of(index: &LineIndex, token: &TokenTree) -> Result<usize, Error> {
    index.offset(token.span().end()).ok_or_else(unlocated)
}

pub(crate) fn unlocated() -> Error {
    Error::Tokens("a span does not belong to the text being formatted".to_string())
}

/// Rebuilds a source text from byte ranges and generated scaffolding.
///
/// The cursor is what keeps trivia: every range that is not rewritten is copied
/// from the original bytes, so comments and blank lines inside a macro body stay
/// where the author put them. A range that is dropped instead is checked first,
/// which turns "the layout moved a comment somewhere unexpected" into an error
/// rather than into silent loss.
pub(crate) struct Emitter<'a> {
    pub src: &'a str,
    pub index: LineIndex<'a>,
    pub out: String,
    pub at: usize,
    /// Source range of every macro invocation this pass rewrote.
    pub macros: Vec<Range<usize>>,
}

impl<'a> Emitter<'a> {
    fn new(src: &'a str, options: &'a Options) -> Self {
        let _ = options;
        Self {
            src,
            index: LineIndex::new(src),
            out: String::with_capacity(src.len() + 256),
            at: 0,
            macros: Vec::new(),
        }
    }

    pub fn copy_to(&mut self, offset: usize) {
        if offset > self.at {
            self.out.push_str(&self.src[self.at..offset]);
            self.at = offset;
        }
    }

    pub fn skip_to(&mut self, offset: usize) {
        if offset > self.at {
            self.at = offset;
        }
    }

    pub fn push(&mut self, text: &str) {
        self.out.push_str(text);
    }

    /// Drop a region of the input, refusing to drop a comment with it.
    pub fn drop_region(&mut self, from: usize, to: usize, what: &str) -> Result<(), Error> {
        if to < from || to > self.src.len() {
            return Err(self.error(from, format!("{what} is out of order")));
        }
        if let Some(text) = self.src.get(from..to)
            && has_comment(text)
        {
            return Err(self.error(from, format!("a comment sits inside the {what}")));
        }
        self.skip_to(to);
        Ok(())
    }

    pub fn error(&self, offset: usize, message: impl Into<String>) -> Error {
        Error::grammar(&self.index, offset, message)
    }

    pub fn offset_of(&self, at: LineColumn) -> Result<usize, Error> {
        self.index.offset(at).ok_or_else(unlocated)
    }

    pub(crate) fn emit_tokens(
        &mut self,
        tokens: &[TokenTree],
        statement_start: bool,
    ) -> Result<(), Error> {
        let mut at = 0;
        while at < tokens.len() {
            if let Some(call) = scan::macro_call_at(tokens, at, &self.index)
                && is_target(&call)
            {
                // A parenthesized call is an expression statement, so a
                // statement-position macro needs a semicolon of its own;
                // the sentinel records that lifting must remove it again.
                let statement = statement_position(tokens, call.start, statement_start);
                // A call that continues with `.` or `?` is the start of an
                // expression, so its parenthesized form needs no semicolon.
                let followed = matches!(
                    tokens.get(call.next),
                    Some(token) if scan::is_punct(token, ';')
                        || scan::is_punct(token, '.')
                        || scan::is_punct(token, '?')
                );
                let added = statement && !followed;
                self.lower_call(&call, added)?;
                if added {
                    self.push(";");
                }
                at = call.next;
                continue;
            }
            match &tokens[at] {
                TokenTree::Group(group) => {
                    let open_end = self.offset_of(group.span_open().end())?;
                    let close_end = self.offset_of(group.span().end())?;
                    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                    self.copy_to(open_end);
                    self.emit_tokens(&inner, group.delimiter() == Delimiter::Brace)?;
                    self.copy_to(close_end);
                }
                token => {
                    let end = end_of(&self.index, token)?;
                    self.copy_to(end);
                }
            }
            at += 1;
        }
        Ok(())
    }

    fn lower_call(&mut self, call: &MacroCall, statement: bool) -> Result<(), Error> {
        if delimiters(call.group.delimiter()).is_none() {
            return Err(self.error(call.path_start, "the macro body has no delimiter"));
        }
        self.macros.push(call.path_start..call.close_end);
        self.copy_to(call.path_start);
        self.copy_to(call.bang);
        self.skip_to(call.bang + 1);
        self.push("!");
        // The body is always lowered inside parentheses: rustfmt formats a
        // parenthesized macro body and lift restores the recorded delimiter.
        self.drop_region(self.at, call.open_end, "macro opening delimiter")?;
        self.push("(");
        let suffix = scan::suffix(call.group.delimiter(), statement);
        match call.name.as_str() {
            scan::JSON_NAME => json::lower_body(self, &call.group, &suffix),
            scan::SELECT_NAME => select::lower_body(self, &call.group, &suffix),
            other => Err(self.error(call.path_start, format!("unsupported macro `{other}!`"))),
        }
        .map_err(|error| error.in_macro(&call.name))?;
        self.copy_to(call.close_start);
        self.drop_region(self.at, call.close_end, "macro closing delimiter")?;
        self.push(")");
        Ok(())
    }
}

/// Restores a formatted text from the sentinels lowering left behind.
pub(crate) struct Lifter<'a> {
    pub text: &'a str,
    pub index: LineIndex<'a>,
    pub options: &'a Options,
    pub out: String,
    pub at: usize,
    pub column: usize,
    /// Dry runs set this: every object must then be written on one line.
    pub inline_only: bool,
    /// Output range of every macro body this pass lifted.
    pub pieces: Vec<Range<usize>>,
}

impl<'a> Lifter<'a> {
    fn new(text: &'a str, options: &'a Options) -> Self {
        Self {
            text,
            index: LineIndex::new(text),
            options,
            out: String::with_capacity(text.len() + 256),
            at: 0,
            column: 0,
            inline_only: false,
            pieces: Vec::new(),
        }
    }

    /// A lifter that only computes text, used to test whether a value fits inline.
    pub fn scratch(other: &Lifter<'a>) -> Self {
        let mut scratch = Lifter::new(other.text, other.options);
        scratch.column = other.column;
        scratch.inline_only = true;
        scratch
    }

    pub fn copy_to(&mut self, offset: usize) {
        if offset > self.at {
            let piece = &self.text[self.at..offset];
            self.out.push_str(piece);
            self.column = advance_column(self.column, piece);
            self.at = offset;
        }
    }

    pub fn skip_to(&mut self, offset: usize) {
        if offset > self.at {
            self.at = offset;
        }
    }

    pub fn push(&mut self, text: &str) {
        self.out.push_str(text);
        self.column = advance_column(self.column, text);
    }

    /// Copy the trivia that precedes the next token.
    ///
    /// A dry run needs the text of one line, so a line break that rustfmt
    /// inserted for the lowered form collapses into a single space. The layout
    /// of the real output never goes through here.
    pub fn copy_gap_to(&mut self, offset: usize) -> Result<(), Error> {
        if offset <= self.at {
            return Ok(());
        }
        let gap = &self.text[self.at..offset];
        if self.inline_only && gap.contains('\n') {
            if has_comment(gap) {
                return Err(self.error(self.at, "a comment sits inside a folded line break"));
            }
            // A space is only written where dropping it could join two tokens.
            let before = self.out.chars().next_back();
            let after = self.text[offset..].chars().next();
            let tight = matches!(before, Some('(' | '[' | '{' | '.'))
                || matches!(after, Some(')' | ']' | '}' | ',' | ';' | ':' | '.' | '?'));
            self.at = offset;
            if !tight {
                self.push(" ");
            }
            return Ok(());
        }
        self.copy_to(offset);
        Ok(())
    }

    /// Drop a region of the input, refusing to drop a comment with it.
    pub fn drop_region(&mut self, from: usize, to: usize, what: &str) -> Result<(), Error> {
        if to < from || to > self.text.len() {
            return Err(self.error(from, format!("{what} is out of order")));
        }
        if let Some(text) = self.text.get(from..to)
            && has_comment(text)
        {
            return Err(self.error(from, format!("a comment sits inside the {what}")));
        }
        self.skip_to(to);
        Ok(())
    }

    pub fn error(&self, offset: usize, message: impl Into<String>) -> Error {
        Error::grammar(&self.index, offset, message)
    }

    pub fn offset_of(&self, at: LineColumn) -> Result<usize, Error> {
        self.index.offset(at).ok_or_else(unlocated)
    }

    /// Column the formatted text gives the first token of a piece.
    pub fn piece_indent(&self, tokens: &[TokenTree]) -> usize {
        let Some(first) = tokens.first() else {
            return 0;
        };
        start_of(&self.index, first).map_or(0, |start| self.index.column(start))
    }

    /// Width of the first line of a piece, as the formatted text spells it.
    pub fn piece_width(&self, tokens: &[TokenTree]) -> usize {
        let Some(first) = tokens.first() else {
            return 0;
        };
        let Ok(start) = start_of(&self.index, first) else {
            return 0;
        };
        self.text[start..]
            .split('\n')
            .next()
            .map_or(0, |line| line.chars().count())
    }

    /// Whether writing a piece now would pass the width the formatter targets.
    pub fn overflows(&self, tokens: &[TokenTree], extra: usize) -> bool {
        self.column + extra + self.piece_width(tokens) > self.options.max_width
    }

    pub(crate) fn walk(&mut self, tokens: &[TokenTree]) -> Result<(), Error> {
        let mut at = 0;
        while at < tokens.len() {
            if let Some(found) = sentinel_match_at(tokens, at) {
                let match_start = end_of(&self.index, &tokens[at])? - "match".len();
                let open_end = self.offset_of(found.group.span_open().end())?;
                let close_end = self.offset_of(found.group.span().end())?;
                self.copy_gap_to(match_start)?;
                self.drop_region(self.at, open_end, "match scrutinee")?;
                match found.prefix {
                    scan::JSON_MATCH => json::lift_body(self, &found.group, &found.name)?,
                    _ => return Err(self.error(open_end, "a select! body cannot appear here")),
                }
                self.skip_to(close_end);
                at += 3;
                continue;
            }
            if let Some(call) = scan::macro_call_at(tokens, at, &self.index)
                && is_target(&call)
            {
                let added = scan::sentinel_match(&call.group)
                    .is_some_and(|found| scan::statement_of(&found.name, found.prefix));
                self.lift_call(&call)?;
                if added
                    && let Some(token) = tokens.get(call.next)
                    && scan::is_punct(token, ';')
                {
                    self.skip_to(end_of(&self.index, token)?);
                }
                at = call.next;
                continue;
            }
            match &tokens[at] {
                TokenTree::Group(group) => {
                    let open_end = self.offset_of(group.span_open().end())?;
                    let close_end = self.offset_of(group.span().end())?;
                    let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                    self.copy_gap_to(self.offset_of(group.span_open().start())?)?;
                    self.copy_to(open_end);
                    self.walk(&inner)?;
                    self.copy_to(close_end);
                }
                token => {
                    self.copy_gap_to(start_of(&self.index, token)?)?;
                    let end = end_of(&self.index, token)?;
                    self.copy_to(end);
                }
            }
            at += 1;
        }
        Ok(())
    }

    fn lift_call(&mut self, call: &MacroCall) -> Result<(), Error> {
        self.copy_gap_to(call.path_start)?;
        let piece_start = self.out.len();
        self.copy_to(call.bang + 1);
        if let Some(found) = scan::sentinel_match(&call.group) {
            let (open, close) = delimiters(found.delimiter)
                .ok_or_else(|| self.error(call.path_start, "the macro body has no delimiter"))?;
            let open_end = self.offset_of(found.group.span_open().end())?;
            self.drop_region(self.at, open_end, "match scrutinee")?;
            if close == "}" {
                self.push(" ");
            }
            self.push(open);
            match found.prefix {
                scan::JSON_MATCH => json::lift_body(self, &found.group, &found.name)?,
                scan::SELECT_MATCH => select::lift_body(self, &found.group, &found.name)?,
                _ => unreachable!(),
            }
            self.drop_region(self.at, call.close_end, "macro body tail")?;
            self.push(close);
            self.pieces.push(piece_start..self.out.len());
            return Ok(());
        }
        let open_end = self.offset_of(call.group.span_open().end())?;
        let close_end = self.offset_of(call.group.span().end())?;
        let inner: Vec<TokenTree> = call.group.stream().into_iter().collect();
        self.copy_to(open_end);
        json::lift_value(self, &inner)?;
        self.copy_to(close_end);
        self.pieces.push(piece_start..self.out.len());
        Ok(())
    }
}

/// A sentinel match found by walking tokens: `match <sentinel> { .. }`.
pub(crate) struct Found {
    pub prefix: &'static str,
    pub name: String,
    pub group: Group,
}

/// Parse a lowered `match <sentinel> { .. }` with `syn`.
///
/// `Punctuated::<Arm, Comma>` cannot be used here: `syn::Arm` consumes its own
/// trailing comma, so only the enclosing match expression parses the arms the
/// way rustfmt wrote them.
pub(crate) fn parse_match(group: &Group, sentinel: &str) -> Result<syn::ExprMatch, syn::Error> {
    let keyword = proc_macro2::Ident::new("match", proc_macro2::Span::call_site());
    let name = proc_macro2::Ident::new(sentinel, proc_macro2::Span::call_site());
    let stream = TokenStream::from_iter([
        TokenTree::Ident(keyword),
        TokenTree::Ident(name),
        TokenTree::Group(group.clone()),
    ]);
    syn::parse2::<syn::ExprMatch>(stream)
}

pub(crate) fn sentinel_match_at(tokens: &[TokenTree], at: usize) -> Option<Found> {
    let Some(TokenTree::Ident(keyword)) = tokens.get(at) else {
        return None;
    };
    if keyword != "match" {
        return None;
    }
    let Some(TokenTree::Ident(name)) = tokens.get(at + 1) else {
        return None;
    };
    let Some(TokenTree::Group(group)) = tokens.get(at + 2) else {
        return None;
    };
    if group.delimiter() != Delimiter::Brace {
        return None;
    }
    let name = name.to_string();
    for prefix in [scan::JSON_MATCH, scan::SELECT_MATCH] {
        if scan::delimiter_of(&name, prefix).is_some() {
            return Some(Found {
                prefix,
                name: name.clone(),
                group: group.clone(),
            });
        }
    }
    None
}

/// Whether a macro call stands where a statement may begin.
pub(crate) fn statement_position(
    tokens: &[TokenTree],
    start: usize,
    statement_start: bool,
) -> bool {
    match start.checked_sub(1).and_then(|before| tokens.get(before)) {
        None => statement_start,
        Some(token) if scan::is_punct(token, ';') => true,
        Some(TokenTree::Group(group)) => group.delimiter() == Delimiter::Brace,
        _ => false,
    }
}

pub(crate) fn is_target(call: &MacroCall) -> bool {
    call.name == scan::JSON_NAME || call.name == scan::SELECT_NAME
}

/// The mode a set of files is formatted in.
pub enum Mode {
    Check,
    Write,
    Stdin,
}

/// Result of formatting a set of files.
pub struct Outcome {
    pub changed: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, Error)>,
}

/// Format one file's text, verifying the result is a fixed point before writing.
pub fn format_file(path: &Path, options: &Options, write: bool) -> Result<bool, Error> {
    let source = std::fs::read_to_string(path)?;
    let formatted = format_source(&source, options)?;
    if formatted == source {
        return Ok(false);
    }
    if options.verify {
        let again = format_source(&formatted, options)?;
        if again != formatted {
            return Err(Error::Verification {
                line: first_difference(&formatted, &again),
                message: "re-formatting the result changes it again".to_string(),
            });
        }
    }
    if write {
        std::fs::write(path, &formatted)?;
    }
    Ok(true)
}

fn first_difference(before: &str, after: &str) -> usize {
    before
        .lines()
        .zip(after.lines())
        .position(|(left, right)| left != right)
        .map_or(1, |line| line + 1)
}

/// Every `.rs` file under the given paths, in a stable order.
///
/// A skip prefix excludes a whole subtree, which is how the root workspace
/// leaves the independent author projects to their own manifest.
pub fn collect(paths: &[PathBuf], skips: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    let skips: Vec<PathBuf> = skips.iter().map(|skip| normalize(skip)).collect();
    let mut files = Vec::new();
    for path in paths {
        collect_into(path, &mut files, &skips)?;
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Drop `.` components so two spellings of the same path compare equal.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        if let std::path::Component::CurDir = component {
            continue;
        }
        out.push(component.as_os_str());
    }
    out
}

fn collect_into(path: &Path, files: &mut Vec<PathBuf>, skips: &[PathBuf]) -> Result<(), Error> {
    let metadata = std::fs::metadata(path)?;
    if metadata.is_file() {
        if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path.to_path_buf());
        }
        return Ok(());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(path)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    entries.sort();
    for entry in entries {
        let name = entry
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if entry.is_dir() && (name.starts_with('.') || is_skipped(&name)) {
            continue;
        }
        if skips.iter().any(|skip| normalize(&entry).starts_with(skip)) {
            continue;
        }
        collect_into(&entry, files, skips)?;
    }
    Ok(())
}

fn is_skipped(name: &str) -> bool {
    matches!(
        name,
        "target" | "node_modules" | "artifacts" | "__pycache__"
    )
}

/// Format every file, reporting failures instead of stopping at the first one.
pub fn run(files: &[PathBuf], mode: &Mode, options: &Options) -> Outcome {
    let write = matches!(mode, Mode::Write);
    let results: std::sync::Mutex<Vec<(PathBuf, Result<bool, Error>)>> =
        std::sync::Mutex::new(Vec::new());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let jobs = options.jobs.max(1).min(files.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let Some(path) = files.get(index) else { break };
                    let result = format_file(path, options, write);
                    results
                        .lock()
                        .expect("results lock")
                        .push((path.clone(), result));
                }
            });
        }
    });
    let mut changed = Vec::new();
    let mut failed = Vec::new();
    for (path, result) in results.into_inner().expect("results lock") {
        match result {
            Ok(true) => changed.push(path),
            Ok(false) => {}
            Err(error) => failed.push((path, error)),
        }
    }
    changed.sort();
    failed.sort_by(|left, right| left.0.cmp(&right.0));
    Outcome { changed, failed }
}
