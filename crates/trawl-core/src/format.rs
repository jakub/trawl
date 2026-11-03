// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DSL query formatter — canonical pretty-printing of parsed queries.
//!
//! Takes a parsed AST and produces a consistently formatted DSL string.
//! One pipe stage per line, tight spacing in the search stage,
//! minimal parenthesization in expressions.
//!
//! [`checked_format_query`] is the only way to format a query: it proves
//! its text before returning it. The text reparses through [`parse`] to the
//! query's canonical form ([`same_query`]), or the call is refused
//! (ADR-0061). A formatted query means what its source meant.
//!
//! Rendering walks an explicit stack, not the call stack: `MAX_EXPR_DEPTH`
//! bounds nesting, not tree depth, so `1+1+…` folds to a left-leaning
//! tree as deep as the chain is long.

use std::fmt::{self, Write};

use crate::ast::{
    AggExpr, BinaryOp, Expr, ExtractMode, ExtractStage, FilterValue, FromSavedStage, LiteralValue,
    PipeStage, Query, SampleMode, SavedRunSelector, Scalar, SearchStage, SearchToken,
    SortDirection, Spanned, TimechartStage, UnaryOp,
};
use crate::canonical_form::same_query;
use crate::parser::primitives::{regex_compile_error, regex_syntax_error};
use crate::parser::suggest::{backtick_dsl_field, quote_dsl_field};
use crate::parser::{MAX_QUERY_LEN, first_cut, parse};

/// Why [`checked_format_query`] returned no text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// Even the compact layout is longer than [`MAX_QUERY_LEN`]. The query
    /// itself still parses and runs: whether a query is admitted never
    /// depends on how long its formatted text would be.
    TooLong {
        /// The length of the compact layout, in bytes.
        bytes: usize,
    },
    /// The rendered text does not reparse to the query's canonical form.
    /// This is a formatter defect, logged at error level when it happens.
    Unfaithful,
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { bytes } => write!(
                f,
                "the formatted query would be {bytes} bytes, over the \
                 {MAX_QUERY_LEN}-byte query limit"
            ),
            Self::Unfaithful => f.write_str(
                "the formatter could not write this query so that it reads back the same",
            ),
        }
    }
}

impl std::error::Error for FormatError {}

/// Format a parsed query, and prove the text means what the query means.
///
/// The readable layout comes first (`format_query`). When it is longer
/// than [`MAX_QUERY_LEN`], the compact layout is tried: every pipe stage
/// still starts its own line, with no space after its `|` or after a
/// list's commas. Whichever fits is reparsed through [`parse`] and
/// compared with [`same_query`], and returned only when the two are equal.
/// Formatting the returned text again returns the same text.
///
/// # Errors
///
/// [`FormatError::TooLong`] when the compact layout is longer than
/// [`MAX_QUERY_LEN`] too, and [`FormatError::Unfaithful`] when the text
/// does not read back as the query.
pub fn checked_format_query(query: &Query) -> Result<String, FormatError> {
    let readable = render(query, Layout::Readable);
    let text = if readable.len() <= MAX_QUERY_LEN {
        readable
    } else {
        let compact = render(query, Layout::Compact);
        if compact.len() > MAX_QUERY_LEN {
            return Err(FormatError::TooLong {
                bytes: compact.len(),
            });
        }
        compact
    };
    verified(query, text)
}

/// `text`, when it reparses to `query`'s canonical form.
///
/// A mismatch is a formatter defect, so it is logged once, at error level,
/// with a hash of the text: never the text, which is the query a client
/// wrote.
fn verified(query: &Query, text: String) -> Result<String, FormatError> {
    if parse(&text).is_ok_and(|reparsed| same_query(query, &reparsed)) {
        return Ok(text);
    }
    tracing::error!(
        query_hash = %text_hash(&text),
        "formatted query text does not read back as the query; refused"
    );
    Err(FormatError::Unfaithful)
}

/// A hex digest of `text`, for a log line that must not carry the text.
fn text_hash(text: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The readable layout, unchecked: what the tests snapshot.
///
/// Test-only on purpose: [`checked_format_query`] is the only way to hand
/// formatted text to a caller, because only it proves the text reads back
/// as the query (ADR-0061).
///
/// Output uses one pipe stage per line with `| ` prefix, and the time
/// clauses first:
/// ```text
/// last=2h service=nginx _severity=error
/// | stats count() by host
/// | where count > 10
/// ```
#[cfg(test)]
fn format_query(query: &Query) -> String {
    render(query, Layout::Readable)
}

/// Render one aggregate as DSL text.
///
/// `pub(crate)` because [`crate::projection`]'s refusal messages quote an
/// aggregate back at the user as text to type — one renderer, so a name
/// trawl offers is a name trawl can parse back (ADR-0013 ruling 7).
pub(crate) fn format_agg_expr(agg: &AggExpr, out: &mut String) {
    let mut renderer = Renderer::new(Layout::Readable);
    renderer.agg(agg);
    out.push_str(&renderer.out);
}

/// Serialize a saved-query name in its DSL literal position.
pub fn format_saved_name(name: &str) -> String {
    use chumsky::Parser as _;
    if crate::parser::primitives::plain_name()
        .parse(name)
        .into_result()
        .is_ok()
    {
        name.to_owned()
    } else {
        format!("\"{}\"", escape_quoted(name))
    }
}

// ---------------------------------------------------------------------------
// Spellings
// ---------------------------------------------------------------------------

/// Field names stored in an AST came through the parser, so the quoted-name
/// grammar has already proved they are representable. Catalog-backed query
/// builders use the fallible renderer directly instead.
///
/// The fallback echoes the raw name rather than panicking: `parse` cannot
/// produce a name [`quote_dsl_field`] refuses — its refusal set IS that
/// production's — so the branch is unreachable for any AST that came from
/// a query, and a hand-built one is not worth a panic inside a response
/// path. [`checked_format_query`] refuses the text it would make.
fn dsl_field(name: &str) -> String {
    quote_dsl_field(name).unwrap_or_else(|| name.to_string())
}

/// A string rendered for a double-quoted DSL position, escaped as the exact
/// inverse of `primitives::quoted_string`: a backslash, a quote, and the
/// three control characters it decodes (`\n`, `\t`, `\r`), so a quoted
/// string in formatted text holds no raw line break or tab (ADR-0061).
/// Other control characters have no escape and print as themselves.
fn escape_quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// Whether an exact filter value needs quoting in the formatted output.
///
/// The complement of `primitives::bare_value`, plus `"` which ends the
/// quoted form — and, because a value position re-lexes more than that one
/// production, four shapes whose bare rendering would parse back as
/// something else:
///
/// - a `#`, which the grammar refuses inside an unquoted token
///   (ADR-0014 ruling 2), so `host="a#b"` rendered bare would come back a
///   parse error rather than the same AST. `//` is deliberately not in
///   this set: `#` is the only comment opener (ruling 3), so a value
///   carries slashes freely and quoting them would change nothing but the
///   spelling;
/// - a value that could open a regex ([`could_open_a_regex`]);
/// - a glob character, which the same production reads as a glob in an
///   unquoted value, so `host!="a*b"` would come back a pattern;
/// - a leading `=`, which fuses with the operator before it:
///   `host>"=a"` written bare is `host>=a`. The operator is read longest
///   first, so no other leading byte fuses (`host<<a` is `<` and `<a`).
///   Quoted whatever the operator, since quoting an exact value never
///   changes it.
///
/// The operator plays no part in the decision (ADR-0060): it is written as
/// typed, and the value's kind alone decides how it is spelled. Keeping the
/// sets aligned is what makes format → reparse yield the same AST for
/// every exact value shape.
fn needs_quoting(s: &str) -> bool {
    s.is_empty()
        || s.starts_with('=')
        || s.contains('"')
        || s.contains('#')
        || could_open_a_regex(s)
        || s.contains('*')
        || s.contains('?')
        || s.chars()
            .any(|c| c.is_ascii_whitespace() || matches!(c, '|' | '(' | ')' | ',' | '`'))
}

/// Whether a bare rendering could re-lex as a regex, or as the start of
/// one. `primitives::regex_shape` is a `/`, a non-empty body carrying no
/// `/` and no cut ([`first_cut`]: a raw line break, or `|` after
/// whitespace), and a closing `/`, and `search::filter_value` tries it at
/// the start of every value, before a list splits. So a value opening
/// with `/` is read up to the next `/` in the text, unless a cut comes
/// first:
///
/// - the value is the whole shape (`/foo/`). `search::filter_value` then
///   requires a byte that ends a bare value, or the end, after it
///   (ADR-0060), and a formatted value is always followed by one of those
///   (a space, a `,` in a list, a group's `)`, or the end);
/// - the value holds no second `/` (`/x`, `/`), so a later `/` closes the
///   shape: in the next list element (`"/a",b/` bare is `/a,b/`, the regex
///   `a,b`) or in a later token (`f="/x" g=a/` bare is the regex `x g=a`).
///
/// A value whose second `/` is followed by more of the value
/// (`/foo/bar/`, `/api//v1`) stays bare: the shape closes inside it and
/// the next byte continues the value. So does one opening with `//`
/// (`//cdn.example.com/x`), whose shape has an empty body and never
/// forms. A cut cannot sit inside a bare value, which holds no
/// whitespace, so the rule needs no case of its own for one; a later cut
/// only stops a shape this rule already quotes for. The rule quotes what
/// would change meaning and nothing more.
fn could_open_a_regex(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('/') else {
        return false;
    };
    match rest.find('/') {
        None => true,
        Some(at) => at > 0 && at + 1 == rest.len(),
    }
}

/// Whether `body` printed as `/body/` after `matches` reads back as the
/// string `body`: the checks the parser runs on a regex literal, in its
/// order. The shape (`primitives::slash_run`: a body of at least one
/// byte, none of them `/`), the body rule ([`first_cut`]), the syntax
/// check, and the compile the parser runs once the query has parsed.
///
/// A string that fails any of them prints quoted instead. A quoted
/// `matches` string is a string literal as a `/body/` one is, so both
/// spellings give the same AST, and the parser counts both against
/// `MAX_QUERY_PATTERNS`.
fn spells_as_regex_literal(body: &str) -> bool {
    !body.is_empty()
        && !body.contains('/')
        && first_cut(body).is_none()
        && regex_syntax_error(body).is_none()
        && regex_compile_error(body).is_none()
}

/// Whether a literal prints with a leading `-`. A unary minus before one
/// takes a space, because `--1` re-lexes as two minuses over `1`, while
/// `- -1` is one minus over the literal `-1`.
fn is_negative_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(LiteralValue::Int(n)) => *n < 0,
        Expr::Literal(LiteralValue::Float(f)) => f.text().starts_with('-'),
        _ => false,
    }
}

/// The precedence of a comparison: `==` and its kin, `matches`, `like`,
/// `ilike`, and an `in` list.
const COMPARISON: u8 = 4;

/// Operator precedence (higher = binds tighter), the levels of
/// `parser::expr`.
fn precedence(op: BinaryOp) -> u8 {
    match op {
        BinaryOp::Or => 1,
        BinaryOp::And => 2,
        BinaryOp::Eq
        | BinaryOp::Ne
        | BinaryOp::Gt
        | BinaryOp::Gte
        | BinaryOp::Lt
        | BinaryOp::Lte
        | BinaryOp::Matches
        | BinaryOp::Like
        | BinaryOp::ILike => COMPARISON,
        BinaryOp::Add | BinaryOp::Sub => 5,
        BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => 6,
    }
}

/// The precedence of `not`, below every comparison.
const NOT: u8 = 3;

/// The precedence of a unary minus, above every binary operator.
const NEG: u8 = 7;

// ---------------------------------------------------------------------------
// The renderer
// ---------------------------------------------------------------------------

/// How a rendering lays a query out.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Layout {
    /// `| ` before each pipe stage and `, ` between list items.
    Readable,
    /// `|` before each pipe stage and `,` between list items, for a query
    /// whose readable layout is longer than [`MAX_QUERY_LEN`]. A 49 KB
    /// `in (…)` list of short ids is 66 KB readable.
    Compact,
}

impl Layout {
    /// What opens each pipe stage's line.
    fn stage_opener(self) -> &'static str {
        match self {
            Self::Readable => "| ",
            Self::Compact => "|",
        }
    }

    /// What separates the items of a list: fields, aggregates, arguments,
    /// assignments, renames and `in` items. A filter value's list takes
    /// no space in either layout.
    fn comma(self) -> &'static str {
        match self {
            Self::Readable => ", ",
            Self::Compact => ",",
        }
    }
}

/// Context for expression formatting — controls how certain literals render.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ExprContext {
    Normal,
    /// RHS of a `matches` operator — a string literal renders as `/regex/`
    /// when that spelling reads back as the same string
    /// ([`spells_as_regex_literal`]).
    MatchesRhs,
}

/// One piece of output still to write. Pieces wait on an explicit stack,
/// so a deep tree costs heap rather than call stack.
enum Piece<'q> {
    Text(&'static str),
    /// A binary operator, with a space on each side.
    Op(BinaryOp),
    /// A search token. `under_not` when it sits directly under `NOT`.
    Token {
        token: &'q SearchToken,
        under_not: bool,
    },
    /// An expression, parenthesised when it binds looser than `min_prec`.
    Expr {
        expr: &'q Expr,
        min_prec: u8,
        ctx: ExprContext,
    },
}

/// Renders one query, or one aggregate, into `out`.
struct Renderer<'q> {
    out: String,
    layout: Layout,
    /// Pieces still to write, the next one last.
    pending: Vec<Piece<'q>>,
}

fn render(query: &Query, layout: Layout) -> String {
    let mut renderer = Renderer::new(layout);
    renderer.search(&query.search);
    for stage in &query.pipeline {
        if !renderer.out.is_empty() {
            renderer.out.push('\n');
        }
        renderer.out.push_str(layout.stage_opener());
        renderer.stage(&stage.node);
    }
    renderer.out
}

impl<'q> Renderer<'q> {
    fn new(layout: Layout) -> Self {
        Self {
            out: String::new(),
            layout,
            pending: Vec::new(),
        }
    }

    /// Write every pending piece, in order.
    fn drain(&mut self) {
        while let Some(piece) = self.pending.pop() {
            match piece {
                Piece::Text(text) => self.out.push_str(text),
                Piece::Op(op) => {
                    let _ = write!(self.out, " {op} ");
                }
                Piece::Token { token, under_not } => self.token(token, under_not),
                Piece::Expr {
                    expr,
                    min_prec,
                    ctx,
                } => self.expr(expr, min_prec, ctx),
            }
        }
    }

    // -- search stage -------------------------------------------------------

    /// The time clauses print first, in the order `last`, `earliest`,
    /// `latest`, and the groups after them (ADR-0061). A clause applies to
    /// the whole search wherever it sat, and printed after the groups it
    /// would change the token before it: `last=1h NOT` is the search word
    /// `NOT`, while `NOT last=1h` puts the clause under `NOT`.
    fn search(&mut self, search: &'q SearchStage) {
        let mut clauses = Vec::new();
        if let Some(ref tf) = search.time_filter {
            clauses.push(format!("last={}", tf.node.duration));
        }
        if let Some(ref earliest) = search.earliest {
            clauses.push(format!("earliest=\"{}\"", escape_quoted(&earliest.node)));
        }
        if let Some(ref latest) = search.latest {
            clauses.push(format!("latest=\"{}\"", escape_quoted(&latest.node)));
        }
        self.out.push_str(&clauses.join(" "));

        if search.groups.iter().any(|g| !g.is_empty()) && !self.out.is_empty() {
            self.out.push(' ');
        }
        self.push_groups(&search.groups);
        self.drain();
    }

    /// Queue OR-separated groups of space-joined tokens.
    fn push_groups(&mut self, groups: &'q [Vec<Spanned<SearchToken>>]) {
        for (i, group) in groups.iter().enumerate().rev() {
            for (j, token) in group.iter().enumerate().rev() {
                self.pending.push(Piece::Token {
                    token: &token.node,
                    under_not: false,
                });
                if j > 0 {
                    self.pending.push(Piece::Text(" "));
                }
            }
            if i > 0 && !group.is_empty() {
                self.pending.push(Piece::Text(" OR "));
            }
        }
    }

    fn token(&mut self, token: &'q SearchToken, under_not: bool) {
        match token {
            SearchToken::FieldFilter(ff) => {
                // `NOT` negates the next token only when a word
                // character, a quote, `-`, `(` or a backtick starts it,
                // so an `@`-name directly under it keeps its backticks.
                let name = if under_not && ff.field.starts_with('@') {
                    backtick_dsl_field(&ff.field).unwrap_or_else(|| ff.field.clone())
                } else {
                    dsl_field(&ff.field)
                };
                let _ = write!(self.out, "{name}{}", ff.op);
                self.filter_value(&ff.value);
            }
            SearchToken::TextSearch(ts) => {
                if ts.negated {
                    self.out.push('-');
                }
                // A parser-produced term is already a bare word the grammar
                // accepts: it carries no whitespace, `|`, backtick or `#`,
                // and does not open with `//` (ADR-0014), so it re-lexes as
                // itself.
                self.out.push_str(&ts.term);
            }
            SearchToken::TimeFilter(tf) => {
                let _ = write!(self.out, "last={}", tf.duration);
            }
            SearchToken::QuotedSearch(qs) => {
                let _ = write!(self.out, "\"{}\"", escape_quoted(&qs.phrase));
            }
            SearchToken::EarliestFilter(val) => {
                let _ = write!(self.out, "earliest=\"{}\"", escape_quoted(val));
            }
            SearchToken::LatestFilter(val) => {
                let _ = write!(self.out, "latest=\"{}\"", escape_quoted(val));
            }
            SearchToken::Not(inner) => {
                self.out.push_str("NOT ");
                self.pending.push(Piece::Token {
                    token: &inner.node,
                    under_not: true,
                });
            }
            SearchToken::Group(groups) => {
                self.out.push('(');
                self.pending.push(Piece::Text(")"));
                self.push_groups(groups);
            }
        }
    }

    fn filter_value(&mut self, value: &FilterValue) {
        match value {
            FilterValue::Single(scalar) => self.scalar(scalar),
            FilterValue::List(items) => {
                // Each element re-lexes on its own, so each is quoted on its
                // own: a `#` or a comma inside one would otherwise come back
                // as a parse error or as two elements.
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        self.out.push(',');
                    }
                    self.scalar(item);
                }
            }
        }
    }

    /// Spell one value by its kind: a regex as `/body/`, a glob bare, and an
    /// exact value quoted when its bare spelling would read as anything else.
    ///
    /// A glob is never quoted, because quoting makes it exact. So a glob whose
    /// bare spelling would re-lex as something else — one holding whitespace
    /// or a `#`, or opening with a complete quoted string — has no spelling
    /// (ADR-0060 names the gap), and nor does a glob that
    /// [could open a regex](could_open_a_regex): one opening with `/` and
    /// holding no second `/`, which a later `/` closes into a regex (`/a*,b/`,
    /// `f=/a* g=b/`). A glob spelled like a regex (`/a*/`) has none either, but
    /// the parser never makes one: a bare value of that shape is a regex, or
    /// an invalid-regex error (`/*/`).
    fn scalar(&mut self, scalar: &Scalar) {
        match scalar {
            Scalar::Regex(body) => {
                let _ = write!(self.out, "/{body}/");
            }
            Scalar::Glob(glob) => self.out.push_str(glob),
            Scalar::Exact(s) if needs_quoting(s) => {
                let _ = write!(self.out, "\"{}\"", escape_quoted(s));
            }
            Scalar::Exact(s) => self.out.push_str(s),
        }
    }

    // -- pipe stages --------------------------------------------------------

    fn stage(&mut self, stage: &'q PipeStage) {
        match stage {
            PipeStage::Stats(s) => self.stats("stats", &s.aggregations, &s.group_by),
            PipeStage::EventStats(s) => self.stats("eventstats", &s.aggregations, &s.group_by),
            PipeStage::Where(s) => {
                self.out.push_str("where ");
                self.expr_root(&s.condition.node);
            }
            PipeStage::Sort(s) => {
                self.out.push_str("sort ");
                for (i, sf) in s.fields.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(self.layout.comma());
                    }
                    if sf.direction == SortDirection::Desc {
                        self.out.push('-');
                    }
                    self.out.push_str(&dsl_field(&sf.field));
                }
            }
            PipeStage::Limit(s) => {
                let _ = write!(self.out, "{} {}", s.keyword, s.count);
            }
            PipeStage::Tail(s) => {
                let _ = write!(self.out, "tail {}", s.count);
            }
            PipeStage::Table(s) => {
                let _ = write!(self.out, "{} ", s.keyword);
                self.field_list(&s.fields);
            }
            PipeStage::Top(s) => self.top_rare("top", s.count, &s.field, &s.by),
            PipeStage::Rare(s) => self.top_rare("rare", s.count, &s.field, &s.by),
            PipeStage::Drop(s) => {
                self.out.push_str("drop ");
                self.field_list(&s.fields);
            }
            PipeStage::Let(s) => {
                let _ = write!(self.out, "{} ", s.keyword);
                for (i, (name, expr)) in s.assignments.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(self.layout.comma());
                    }
                    let _ = write!(self.out, "{} = ", dsl_field(name));
                    self.expr_root(&expr.node);
                }
            }
            PipeStage::Extract(s) => self.extract(s),
            PipeStage::Dedup(s) => {
                self.out.push_str("dedup");
                if !s.fields.is_empty() {
                    self.out.push(' ');
                    self.field_list(&s.fields);
                }
            }
            PipeStage::Timechart(s) => self.timechart(s),
            PipeStage::Pivot(s) => {
                self.out.push_str("pivot ");
                self.agg(&s.aggregation);
                let _ = write!(self.out, " on {}", dsl_field(&s.on_field));
                self.by(&s.by);
            }
            PipeStage::Rename(s) => {
                self.out.push_str("rename ");
                for (i, (old, new)) in s.renames.iter().enumerate() {
                    if i > 0 {
                        self.out.push_str(self.layout.comma());
                    }
                    let _ = write!(self.out, "{} as {}", dsl_field(old), dsl_field(new));
                }
            }
            PipeStage::Sample(s) => match s.mode {
                SampleMode::Percent(p) => {
                    let _ = write!(self.out, "sample {p}%");
                }
                SampleMode::Count(n) => {
                    let _ = write!(self.out, "sample {n}");
                }
            },
            PipeStage::FromSaved(s) => self.saved_source(s),
        }
    }

    /// Render a field-name list (`table a, b`, `by a, b`).
    fn field_list(&mut self, fields: &[String]) {
        for (i, field) in fields.iter().enumerate() {
            if i > 0 {
                self.out.push_str(self.layout.comma());
            }
            self.out.push_str(&dsl_field(field));
        }
    }

    /// A ` by a, b` clause, when there are fields to group by.
    fn by(&mut self, fields: &[String]) {
        if !fields.is_empty() {
            self.out.push_str(" by ");
            self.field_list(fields);
        }
    }

    fn top_rare(&mut self, kw: &str, count: u64, field: &str, by: &[String]) {
        let _ = write!(self.out, "{kw} {count} {}", dsl_field(field));
        self.by(by);
    }

    fn stats(&mut self, keyword: &str, aggs: &'q [AggExpr], group_by: &[String]) {
        let _ = write!(self.out, "{keyword} ");
        self.agg_list(aggs);
        self.by(group_by);
    }

    fn agg_list(&mut self, aggs: &'q [AggExpr]) {
        for (i, agg) in aggs.iter().enumerate() {
            if i > 0 {
                self.out.push_str(self.layout.comma());
            }
            self.agg(agg);
        }
    }

    fn agg(&mut self, agg: &'q AggExpr) {
        let _ = write!(self.out, "{}(", agg.function);
        for (i, arg) in agg.args.iter().enumerate() {
            if i > 0 {
                self.out.push_str(self.layout.comma());
            }
            self.expr_root(&arg.node);
        }
        self.out.push(')');
        if let Some(ref alias) = agg.alias {
            let _ = write!(self.out, " as {}", dsl_field(alias));
        }
    }

    fn extract(&mut self, s: &ExtractStage) {
        self.out.push_str(s.keyword);
        match &s.mode {
            ExtractMode::Regex(pattern) => {
                let _ = write!(self.out, " \"{pattern}\"");
            }
            ExtractMode::KeyValue { separator } => {
                self.out.push_str(" kv");
                if *separator != '=' {
                    let _ = write!(self.out, " sep=\"{separator}\"");
                }
            }
        }
        if let Some(ref field) = s.source_field {
            let _ = write!(self.out, " from {}", dsl_field(field));
        }
    }

    fn timechart(&mut self, s: &'q TimechartStage) {
        self.out.push_str("timechart");
        if let Some(on) = &s.on {
            let _ = write!(self.out, " on {}", dsl_field(on));
        }
        if let Some(span) = s.span {
            let _ = write!(self.out, " span={span}");
        }
        self.out.push(' ');
        self.agg_list(&s.aggregations);
        self.by(&s.group_by);
    }

    /// A `from saved` stage.
    fn saved_source(&mut self, s: &FromSavedStage) {
        let _ = write!(self.out, "from saved {}", format_saved_name(&s.name));
        match s.run {
            SavedRunSelector::Latest => {} // default, don't emit
            SavedRunSelector::All => self.out.push_str(" run=all"),
            SavedRunSelector::Specific(id) => {
                let _ = write!(self.out, " run={id}");
            }
        }
    }

    // -- expressions --------------------------------------------------------

    /// Render a whole expression, in a position that needs no parentheses.
    fn expr_root(&mut self, expr: &'q Expr) {
        self.pending.push(Piece::Expr {
            expr,
            min_prec: 0,
            ctx: ExprContext::Normal,
        });
        self.drain();
    }

    /// Write what comes before an expression's children and queue the rest,
    /// the last piece first.
    fn expr(&mut self, expr: &'q Expr, min_prec: u8, ctx: ExprContext) {
        match expr {
            Expr::Binary { lhs, op, rhs } => {
                let prec = precedence(*op);
                let parens = self.open(prec < min_prec);
                // The grammar reads one comparison suffix per expression,
                // so both operands of a comparison print a level above it:
                // `(a == b) == c` keeps its parentheses. Every other
                // operator is left-associative, so only an
                // equal-precedence right operand needs them.
                let (lhs_prec, rhs_prec) = if prec == COMPARISON {
                    (prec + 1, prec + 1)
                } else {
                    (prec, prec + 1)
                };
                let rhs_ctx = if *op == BinaryOp::Matches {
                    ExprContext::MatchesRhs
                } else {
                    ExprContext::Normal
                };
                self.close(parens);
                self.pending.push(Piece::Expr {
                    expr: &rhs.node,
                    min_prec: rhs_prec,
                    ctx: rhs_ctx,
                });
                self.pending.push(Piece::Op(*op));
                self.pending.push(Piece::Expr {
                    expr: &lhs.node,
                    min_prec: lhs_prec,
                    ctx: ExprContext::Normal,
                });
            }
            Expr::Unary { op, operand } => {
                let op_prec = match op {
                    UnaryOp::Not => NOT,
                    UnaryOp::Neg => NEG,
                };
                let parens = self.open(op_prec < min_prec);
                match op {
                    UnaryOp::Not => self.out.push_str("not "),
                    UnaryOp::Neg if is_negative_literal(&operand.node) => {
                        self.out.push_str("- ");
                    }
                    UnaryOp::Neg => self.out.push('-'),
                }
                self.close(parens);
                self.pending.push(Piece::Expr {
                    expr: &operand.node,
                    min_prec: op_prec,
                    ctx: ExprContext::Normal,
                });
            }
            Expr::FunctionCall { name, args } => {
                let _ = write!(self.out, "{name}(");
                self.pending.push(Piece::Text(")"));
                self.push_list(args);
            }
            Expr::FieldRef(name) => self.out.push_str(&dsl_field(name)),
            Expr::Literal(lit) => self.literal(lit, ctx),
            Expr::InList { expr, list } => {
                // An `in` list is a comparison suffix: under an operator that
                // binds tighter it needs parentheses, so `a + (b in (1))`
                // does not print as `(a + b) in (1)` does.
                let parens = self.open(COMPARISON < min_prec);
                self.close(parens);
                self.pending.push(Piece::Text(")"));
                self.push_list(list);
                self.pending.push(Piece::Text(" in ("));
                self.pending.push(Piece::Expr {
                    expr: &expr.node,
                    min_prec: COMPARISON + 1,
                    ctx: ExprContext::Normal,
                });
            }
        }
    }

    /// Write `(` when `parens`, and say whether it did.
    fn open(&mut self, parens: bool) -> bool {
        if parens {
            self.out.push('(');
        }
        parens
    }

    /// Queue the `)` that [`Renderer::open`] opened. Called before the
    /// node's children are queued, so it comes off the stack after them.
    fn close(&mut self, parens: bool) {
        if parens {
            self.pending.push(Piece::Text(")"));
        }
    }

    /// Queue comma-separated expressions, each needing no parentheses.
    fn push_list(&mut self, items: &'q [Spanned<Expr>]) {
        for (i, item) in items.iter().enumerate().rev() {
            self.pending.push(Piece::Expr {
                expr: &item.node,
                min_prec: 0,
                ctx: ExprContext::Normal,
            });
            if i > 0 {
                self.pending.push(Piece::Text(self.layout.comma()));
            }
        }
    }

    fn literal(&mut self, lit: &LiteralValue, ctx: ExprContext) {
        match lit {
            LiteralValue::String(s) => {
                if ctx == ExprContext::MatchesRhs && spells_as_regex_literal(s) {
                    let _ = write!(self.out, "/{s}/");
                } else {
                    let _ = write!(self.out, "\"{}\"", escape_quoted(s));
                }
            }
            LiteralValue::Int(n) => {
                let _ = write!(self.out, "{n}");
            }
            LiteralValue::Float(n) => {
                let _ = write!(self.out, "{n}");
            }
            LiteralValue::Bool(b) => {
                let _ = write!(self.out, "{b}");
            }
            LiteralValue::Null => self.out.push_str("null"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse → format helper.
    fn fmt(input: &str) -> String {
        let query = crate::parser::parse(input).unwrap();
        format_query(&query)
    }

    // ── search stage ──────────────────────────────────────────────

    #[test]
    fn bare_star() {
        insta::assert_snapshot!(fmt("*"), @"*");
    }

    #[test]
    fn single_field_filter() {
        insta::assert_snapshot!(fmt("service=nginx"), @"service=nginx");
    }

    #[test]
    fn multiple_field_filters() {
        insta::assert_snapshot!(fmt("service=nginx   _severity=error   status>=400"), @"service=nginx _severity=error status>=400");
    }

    #[test]
    fn values_refused_by_the_bare_grammar_round_trip_quoted() {
        for dsl in [
            r#"host="a`b""#,
            r#"host="a b""#,
            "host=\"a\tb\"",
            r#"host="a,b""#,
            r#"host="a|b""#,
            r#"host="a(b)""#,
            r#"host="""#,
        ] {
            let once = fmt(dsl);
            let twice = fmt(&once);
            assert_eq!(once, twice, "{dsl}: formatter must be idempotent");
        }
        assert_eq!(fmt("host=web-01"), "host=web-01");
    }

    /// The invariant the whole formatter rests on: format → reparse
    /// yields the same AST. Idempotence alone does not prove it — a value
    /// that re-lexes as a different kind reformats to itself while meaning
    /// something else — so this compares the parsed queries by canonical form, spans aside.
    ///
    /// [`generated::a_formatted_query_reparses_to_the_same_ast`] proves it
    /// for generated queries. These are shrunk cases it and its
    /// search-only predecessor found, kept so they never depend on it
    /// finding them again.
    #[test]
    fn a_formatted_filter_value_reparses_to_the_same_ast() {
        fn parsed(dsl: &str) -> Query {
            crate::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl}: {e:?}"))
        }

        for dsl in [
            // a pipeline: an expression literal holding the comment opener
            r#"* | where message == "a#b""#,
            // a text search beside a regex that closes its group tight
            "(a OR msg=/x/ )",
            // a leading `=` would fuse with an ordered operator: `a>=`
            r#"a>"=""#,
            r#"a<"=x""#,
            // an exact value opening with `/` and holding no second `/`
            // would be closed into a regex by a later token's `/`
            r#"((_="/") (_=/ /))"#,
            r#"f="/x" g=a/"#,
        ] {
            let formatted = fmt(dsl);
            assert!(
                crate::canonical_form::same_query(&parsed(dsl), &parsed(&formatted)),
                "{dsl} formatted as {formatted}"
            );
            assert_eq!(formatted, fmt(&formatted), "{dsl}: not idempotent");
        }
    }

    /// Generated queries for the format → reparse property.
    ///
    /// The domain is every AST shape `parse` makes, and nothing is left out
    /// for being hard to spell. A search stage of field filters, text
    /// terms, quoted phrases, `NOT` and groups, nested at most three deep,
    /// with or without each time slot; and a pipeline of every stage, with
    /// expressions over every operator, literal kind, function call and
    /// `in` list. Field names are drawn from every shape the name
    /// productions read: bare and dotted, keyword-named or keyword-prefixed
    /// (`OR.a`, `not`, `last`), `@` names, and names only backticks can
    /// spell. The formatter decides how to spell each one.
    ///
    /// An exclusion is allowed only where the parser cannot make the AST,
    /// and each names the parse error or the production that makes it
    /// unreachable:
    ///
    /// - a name the quoted-name production refuses: empty, or holding a
    ///   character that cannot be shown as itself (`primitives::quoted_name`);
    /// - a written name (`let` and `rename` targets, aliases) opening with
    ///   `_`: the reserved-namespace error of `pipe::assignment_target`.
    ///   Two `let` or `rename` targets that fold to one name: the
    ///   duplicate-target error (`schema::duplicate_target_message`);
    /// - a text term the grammar reads as something else. `text_search`
    ///   ends a term at whitespace, `|`, a parenthesis or a backtick, and a
    ///   positive one at `"` too. A `#`, or a positive term opening with
    ///   `//`, is a comment-opener error (`search::check_term`). A positive
    ///   term opening with `-` is the negated arm's. A term opening with a
    ///   field name and an operator is `field_filter`'s, tried first. A
    ///   term whose first word is `OR` or `or` is the group separator
    ///   (`search_stage`'s and the group's `or_marker`). A term whose first
    ///   word is `NOT`, followed by `-` or a letter, is `not_token`'s, and
    ///   so is the bare word `NOT` when another token or an `OR` follows it;
    /// - `NOT` over a token whose text does not open with a word
    ///   character, a quote, `-`, `(` or a backtick: `not_token`'s
    ///   lookahead reads that `NOT` as a search word;
    /// - a time clause inside a group or under `NOT`: the nested-clause
    ///   error (ADR-0061 ruling 2). At the top level `search_stage` hoists
    ///   every clause into the stage's slots, so the groups hold none;
    /// - an empty group: `search_stage` and the group production drop
    ///   them, and a group of only `OR` separators is an error (ADR-0061
    ///   ruling 4);
    /// - a negative number literal anywhere but directly under a unary
    ///   minus: `unary_neg` is tried before the literal, so `-1` is a minus
    ///   over `1`, and only `- -1` reaches the signed literal;
    /// - a glob that needs quoting, or that opens with `/` and holds no
    ///   second `/` ([`super::super::could_open_a_regex`]): quoting makes a
    ///   value exact, and a later `/` closes such a glob into a regex
    ///   (ADR-0060). A bare value that opens with `"` is an error (ADR-0061
    ///   ruling 3);
    /// - a regex body that holds `/`, does not compile, or holds a cut (a
    ///   raw line break, or `|` after whitespace): `regex_shape` and the
    ///   regex body errors (ADR-0061 ruling 1);
    /// - a glob, a regex or a list under an ordered operator: the
    ///   ordered-operator errors (ADR-0060);
    /// - an `extract` pattern or `kv` separator holding `"`: the raw quoted
    ///   string ends at the first `"`, so neither can carry one. A
    ///   separator other than one character: the parser reads it as `=`,
    ///   so the AST holds the default.
    mod generated {
        use proptest::prelude::*;

        use crate::ast::{
            AggExpr, BinaryOp, DedupStage, DropStage, EventStatsStage, Expr, ExtractMode,
            ExtractStage, FieldFilter, FilterOp, FilterValue, FloatLiteral, FromSavedStage,
            LetStage, LimitStage, LiteralValue, PipeStage, PivotStage, Query, QuotedSearch,
            RareStage, RenameStage, SampleMode, SampleStage, SavedRunSelector, Scalar, SearchStage,
            SearchToken, SortDirection, SortField, SortStage, Spanned, StatsStage, TableStage,
            TailStage, TextSearch, TimeFilter, TimeUnit, TimechartStage, TopStage, TrawlDuration,
            UnaryOp, WhereStage,
        };
        use crate::canonical_form::same_query;
        use crate::parser::suggest::{
            EXPRESSION_KEYWORDS, FIELD_START_KEYWORDS, GRAMMAR_KEYWORDS, backtick_dsl_field,
        };

        fn spanned<T>(node: T) -> Spanned<T> {
            Spanned::new(node, 0..0)
        }

        // ── names ────────────────────────────────────────────────────

        fn field() -> impl Strategy<Value = String> + Clone {
            let keywords: Vec<&'static str> = FIELD_START_KEYWORDS
                .iter()
                .chain(GRAMMAR_KEYWORDS)
                .chain(EXPRESSION_KEYWORDS)
                .copied()
                .collect();
            let quoted_only = vec![
                "a", " ", "`", "#", "-", "é", "1", ".", "@", "\"", "|", "(", "=", "/",
            ];
            prop_oneof![
                4 => "[A-Za-z_][A-Za-z0-9_]{0,4}(\\.[a-z_][a-z0-9_]{0,3})?",
                2 => (prop::sample::select(keywords), prop::sample::select(vec!["", ".a", ".b.c"]))
                    .prop_map(|(word, rest)| format!("{word}{rest}")),
                1 => "@[a-z_][a-z0-9_]{0,3}(\\.[a-z][a-z0-9]{0,2})?",
                1 => prop::collection::vec(prop::sample::select(quoted_only), 1..5)
                    .prop_map(|parts| parts.concat()),
            ]
            .prop_filter("the quoted-name production refuses the name", |name| {
                backtick_dsl_field(name).is_some()
            })
        }

        fn fields(len: std::ops::Range<usize>) -> impl Strategy<Value = Vec<String>> + Clone {
            prop::collection::vec(field(), len)
        }

        /// A name a stage writes.
        fn target() -> impl Strategy<Value = String> + Clone {
            field().prop_filter(
                "assignment_target refuses a name in the reserved namespace",
                |name| !crate::schema::is_reserved_name(name),
            )
        }

        fn distinct_targets(names: &[&str]) -> bool {
            crate::schema::duplicate_target_message(names.iter().copied(), "stage").is_none()
        }

        // ── the search stage ─────────────────────────────────────────

        fn exact() -> impl Strategy<Value = Scalar> + Clone {
            let pieces = vec![
                "a", "Z", "0", "-", ".", "_", "/", "*", "?", "#", "\"", ",", "(", ")", "|", " ",
                "\t", "\n", "\r", "`", "\\", "é", "=", "<", ">", "!", "/x/", "//",
            ];
            prop::collection::vec(prop::sample::select(pieces), 0..6)
                .prop_map(|parts| Scalar::Exact(parts.concat()))
        }

        fn glob() -> impl Strategy<Value = Scalar> + Clone {
            let pieces = vec![
                "a", "b9", "-", ".", "_", "/", "\\", "é", "=", "!", "@", "*", "?",
            ];
            let part = || prop::collection::vec(prop::sample::select(pieces.clone()), 0..4);
            (part(), prop::sample::select(vec!["*", "?"]), part())
                .prop_map(|(head, wildcard, tail)| {
                    format!("{}{wildcard}{}", head.concat(), tail.concat())
                })
                .prop_filter("a glob written bare could not open a regex", |s| {
                    !super::super::could_open_a_regex(s)
                })
                .prop_map(Scalar::Glob)
        }

        fn regex() -> impl Strategy<Value = Scalar> + Clone {
            let pieces = vec![
                "a", "z9", "é", " ", ",", "|", "#", "`", "=", "<", "-", ".", ".*", "x+", "b?",
                "[a-c]", "[,|)]", "(a|b)", "(?i)", "\\d", "\\(", "\\)", "\\.", "^", "$",
            ];
            prop::collection::vec(prop::sample::select(pieces), 1..6)
                .prop_map(|parts| parts.concat())
                .prop_filter("a regex body compiles and holds no cut", |body| {
                    crate::parser::primitives::regex_body_error(body).is_none()
                        && crate::parser::first_cut(body).is_none()
                })
                .prop_map(Scalar::Regex)
        }

        fn scalar() -> impl Strategy<Value = Scalar> + Clone {
            prop_oneof![exact(), glob(), regex()]
        }

        fn filter() -> impl Strategy<Value = FieldFilter> + Clone {
            let pattern_op = prop::sample::select(vec![FilterOp::Eq, FilterOp::Ne]);
            let ordered_op = prop::sample::select(vec![
                FilterOp::Gt,
                FilterOp::Gte,
                FilterOp::Lt,
                FilterOp::Lte,
            ]);
            let value = prop_oneof![
                2 => scalar().prop_map(FilterValue::Single),
                1 => prop::collection::vec(scalar(), 2..5).prop_map(FilterValue::List),
            ];
            prop_oneof![
                2 => (field(), pattern_op, value),
                1 => (field(), ordered_op, exact().prop_map(FilterValue::Single)),
            ]
            .prop_map(|(field, op, value)| FieldFilter { field, op, value })
        }

        /// The length of the field name `term` opens with, if any: an
        /// optional `@`, then dot-joined `[A-Za-z_][A-Za-z0-9_]*`
        /// segments (`primitives::ident`).
        fn leading_name(term: &str) -> usize {
            let bytes = term.as_bytes();
            let mut at = usize::from(bytes.first() == Some(&b'@'));
            let start = at;
            loop {
                match bytes.get(at) {
                    Some(c) if c.is_ascii_alphabetic() || *c == b'_' => at += 1,
                    _ => break,
                }
                while bytes
                    .get(at)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
                {
                    at += 1;
                }
                let next_starts_a_segment = bytes.get(at) == Some(&b'.')
                    && bytes
                        .get(at + 1)
                        .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_');
                if !next_starts_a_segment {
                    break;
                }
                at += 1;
            }
            if at == start { 0 } else { at }
        }

        /// Whether `keyword()` would read `word` at the start of `term`:
        /// the word, then anything but an identifier byte.
        fn opens_with_keyword(term: &str, word: &str) -> bool {
            term.strip_prefix(word).is_some_and(|rest| {
                !rest
                    .bytes()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
            })
        }

        /// Whether `text_search` reads `term` back as this term, wherever
        /// it sits (the neighbour rule for a bare `NOT` is
        /// [`not_word_meets_a_token`]).
        fn is_a_term(term: &str, negated: bool) -> bool {
            let ends_a_term =
                |c: char| c.is_ascii_whitespace() || matches!(c, '|' | '(' | ')' | '`');
            if term.is_empty() || term.chars().any(ends_a_term) || term.contains('#') {
                return false;
            }
            if negated {
                return true;
            }
            let name = leading_name(term);
            let opens_a_filter = name > 0
                && ["=", "!=", "<", ">"]
                    .iter()
                    .any(|op| term[name..].starts_with(op));
            let not_negates = opens_with_keyword(term, "NOT")
                && term[3..]
                    .chars()
                    .next()
                    .is_some_and(|c| c == '-' || c.is_alphanumeric());
            let read_otherwise = term.contains('"')
                || term.starts_with("//")
                || (term.starts_with('-') && term.len() > 1)
                || opens_a_filter
                || opens_with_keyword(term, "OR")
                || opens_with_keyword(term, "or")
                || not_negates;
            !read_otherwise
        }

        fn text_term() -> impl Strategy<Value = TextSearch> + Clone {
            let pieces = vec![
                "a", "Z", "0", "-", ".", "_", "/", "*", "?", "\"", "=", "<", ">", "!", "é", "@",
                ",", "NOT", "OR", "or", "\\",
            ];
            (
                prop::collection::vec(prop::sample::select(pieces), 1..5),
                prop::bool::weighted(0.3),
            )
                .prop_map(|(parts, negated)| TextSearch {
                    term: parts.concat(),
                    negated,
                })
                .prop_filter("text_search reads the term back as itself", |t| {
                    is_a_term(&t.term, t.negated)
                })
        }

        fn phrase() -> impl Strategy<Value = QuotedSearch> + Clone {
            exact().prop_map(|scalar| match scalar {
                Scalar::Exact(phrase) => QuotedSearch { phrase },
                _ => unreachable!("exact() makes exact scalars"),
            })
        }

        /// Whether `NOT`'s lookahead accepts the text `token` prints as.
        fn not_can_negate(token: &SearchToken) -> bool {
            match token {
                SearchToken::TextSearch(t) => {
                    t.negated
                        || t.term
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_alphanumeric() || c == '_')
                }
                _ => true,
            }
        }

        fn groups(
            token: impl Strategy<Value = SearchToken> + Clone,
        ) -> impl Strategy<Value = Vec<Vec<Spanned<SearchToken>>>> + Clone {
            prop::collection::vec(prop::collection::vec(token.prop_map(spanned), 1..4), 1..4)
        }

        fn token() -> impl Strategy<Value = SearchToken> + Clone {
            prop_oneof![
                4 => filter().prop_map(SearchToken::FieldFilter),
                3 => text_term().prop_map(SearchToken::TextSearch),
                1 => phrase().prop_map(SearchToken::QuotedSearch),
            ]
            .prop_recursive(3, 24, 4, |inner| {
                prop_oneof![
                    inner
                        .clone()
                        .prop_filter("not_token's lookahead accepts the operand", not_can_negate)
                        .prop_map(|t| SearchToken::Not(Box::new(spanned(t)))),
                    groups(inner).prop_map(SearchToken::Group),
                ]
            })
        }

        /// Whether a positive bare `NOT` in `groups` is followed, in the
        /// printed text, by a token or an `OR`, which `not_token` would
        /// then negate. `followed` says whether something follows the
        /// whole list.
        fn not_word_meets_a_token(groups: &[Vec<Spanned<SearchToken>>], followed: bool) -> bool {
            groups.iter().enumerate().any(|(i, group)| {
                group.iter().enumerate().any(|(j, token)| {
                    let next = j + 1 < group.len() || i + 1 < groups.len() || followed;
                    meets(&token.node, next)
                })
            })
        }

        fn meets(token: &SearchToken, followed: bool) -> bool {
            match token {
                SearchToken::TextSearch(t) => !t.negated && t.term == "NOT" && followed,
                SearchToken::Not(inner) => meets(&inner.node, followed),
                // A group's last token is followed by its `)`.
                SearchToken::Group(groups) => not_word_meets_a_token(groups, false),
                _ => false,
            }
        }

        fn duration() -> impl Strategy<Value = TrawlDuration> + Clone {
            (
                1u64..10_000,
                prop::sample::select(vec![
                    TimeUnit::Seconds,
                    TimeUnit::Minutes,
                    TimeUnit::Hours,
                    TimeUnit::Days,
                    TimeUnit::Weeks,
                ]),
            )
                .prop_map(|(quantity, unit)| TrawlDuration { quantity, unit })
        }

        fn bound() -> impl Strategy<Value = Option<Spanned<String>>> + Clone {
            prop::option::weighted(
                0.2,
                exact().prop_map(|scalar| match scalar {
                    Scalar::Exact(text) => spanned(text),
                    _ => unreachable!("exact() makes exact scalars"),
                }),
            )
        }

        fn search() -> impl Strategy<Value = SearchStage> + Clone {
            (
                prop::collection::vec(prop::collection::vec(token().prop_map(spanned), 1..4), 0..4),
                prop::option::weighted(
                    0.3,
                    duration().prop_map(|duration| spanned(TimeFilter { duration })),
                ),
                bound(),
                bound(),
            )
                .prop_filter(
                    "not_token negates a bare NOT's next token",
                    |(groups, ..)| !not_word_meets_a_token(groups, false),
                )
                .prop_map(|(groups, time_filter, earliest, latest)| SearchStage {
                    groups,
                    time_filter,
                    earliest,
                    latest,
                    hoisted: Vec::new(),
                })
        }

        // ── expressions ──────────────────────────────────────────────

        /// A float token as `primitives::float` reads one: its integer
        /// part takes no leading zero (`text::int`).
        fn float_text() -> impl Strategy<Value = String> + Clone {
            "(0|[1-9][0-9]{0,2})\\.[0-9]{1,3}"
        }

        fn float(text: &str) -> LiteralValue {
            LiteralValue::Float(FloatLiteral::new(
                text.parse().expect("a float token parses"),
                text,
            ))
        }

        fn string() -> impl Strategy<Value = String> + Clone {
            let pieces = vec![
                "a", "/", "[", "\"", "\\", "\n", "\t", "#", " ", "|", "*", "é", "(?i)", "x+",
            ];
            prop::collection::vec(prop::sample::select(pieces), 0..5).prop_map(|p| p.concat())
        }

        /// A literal as an atom: never signed, since `unary_neg` claims
        /// the `-`.
        fn literal() -> impl Strategy<Value = LiteralValue> + Clone {
            prop_oneof![
                (0i64..1000).prop_map(LiteralValue::Int),
                Just(LiteralValue::Int(i64::MAX)),
                float_text().prop_map(|text| float(&text)),
                string().prop_map(LiteralValue::String),
                any::<bool>().prop_map(LiteralValue::Bool),
                Just(LiteralValue::Null),
            ]
        }

        /// A unary minus over a negative literal, the one place the
        /// parser reads a signed one (`- -1`).
        fn minus_a_negative_literal() -> impl Strategy<Value = Expr> + Clone {
            prop_oneof![
                (1i64..1000).prop_map(|n| LiteralValue::Int(-n)),
                float_text().prop_map(|text| float(&format!("-{text}"))),
            ]
            .prop_map(|lit| Expr::Unary {
                op: UnaryOp::Neg,
                operand: Box::new(spanned(Expr::Literal(lit))),
            })
        }

        fn binary_op() -> impl Strategy<Value = BinaryOp> + Clone {
            prop::sample::select(vec![
                BinaryOp::Or,
                BinaryOp::And,
                BinaryOp::Eq,
                BinaryOp::Ne,
                BinaryOp::Gt,
                BinaryOp::Gte,
                BinaryOp::Lt,
                BinaryOp::Lte,
                BinaryOp::Matches,
                BinaryOp::Like,
                BinaryOp::ILike,
                BinaryOp::Add,
                BinaryOp::Sub,
                BinaryOp::Mul,
                BinaryOp::Div,
                BinaryOp::Mod,
            ])
        }

        fn expr() -> impl Strategy<Value = Expr> + Clone {
            let leaf = prop_oneof![
                3 => field().prop_map(Expr::FieldRef),
                3 => literal().prop_map(Expr::Literal),
                1 => minus_a_negative_literal(),
            ];
            leaf.prop_recursive(4, 24, 3, |inner| {
                let boxed = inner.clone().prop_map(|e| Box::new(spanned(e)));
                let list = prop::collection::vec(inner.clone().prop_map(spanned), 0..3);
                prop_oneof![
                    4 => (boxed.clone(), binary_op(), boxed.clone())
                        .prop_map(|(lhs, op, rhs)| Expr::Binary { lhs, op, rhs }),
                    2 => (prop::sample::select(vec![UnaryOp::Not, UnaryOp::Neg]), boxed.clone())
                        .prop_map(|(op, operand)| Expr::Unary { op, operand }),
                    1 => (
                        prop::sample::select(vec!["lower", "length", "coalesce", "if", "abs"]),
                        list,
                    )
                        .prop_map(|(name, args)| Expr::FunctionCall {
                            name: name.to_owned(),
                            args,
                        }),
                    1 => (boxed, prop::collection::vec(inner.prop_map(spanned), 1..3))
                        .prop_map(|(expr, list)| Expr::InList { expr, list }),
                ]
            })
        }

        // ── the pipeline ─────────────────────────────────────────────

        fn agg() -> impl Strategy<Value = AggExpr> + Clone {
            (
                prop::sample::select(vec!["count", "sum", "avg", "max", "dc"]),
                prop::collection::vec(expr().prop_map(spanned), 0..2),
                prop::option::weighted(0.4, target()),
            )
                .prop_map(|(function, args, alias)| AggExpr {
                    function: function.to_owned(),
                    args,
                    alias,
                })
        }

        fn aggs() -> impl Strategy<Value = Vec<AggExpr>> + Clone {
            prop::collection::vec(agg(), 1..3)
        }

        #[allow(clippy::too_many_lines)]
        fn stage() -> impl Strategy<Value = PipeStage> {
            let keyword = |words: Vec<&'static str>| prop::sample::select(words);
            prop_oneof![
                4 => expr().prop_map(|e| PipeStage::Where(WhereStage {
                    condition: spanned(e),
                })),
                2 => (
                    keyword(vec!["let", "eval"]),
                    prop::collection::vec((target(), expr().prop_map(spanned)), 1..3),
                )
                    .prop_filter("the duplicate-target error", |(_, assignments)| {
                        let names: Vec<&str> =
                            assignments.iter().map(|(n, _)| n.as_str()).collect();
                        distinct_targets(&names)
                    })
                    .prop_map(|(keyword, assignments)| PipeStage::Let(LetStage {
                        assignments,
                        keyword,
                    })),
                2 => (aggs(), fields(0..3)).prop_map(|(aggregations, group_by)| {
                    PipeStage::Stats(StatsStage {
                        aggregations,
                        group_by,
                    })
                }),
                1 => (aggs(), fields(0..3)).prop_map(|(aggregations, group_by)| {
                    PipeStage::EventStats(EventStatsStage {
                        aggregations,
                        group_by,
                    })
                }),
                1 => prop::collection::vec(
                    (field(), prop::sample::select(vec![SortDirection::Asc, SortDirection::Desc])),
                    1..3,
                )
                .prop_map(|fields| PipeStage::Sort(SortStage {
                    fields: fields
                        .into_iter()
                        .map(|(field, direction)| SortField { field, direction })
                        .collect(),
                })),
                1 => (keyword(vec!["head", "limit"]), 1u64..100_000)
                    .prop_map(|(keyword, count)| PipeStage::Limit(LimitStage { count, keyword })),
                1 => (1u64..1000).prop_map(|count| PipeStage::Tail(TailStage { count })),
                1 => (keyword(vec!["table", "fields"]), fields(1..3))
                    .prop_map(|(keyword, fields)| PipeStage::Table(TableStage { fields, keyword })),
                1 => (1u64..100, field(), fields(0..3))
                    .prop_map(|(count, field, by)| PipeStage::Top(TopStage { count, field, by })),
                1 => (1u64..100, field(), fields(0..3))
                    .prop_map(|(count, field, by)| PipeStage::Rare(RareStage { count, field, by })),
                1 => fields(1..3).prop_map(|fields| PipeStage::Drop(DropStage { fields })),
                1 => (
                    keyword(vec!["extract", "rex"]),
                    prop_oneof![
                        prop::sample::select(vec![
                            r"(?P<ip>\d+\.\d+)",
                            "(?P<a>x) #|",
                            r"a\",
                            "",
                        ])
                        .prop_map(|p| ExtractMode::Regex(p.to_owned())),
                        prop::sample::select(vec!['=', ':', ';', '|', ' ', 'é', '\\', '#'])
                            .prop_map(|separator| ExtractMode::KeyValue { separator }),
                    ],
                    prop::option::of(field()),
                )
                    .prop_map(|(keyword, mode, source_field)| {
                        PipeStage::Extract(ExtractStage {
                            mode,
                            source_field,
                            keyword,
                        })
                    }),
                1 => fields(0..3).prop_map(|fields| PipeStage::Dedup(DedupStage { fields })),
                1 => (
                    prop::option::of(field()),
                    prop::option::of(duration()),
                    aggs(),
                    fields(0..3),
                )
                    .prop_map(|(on, span, aggregations, group_by)| {
                        PipeStage::Timechart(TimechartStage {
                            on,
                            span,
                            aggregations,
                            group_by,
                        })
                    }),
                1 => (agg(), field(), fields(0..3)).prop_map(|(aggregation, on_field, by)| {
                    PipeStage::Pivot(PivotStage {
                        aggregation,
                        on_field,
                        by,
                    })
                }),
                1 => prop::collection::vec((field(), target()), 1..3)
                    .prop_filter("the duplicate-target error", |renames| {
                        let names: Vec<&str> = renames.iter().map(|(_, n)| n.as_str()).collect();
                        distinct_targets(&names)
                    })
                    .prop_map(|renames| PipeStage::Rename(RenameStage { renames })),
                1 => prop_oneof![
                    (1u64..=100).prop_map(SampleMode::Percent),
                    (1u64..100_000).prop_map(SampleMode::Count),
                ]
                .prop_map(|mode| PipeStage::Sample(SampleStage { mode })),
                1 => (
                    prop_oneof![field(), string()],
                    prop_oneof![
                        Just(SavedRunSelector::Latest),
                        Just(SavedRunSelector::All),
                        (0i64..1000).prop_map(SavedRunSelector::Specific),
                    ],
                )
                    .prop_map(|(name, run)| PipeStage::FromSaved(FromSavedStage { name, run })),
            ]
        }

        fn query() -> impl Strategy<Value = Query> {
            (
                search(),
                prop::collection::vec(stage().prop_map(spanned), 0..4),
            )
                .prop_map(|(search, pipeline)| Query { search, pipeline })
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(1024))]

            /// `parse(format(q))` has `q`'s canonical form, for every
            /// generated query: the formatter spells each token so the
            /// parser reads it back, beside every neighbour and in every
            /// nesting, and formatting the text again gives the same text.
            #[test]
            fn a_formatted_query_reparses_to_the_same_ast(query in query()) {
                let text = super::super::checked_format_query(&query).map_err(|e| {
                    let unchecked = super::super::format_query(&query);
                    TestCaseError::fail(format!("{unchecked:?}: {e}"))
                })?;
                let parsed = crate::parser::parse(&text)
                    .map_err(|e| TestCaseError::fail(format!("{text:?} does not parse: {e:?}")))?;
                prop_assert!(same_query(&parsed, &query), "formatted as {:?}", text);
                prop_assert_eq!(super::super::checked_format_query(&parsed), Ok(text));
            }
        }
    }

    // ── generated query text ──────────────────────────────────────

    /// What formatting `src` came to, when it parses: `Ok` for text that
    /// reparses to the same canonical form and formats again to itself,
    /// or for a too-long refusal; `Err` with what went wrong otherwise.
    /// `None` when `src` does not parse, so there is nothing to format.
    ///
    /// The reparse and the second format are done here rather than
    /// trusted to the checked renderer under test.
    fn format_verdict(src: &str) -> Option<Result<(), String>> {
        let query = crate::parser::parse(src).ok()?;
        let text = match checked_format_query(&query) {
            Ok(text) => text,
            Err(FormatError::TooLong { .. }) => return Some(Ok(())),
            Err(FormatError::Unfaithful) => {
                return Some(Err(format!(
                    "{src:?} formats unfaithfully: {:?}",
                    format_query(&query)
                )));
            }
        };
        let Ok(reparsed) = crate::parser::parse(&text) else {
            return Some(Err(format!(
                "{src:?} formatted as {text:?}, which does not parse"
            )));
        };
        if !same_query(&query, &reparsed) {
            return Some(Err(format!(
                "{src:?} formatted as {text:?}, which means something else"
            )));
        }
        match checked_format_query(&reparsed) {
            Ok(again) if again == text => Some(Ok(())),
            again => Some(Err(format!(
                "{src:?} formatted as {text:?}, and again as {again:?}"
            ))),
        }
    }

    /// Query text built from every token kind ADR-0061 lists, so the
    /// property reaches what the parser accepts rather than what an AST
    /// generator chose to build. Each strategy yields text; whether it
    /// parses is the parser's call, and the property keeps what it
    /// accepts.
    mod text {
        use proptest::prelude::*;

        fn pick(items: &'static [&'static str]) -> impl Strategy<Value = String> + Clone {
            prop::sample::select(items).prop_map(str::to_owned)
        }

        /// Field names: plain and dotted, keyword-prefixed bare and
        /// backticked, `@` names bare and backticked, and backticked
        /// names that need it for a space, a backtick or a `#`.
        const NAMES: &[&str] = &[
            "host",
            "a.b",
            "_x",
            "s9.t_1",
            "status",
            "OR.a",
            "or.a.b",
            "NOT.x",
            "true.a",
            "false.b",
            "null.c",
            "not.d",
            "in.a",
            "by.x",
            "and.y",
            "last.z",
            "@x",
            "@x.y",
            "`OR.a`",
            "`or.a.b`",
            "`OR`",
            "`or`",
            "`NOT`",
            "`NOT.x`",
            "`true.a`",
            "`false`",
            "`null.a`",
            "`not.a`",
            "`not`",
            "`and`",
            "`in`",
            "`matches`",
            "`last`",
            "`earliest`",
            "`latest`",
            "`@x`",
            "`@x.y`",
            "`a b`",
            "`a``b`",
            "`a.b`",
            "`x#y`",
            "`é`",
            "`-a`",
            "`1a`",
        ];

        /// Filter values of every kind: exact bare and quoted (with
        /// quotes, escapes, `#`, a leading `=` or `/`), globs including
        /// slash-opening ones, regexes, and lists that mix them.
        const VALUES: &[&str] = &[
            "web",
            "200",
            "-1",
            "1.5",
            "a-b.c",
            "é",
            r#""a b""#,
            r#""a\"b""#,
            r#""a#b""#,
            r#""""#,
            r#""=x""#,
            r#""/x""#,
            r#""a\nb""#,
            r#""\t""#,
            r#""*""#,
            r#""a*b""#,
            r#""\\""#,
            r#""`""#,
            "*",
            "a*",
            "web-?",
            "/tmp*",
            "/api/*",
            "/a*b",
            "//cdn/*",
            "/x",
            "/",
            "a/b",
            "//cdn/x",
            "/foo/bar/",
            "url=x",
            "a\"b",
            "a\"",
            "/err|warn/",
            "/a b/",
            "/x/",
            "/[a-c]+/",
            "/(?i)x/",
            "/a,b/",
            "/a#b/",
            "/\\d+/",
            "a,b",
            r#"a,"b c",/x/"#,
            "/x/,y",
            "*,b",
            "/tmp*,x",
            r#""a\"b",c"#,
        ];

        /// The ordered operators take neither a pattern nor a list, so the
        /// pattern operators come up more often.
        const FILTER_OPS: &[&str] = &["=", "=", "=", "!=", "!=", ">", ">=", "<", "<="];

        /// Bare text terms, including the words and dashes the grammar
        /// reads specially next to another token.
        const TERMS: &[&str] = &[
            "error",
            "NOT",
            "NOT-",
            "-",
            "-error",
            "--x",
            r#"-"q""#,
            "a*",
            "*",
            "x.y",
            "OR",
            "or",
            "and",
            "a/b",
            "/x",
            "/x/",
            "@x",
            "a\"b",
            "é",
            "NOT-x",
            "NOT.a",
            "NOTx",
            "a=",
            "=a",
            "!x",
            "x!",
            "-NOT",
            "e-1",
            "-//cdn",
            "a,b",
            "-`",
            "NOT`",
            // non-ASCII layout, which a bare word keeps and the grammar's
            // separators skip, beside `NOT` and `-`
            "NOT\u{a0}-",
            "NOT\u{2003}\u{a0}-",
            "NOT\u{a0}",
            "-\u{a0}",
            "-\u{2003}x",
            "\u{a0}-x",
        ];

        const PHRASES: &[&str] = &[
            r#""connection refused""#,
            r#""a\"b""#,
            r#""x\\""#,
            r#""\t""#,
            r#""a\nb""#,
            r#""""#,
            r#""NOT""#,
            r#""OR""#,
            r##""#""##,
            r#""/x""#,
            r#""a|b""#,
            r#""(x)""#,
        ];

        const TIME_CLAUSES: &[&str] = &[
            "last=1h",
            "last=15m",
            "last=7d",
            "last=30s",
            "last=2w",
            r#"earliest="2026-01-01""#,
            r#"latest="now""#,
            r#"earliest="a\"b""#,
            r#"latest="-1h""#,
            r#"earliest="""#,
        ];

        /// What sits between two search tokens: spaces, an `OR`, a line
        /// break, a comment, and non-ASCII layout (U+00A0, U+2003).
        const SEARCH_GAPS: &[&str] = &[
            " ",
            " ",
            " ",
            "  ",
            " OR ",
            " or ",
            "\n",
            " # c\n",
            "\u{a0}",
            "\u{2003}",
            " \u{a0}",
            "\u{2003}\u{a0}",
        ];

        /// What separates `NOT` from what it negates: a space, or the
        /// non-ASCII layout `NOT` accepts and a bare word does not end at.
        const NOT_GAPS: &[&str] = &[" ", " ", " ", "\u{a0}", "\u{2003}", "\u{2003}\u{a0}"];

        fn filter() -> impl Strategy<Value = String> + Clone {
            (pick(NAMES), pick(FILTER_OPS), pick(VALUES))
                .prop_map(|(name, op, value)| format!("{name}{op}{value}"))
        }

        fn joined(
            item: impl Strategy<Value = String> + Clone,
            gaps: &'static [&'static str],
            len: std::ops::Range<usize>,
        ) -> impl Strategy<Value = String> + Clone {
            prop::collection::vec((item, pick(gaps)), len).prop_map(|items| {
                let mut out = String::new();
                for (i, (item, gap)) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(gap);
                    }
                    out.push_str(item);
                }
                out
            })
        }

        fn search_token() -> impl Strategy<Value = String> + Clone {
            // A clause inside parentheses or under `NOT` is a parse error
            // (ADR-0061 ruling 2), so clauses come mostly from the top
            // level ([`search`]) and rarely from here.
            let leaf = prop_oneof![
                8 => filter(),
                6 => pick(TERMS),
                // a term written tight against a filter, so a word ending
                // in `-` can meet a backticked name
                1 => (pick(TERMS), filter()).prop_map(|(t, f)| format!("{t}{f}")),
                2 => pick(PHRASES),
                1 => pick(TIME_CLAUSES),
            ];
            leaf.prop_recursive(3, 16, 4, |inner| {
                prop_oneof![
                    (pick(NOT_GAPS), inner.clone()).prop_map(|(gap, t)| format!("NOT{gap}{t}")),
                    joined(inner.clone(), SEARCH_GAPS, 1..4).prop_map(|g| format!("({g})")),
                    (pick(NOT_GAPS), joined(inner, SEARCH_GAPS, 1..3))
                        .prop_map(|(gap, g)| format!("NOT{gap}({g})")),
                ]
            })
        }

        pub(super) fn search() -> impl Strategy<Value = String> {
            prop_oneof![
                1 => Just(String::new()),
                1 => Just("*".to_owned()),
                8 => joined(
                    prop_oneof![4 => search_token(), 1 => pick(TIME_CLAUSES)],
                    SEARCH_GAPS,
                    1..5,
                ),
            ]
        }

        /// Expression atoms: names, every literal kind (with signs and
        /// strings a regex literal cannot spell), and regex literals.
        const ATOMS: &[&str] = &[
            "host",
            "a.b",
            "@x",
            "`true.a`",
            "`not.a`",
            "`null.a`",
            "`OR.a`",
            "`a b`",
            "`in`",
            "OR.a",
            "1",
            "0",
            "-1",
            "- -1",
            "--1",
            "1.5",
            "-1.5",
            "- -1.5",
            "2.0",
            r#""s""#,
            r#""a/b""#,
            r#""""#,
            r#""[""#,
            r#""a\"b""#,
            r#""a\nb""#,
            r#""a # b""#,
            "true",
            "false",
            "null",
            "/err|warn/",
            "/x/",
            "/a b/",
        ];

        const BINARY_OPS: &[&str] = &[
            "+", "-", "*", "/", "%", "==", "!=", ">", ">=", "<", "<=", "and", "or", "matches",
            "like", "ilike",
        ];

        const FUNCTIONS: &[&str] = &["lower", "length", "coalesce", "abs", "if", "isnotnull"];

        fn expr() -> impl Strategy<Value = String> + Clone {
            pick(ATOMS).prop_recursive(4, 24, 3, |inner| {
                prop_oneof![
                    3 => (inner.clone(), pick(BINARY_OPS), inner.clone())
                        .prop_map(|(a, op, b)| format!("{a} {op} {b}")),
                    2 => (inner.clone(), pick(BINARY_OPS), inner.clone())
                        .prop_map(|(a, op, b)| format!("({a} {op} {b})")),
                    1 => inner.clone().prop_map(|a| format!("not {a}")),
                    1 => inner.clone().prop_map(|a| format!("-{a}")),
                    1 => inner.clone().prop_map(|a| format!("- {a}")),
                    1 => inner.clone().prop_map(|a| format!("({a})")),
                    1 => (inner.clone(), prop::collection::vec(inner.clone(), 1..3))
                        .prop_map(|(a, items)| format!("{a} in ({})", items.join(", "))),
                    1 => (pick(FUNCTIONS), prop::collection::vec(inner, 0..3))
                        .prop_map(|(f, args)| format!("{f}({})", args.join(", "))),
                ]
            })
        }

        /// Names a stage writes: `let` targets, aliases, renames.
        const TARGETS: &[&str] = &[
            "x", "y.z", "`OR.a`", "`true.a`", "`not`", "`a b`", "@t", "`last`", "n",
        ];

        fn agg() -> impl Strategy<Value = String> + Clone {
            prop_oneof![
                Just("count()".to_owned()),
                (pick(&["sum", "avg", "max", "min", "dc"]), expr())
                    .prop_map(|(f, e)| format!("{f}({e})")),
                (pick(&["count", "sum"]), expr(), pick(TARGETS))
                    .prop_map(|(f, e, alias)| format!("{f}({e}) as {alias}")),
            ]
        }

        fn names(len: std::ops::Range<usize>) -> impl Strategy<Value = String> + Clone {
            prop::collection::vec(pick(NAMES), len).prop_map(|names| names.join(", "))
        }

        fn by() -> impl Strategy<Value = String> + Clone {
            prop_oneof![
                Just(String::new()),
                names(1..3).prop_map(|n| format!(" by {n}")),
            ]
        }

        /// Every pipe stage, with expressions where it takes them.
        fn stage() -> impl Strategy<Value = String> {
            prop_oneof![
                4 => expr().prop_map(|e| format!("where {e}")),
                2 => (pick(&["let", "eval"]), prop::collection::vec((pick(TARGETS), expr()), 1..3))
                    .prop_map(|(kw, assignments)| {
                        let assignments: Vec<String> = assignments
                            .into_iter()
                            .map(|(t, e)| format!("{t} = {e}"))
                            .collect();
                        format!("{kw} {}", assignments.join(", "))
                    }),
                2 => (pick(&["stats", "eventstats"]), prop::collection::vec(agg(), 1..3), by())
                    .prop_map(|(kw, aggs, by)| format!("{kw} {}{by}", aggs.join(", "))),
                1 => prop::collection::vec((pick(&["", "-"]), pick(NAMES)), 1..3).prop_map(
                    |fields| {
                        let fields: Vec<String> =
                            fields.into_iter().map(|(d, n)| format!("{d}{n}")).collect();
                        format!("sort {}", fields.join(", "))
                    }
                ),
                1 => pick(&["head 10", "limit 5", "tail 3", "head 1"]),
                1 => (pick(&["table", "fields", "drop"]), names(1..3))
                    .prop_map(|(kw, n)| format!("{kw} {n}")),
                1 => (pick(&["top 5", "rare 2", "top 1"]), pick(NAMES), by())
                    .prop_map(|(kw, n, by)| format!("{kw} {n}{by}")),
                1 => (
                    pick(&["extract", "rex"]),
                    pick(&[r#""(?P<ip>\d+)""#, "kv", r#"kv sep=":""#, r#"kv sep="|""#]),
                    prop_oneof![Just(String::new()), pick(NAMES).prop_map(|n| format!(" from {n}"))],
                )
                    .prop_map(|(kw, mode, from)| format!("{kw} {mode}{from}")),
                1 => prop_oneof![Just(String::new()), names(1..3).prop_map(|n| format!(" {n}"))]
                    .prop_map(|n| format!("dedup{n}")),
                1 => (
                    prop_oneof![Just(String::new()), pick(NAMES).prop_map(|n| format!(" on {n}"))],
                    pick(&["", " span=5m", " span=1h"]),
                    agg(),
                    by(),
                )
                    .prop_map(|(on, span, agg, by)| format!("timechart{on}{span} {agg}{by}")),
                1 => (agg(), pick(NAMES), by())
                    .prop_map(|(agg, on, by)| format!("pivot {agg} on {on}{by}")),
                1 => prop::collection::vec((pick(NAMES), pick(TARGETS)), 1..3).prop_map(|pairs| {
                    let pairs: Vec<String> =
                        pairs.into_iter().map(|(a, b)| format!("{a} as {b}")).collect();
                    format!("rename {}", pairs.join(", "))
                }),
                1 => pick(&["sample 10%", "sample 100", "sample 1"]),
                1 => pick(&[
                    "from saved daily",
                    r#"from saved "a b" run=all"#,
                    "from saved x run=3",
                    r#"from saved "a\"b""#,
                ]),
            ]
        }

        /// What opens a pipe stage: tight or spaced, on the same line
        /// or a new one, or after a comment.
        const STAGE_OPENERS: &[&str] = &[" | ", "|", "\n| ", "\n|", " # c\n| "];

        pub(super) fn query() -> impl Strategy<Value = String> {
            (
                search(),
                prop::collection::vec((pick(STAGE_OPENERS), stage()), 0..4),
            )
                .prop_map(|(search, stages)| {
                    let mut out = search;
                    for (opener, stage) in stages {
                        out.push_str(&opener);
                        out.push_str(&stage);
                    }
                    out
                })
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig {
            cases: 4096,
            max_global_rejects: 1 << 20,
            ..proptest::prelude::ProptestConfig::default()
        })]

        /// Every generated query text that parses formats to text that
        /// reads back as the same query, or is refused as too long
        /// (ADR-0061). 4096 cases count only the inputs that parse: one
        /// that does not is rejected and another drawn.
        #[test]
        fn accepted_text_formats_faithfully(src in text::query()) {
            let verdict = format_verdict(&src);
            proptest::prop_assume!(verdict.is_some(), "does not parse");
            if let Some(Err(why)) = verdict {
                return Err(proptest::test_runner::TestCaseError::fail(why));
            }
        }
    }

    /// Each risky token, written in the search stage, for
    /// [`token_spellings_survive_every_neighbour`].
    const NEIGHBOUR_TOKENS: [(&str, &str); 13] = [
        ("NOT", "NOT"),
        ("NOT-", "NOT-"),
        ("NOT and a no-break space", "NOT\u{a0}"),
        ("NOT, a no-break space and -", "NOT\u{a0}-"),
        (
            "NOT, an em space, a no-break space and -",
            "NOT\u{2003}\u{a0}-",
        ),
        ("-", "-"),
        ("- and a no-break space", "-\u{a0}"),
        ("a slash-opening glob", "path=/tmp*"),
        ("a slash-opening glob with no second slash", "path=/a*b"),
        ("a quote-bearing value", r#"host="a\"b""#),
        ("a bare value holding a quote", "host=a\"b"),
        ("a regex", "msg=/err|warn/"),
        ("a regex with a space", "msg=/a b/"),
    ];

    /// Each context around a risky token, spelled as the text around `{}`.
    const NEIGHBOUR_CONTEXTS: [(&str, &[&str]); 7] = [
        ("a comma", &["{},b", "{} ,b", "f={},b", "x {},b"]),
        ("`)`", &["({})", "(a {})", "(a OR {})", "NOT ({})"]),
        (
            "a backtick field",
            &[
                "{} `f`=1",
                "{} `OR.a`=1",
                "{}`f`=1",
                "{} `@x`=1",
                "{}\u{a0}`f`=1",
                "{}\u{2003}\u{a0}`f`=1",
            ],
        ),
        (
            "non-ASCII layout",
            &[
                "{}\u{a0}x",
                "{}\u{2003}-x",
                "{}\u{a0}-`f`=1",
                "{}\u{a0}(a)",
                "NOT\u{a0}{}",
                "a\u{2003}{} b",
            ],
        ),
        ("a comment", &["{} # note", "{} # note\nx", "{}\n# note\nx"]),
        (
            "an appended pipe stage",
            &[
                "{} | where x / 2 == 1",
                "{}|head 1",
                "{}\n| where a matches /x/",
                "{} | stats count() by `OR.a`",
            ],
        ),
        (
            "a time clause",
            &[
                "{} last=1h",
                "{} earliest=\"2026-01-01\"",
                "{} last=1h | head 1",
                // `NOT` before a clause would put the clause under it,
                // a parse error (ADR-0061 ruling 2), so it also meets
                // one across an `OR`, and one written before it
                "{} OR last=1h",
                "last=1h {}",
            ],
        ),
    ];

    /// Each token whose spelling depends on what follows it, beside each
    /// following context, formats faithfully (ADR-0061). A pair spelled
    /// several ways counts as covered when at least one spelling parses;
    /// every spelling that parses must format faithfully.
    #[test]
    fn token_spellings_survive_every_neighbour() {
        // A regex in an expression, beside the contexts an expression has.
        let expr_rows: [(&str, &[&str]); 4] = [
            (
                "a comma",
                &["* | where f(m matches /a|b/, 1)", "* | where m in (/x/, 1)"],
            ),
            (
                "`)`",
                &[
                    "* | where (m matches /a|b/)",
                    "* | where (m matches \"a/b\")",
                ],
            ),
            (
                "a comment",
                &[
                    "* | where m matches /x/ # note",
                    "* | where m matches /x/\n# note\n| head 1",
                ],
            ),
            (
                "an appended pipe stage",
                &[
                    "* | where m matches /x/ | head 1",
                    "* | where m matches /x/|where y / 2 == 1",
                ],
            ),
        ];

        let mut failures = Vec::new();
        let mut check = |what: String, spellings: Vec<String>| {
            let mut parsed = 0;
            for src in &spellings {
                match format_verdict(src) {
                    None => {}
                    Some(Ok(())) => parsed += 1,
                    Some(Err(why)) => {
                        parsed += 1;
                        failures.push(why);
                    }
                }
            }
            if parsed == 0 {
                failures.push(format!("{what}: no spelling parses: {spellings:?}"));
            }
        };
        for (token_name, token) in NEIGHBOUR_TOKENS {
            for (context_name, templates) in NEIGHBOUR_CONTEXTS {
                let spellings = templates
                    .iter()
                    .map(|template| template.replacen("{}", token, 1))
                    .collect();
                check(format!("{token_name} before {context_name}"), spellings);
            }
        }
        for (context_name, spellings) in expr_rows {
            check(
                format!("a regex expression before {context_name}"),
                spellings.iter().map(|s| (*s).to_owned()).collect(),
            );
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// One named test per formatter defect ADR-0061 lists, each an input
    /// that `parse` accepts and whose formatted text used to mean
    /// something else or not parse at all. Every test goes through
    /// [`assert_faithful`](regressions::assert_faithful), so the module
    /// runs unchanged against an older tree with only the helpers
    /// swapped (the red run in `docs/evidence/issue-332/`).
    mod regressions {
        // ── helpers ──────────────────────────────────────────────────
        use crate::ast::Query;
        use crate::canonical_form::same_query;
        use crate::parser::parse;

        fn parsed(src: &str) -> Query {
            parse(src).unwrap_or_else(|e| panic!("{} must parse: {e:?}", shown(src)))
        }

        /// `text` for a failure message: whole when short, else its
        /// start and its length.
        fn shown(text: &str) -> String {
            match text.char_indices().nth(80) {
                None => format!("{text:?}"),
                Some((at, _)) => format!("{:?}… ({} bytes)", &text[..at], text.len()),
            }
        }

        fn checked(src: &str, query: &Query) -> String {
            super::super::checked_format_query(query)
                .unwrap_or_else(|e| panic!("{}: formatting refused: {e}", shown(src)))
        }

        /// The text `src` formats to.
        fn formatted(src: &str) -> String {
            checked(src, &parsed(src))
        }

        /// `src` formats to text that reparses to the same canonical
        /// form, and formatting that text again gives the same text. The
        /// reparse is repeated here rather than trusted to the renderer
        /// under test.
        fn assert_faithful(src: &str) {
            let query = parsed(src);
            let text = checked(src, &query);
            let reparsed = parse(&text).unwrap_or_else(|e| {
                panic!(
                    "{} formatted as {}, which does not parse: {e:?}",
                    shown(src),
                    shown(&text)
                )
            });
            assert!(
                same_query(&query, &reparsed),
                "{} formatted as {}, which means something else",
                shown(src),
                shown(&text)
            );
            assert!(
                checked(&text, &reparsed) == text,
                "{}: formatting {} again changed it",
                shown(src),
                shown(&text)
            );
        }
        // ── end of helpers ───────────────────────────────────────────

        /// `` `OR.a` `` printed bare re-lexes as the group separator `OR`
        /// and a text search, and the filter disappears.
        #[test]
        fn or_dot_a_keeps_its_filter() {
            assert_faithful(r#"`OR.a`="""#);
        }

        #[test]
        fn lowercase_or_dot_a_b_keeps_its_filter() {
            assert_faithful("`or.a.b`=x");
        }

        /// A literal word claims the first dot segment in an expression:
        /// bare, `true.a` is the literal `true` and then `.a`.
        #[test]
        fn true_dot_a_stays_a_field() {
            assert_faithful("* | where `true.a` == 1");
            assert_faithful("* | let x = `true.a`");
        }

        #[test]
        fn false_dot_a_stays_a_field() {
            assert_faithful("* | where `false.a` == 1");
            assert_faithful("* | let x = `false.a`");
        }

        #[test]
        fn null_dot_a_stays_a_field() {
            assert_faithful("* | where `null.a` == 1");
            assert_faithful("* | let x = `null.a`");
        }

        #[test]
        fn not_dot_a_stays_a_field() {
            assert_faithful("* | where `not.a` == 1");
            assert_faithful("* | let x = `not.a`");
        }

        /// `NOT` negates the next token only when a backtick, a word
        /// character, a quote, `-` or `(` starts it, so a bare `@x` after
        /// `NOT` leaves `NOT` a search word.
        #[test]
        fn not_keeps_a_backticked_at_name() {
            assert_faithful("NOT `@x`=1");
        }

        /// A `matches` string holding `/` cannot be spelled `/body/`.
        #[test]
        fn matches_a_string_holding_a_slash() {
            assert_faithful(r#"* | where m matches "a/b""#);
        }

        /// `//` is not a regex literal: the body is empty.
        #[test]
        fn matches_the_empty_string() {
            assert_faithful(r#"* | where m matches """#);
        }

        /// A quoted `matches` string is never compiled at parse, while a
        /// `/body/` that is not regex syntax is a parse error.
        #[test]
        fn matches_a_string_that_is_not_a_regex() {
            assert_faithful(r#"* | where m matches "[""#);
        }

        /// The grammar takes one comparison suffix per expression.
        #[test]
        fn a_comparison_under_a_comparison() {
            assert_faithful("* | where (a == b) == c");
        }

        /// An `in` list is a comparison-level node, on either side of
        /// another comparison.
        #[test]
        fn a_membership_test_beside_a_comparison() {
            assert_faithful("* | where (a in (1)) == 1");
            assert_faithful("* | where (a == b) in (1)");
        }

        /// `(a + b) in (1)` and `a + (b in (1))` emit different SQL, so
        /// they must not format to the same text.
        #[test]
        fn a_membership_test_under_addition() {
            assert_faithful("* | where (a + b) in (1)");
            assert_faithful("* | where a + (b in (1))");
            assert_ne!(
                formatted("* | where (a + b) in (1)"),
                formatted("* | where a + (b in (1))")
            );
        }

        /// `- -1` is a minus over the literal `-1`; `--1` is two minuses
        /// over `1`.
        #[test]
        fn a_minus_before_a_negative_literal() {
            assert_faithful("* | where a == - -1");
            assert_faithful("* | where a == - -1.5");
            assert_faithful("* | where a == --1");
        }

        /// A search term `NOT` after a time clause: printed before the
        /// clause, it negates the clause.
        #[test]
        fn a_trailing_not_after_a_time_clause() {
            assert_faithful("last=1h NOT");
        }

        /// `*` and 8 000 `|head 1` stages, 56 001 bytes. Readable, each
        /// stage gains a line break and a space, 72 001 bytes and past the
        /// query limit, so the compact layout carries it.
        #[test]
        fn a_56_kb_head_chain() {
            assert_faithful(&format!("*{}", "|head 1".repeat(8000)));
        }

        /// Fixed before ADR-0061 (ADR-0060), kept so it stays fixed: a
        /// regex closes its group tight.
        #[test]
        fn a_regex_before_a_closing_parenthesis() {
            assert_faithful("(a OR msg=/x/ )");
        }

        /// Fixed before ADR-0061 (ADR-0060), kept so it stays fixed: a
        /// value that opens with `"` and never closes it has no faithful
        /// spelling, so it does not parse.
        #[test]
        fn an_unclosed_quote_under_an_ordered_operator() {
            assert!(parse(r#"host<"*"#).is_err());
        }
    }

    // ── the checked renderer ──────────────────────────────────────

    /// The head chain of [`regressions::a_56_kb_head_chain`]: its readable
    /// layout is past the query limit, so the compact one is returned,
    /// still one stage per line.
    #[test]
    fn a_head_chain_past_the_limit_takes_the_compact_layout() {
        let query = crate::parser::parse(&format!("*{}", "|head 1".repeat(8000))).expect("parses");
        assert_eq!(format_query(&query).len(), 72_001);
        let text = checked_format_query(&query).expect("the compact layout fits");
        assert_eq!(text, format!("*{}", "\n|head 1".repeat(8000)));
        assert!(text.len() <= MAX_QUERY_LEN);
    }

    /// Every stage the compact layout prints still starts its own line,
    /// and every list it shortens reads back the same.
    #[test]
    fn the_compact_layout_reads_back_the_same() {
        for dsl in [
            "a=1,2 b=\"x y\" | stats count() as n, avg(d) by a, b | sort a, -b",
            "* | table a, b | fields `c d`, e | drop f, g | dedup h, i",
            "* | let x = f(a, b), y = - -1 | where x in (1, 2, 3) and y > 0",
            "* | rename a as b, c as d | top 5 a by b, c | rare 2 a by b, c",
            "* | timechart span=5m count(), sum(x) by a, b | pivot count() on s by a, b",
            "* | eventstats sum(x) as s, max(y) by a, b",
        ] {
            let query = crate::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
            let compact = render(&query, Layout::Compact);
            let reparsed = crate::parser::parse(&compact)
                .unwrap_or_else(|e| panic!("{dsl} compact as {compact:?}: {e:?}"));
            assert!(
                same_query(&query, &reparsed),
                "{dsl} compact as {compact:?} means something else"
            );
            assert!(!compact.contains(", "), "{compact:?}");
            for line in compact.lines().skip(1) {
                assert!(
                    line.starts_with('|') && !line.starts_with("| "),
                    "{compact:?}"
                );
            }
            assert_eq!(compact.lines().count(), query.pipeline.len() + 1);
        }
    }

    /// Escaping grows a quoted string, so a query under the limit can
    /// format past it in both layouts. It is refused with the length, and
    /// no shortened text comes back.
    #[test]
    fn a_too_long_rendering_is_refused_not_truncated() {
        let dsl = format!("\"{}\"", "\t".repeat(40_000));
        let query = crate::parser::parse(&dsl).expect("parses");
        let refused = checked_format_query(&query);
        assert_eq!(refused, Err(FormatError::TooLong { bytes: 80_002 }));
        let message = refused.unwrap_err().to_string();
        assert!(
            message.contains("80002") && message.contains("65536"),
            "{message}"
        );
    }

    /// Held by every test here that fires the unfaithful-rendering log.
    /// tracing caches each callsite's interest, and a callsite that
    /// another thread is still registering when a test installs its
    /// capturing dispatcher can keep the interest it computed without
    /// that dispatcher, so the capture would see nothing.
    static UNFAITHFUL_LOG: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// One event a test dispatcher saw: its level, and each field's name
    /// and rendered value.
    type Seen = (tracing::Level, Vec<(String, String)>);

    /// The events a test dispatcher saw.
    #[derive(Clone, Default)]
    struct Events(std::sync::Arc<std::sync::Mutex<Vec<Seen>>>);

    impl tracing::Subscriber for Events {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }
        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            struct Fields(Vec<(String, String)>);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    self.0
                        .push((field.name().to_string(), format!("{value:?}")));
                }
            }
            let mut fields = Fields(Vec::new());
            event.record(&mut fields);
            self.0
                .lock()
                .expect("events lock")
                .push((*event.metadata().level(), fields.0));
        }
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Text that reads back as another query is refused, through the
    /// check every rendering passes ([`verified`]). A faithful text that
    /// differs only in layout passes it.
    #[test]
    fn a_candidate_that_reads_back_differently_is_refused() {
        let _log = UNFAITHFUL_LOG
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let query = crate::parser::parse("a=1 | head 2").expect("parses");
        assert_eq!(
            verified(&query, "a=2\n| head 2".to_string()),
            Err(FormatError::Unfaithful)
        );
        assert_eq!(
            verified(&query, "a=1\n| head 2 | head 2".to_string()),
            Err(FormatError::Unfaithful)
        );
        assert_eq!(
            verified(&query, "a=1 |".to_string()),
            Err(FormatError::Unfaithful)
        );
        assert_eq!(
            verified(&query, "a=1 # note\n|head 2".to_string()),
            Ok("a=1 # note\n|head 2".to_string())
        );
    }

    /// An AST the parser never makes (a text term holding `=`) renders as
    /// a field filter. The checked renderer refuses it and logs one error
    /// carrying a hash of the text, never the text.
    #[test]
    fn an_unfaithful_rendering_is_refused_and_logged_without_its_text() {
        let mut query = crate::parser::parse("x").expect("parses");
        query.search.groups[0][0].node = SearchToken::TextSearch(crate::ast::TextSearch {
            term: "secret=b".to_string(),
            negated: false,
        });
        assert_eq!(format_query(&query), "secret=b");

        let _log = UNFAITHFUL_LOG
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let events = Events::default();
        let refused =
            tracing::subscriber::with_default(events.clone(), || checked_format_query(&query));
        assert_eq!(refused, Err(FormatError::Unfaithful));

        let events = events.0.lock().expect("events lock");
        assert_eq!(events.len(), 1, "{events:?}");
        let (level, fields) = &events[0];
        assert_eq!(*level, tracing::Level::ERROR);
        assert!(
            fields
                .iter()
                .any(|(name, value)| name == "query_hash" && *value == text_hash("secret=b")),
            "{fields:?}"
        );
        assert!(
            fields.iter().all(|(_, value)| !value.contains("secret")),
            "the log must not carry the query text: {fields:?}"
        );
    }

    /// Rendering, the reparse and the comparison walk explicit stacks, so
    /// a flat chain far deeper than `MAX_EXPR_DEPTH` formats and compares
    /// on a tokio worker's 2 MiB stack in a debug build.
    #[test]
    fn flat_chains_format_on_a_worker_stack() {
        const TOKIO_WORKER_STACK: usize = 2 * 1024 * 1024;
        // A recursive renderer overflows this stack at about 5 000 terms
        // in a debug build. Dropping a reparsed tree still recurses
        // (#330), and that fits this stack up to about 16 000.
        const TERMS: usize = 12_000;

        let sum = vec!["1"; TERMS].join(" + ");
        let faithful = vec![
            format!("* | where {sum} > 0"),
            format!("* | where {}1 == 1", "-".repeat(TERMS)),
            format!("* | where {}a", "not ".repeat(TERMS)),
            format!("* | stats sum({sum})"),
        ];
        // 30 000 terms, each two bytes as written and four once spaced
        let too_long = format!("* | let x = {}", vec!["1"; 30_000].join("+"));

        // The thread reports what it saw and asserts nothing: a panic
        // there would drop these trees on the small stack as it unwinds.
        let (trees, outcomes, refusal) = std::thread::Builder::new()
            .stack_size(TOKIO_WORKER_STACK)
            .spawn(move || {
                let mut trees = Vec::new();
                let mut outcomes = Vec::new();
                for dsl in &faithful {
                    let query = crate::parser::parse(dsl).expect("parses");
                    let reparsed = checked_format_query(&query)
                        .ok()
                        .and_then(|text| crate::parser::parse(&text).ok());
                    let same = reparsed.as_ref().is_some_and(|r| same_query(&query, r));
                    outcomes.push((dsl.chars().take(40).collect::<String>(), same));
                    trees.push(query);
                    trees.extend(reparsed);
                }
                let query = crate::parser::parse(&too_long).expect("parses");
                let refusal = checked_format_query(&query);
                trees.push(query);
                (trees, outcomes, refusal)
            })
            .expect("spawn")
            .join()
            .expect("formatting must not overflow a 2 MiB stack");
        // Dropping a tree this deep recurses in its drop glue, which is
        // not the walk under test, so it gets a stack it fits in.
        std::thread::Builder::new()
            .stack_size(64 * TOKIO_WORKER_STACK)
            .spawn(move || drop(trees))
            .expect("spawn")
            .join()
            .expect("drop");
        for (dsl, same) in outcomes {
            assert!(same, "{dsl}… must format and read back the same");
        }
        assert_eq!(refusal, Err(FormatError::TooLong { bytes: 120_008 }));
    }

    /// Quoted output escapes the three control characters
    /// `primitives::quoted_string` decodes, so formatted text never
    /// carries a raw line break or tab inside a quoted string.
    #[test]
    fn quoted_output_escapes_line_breaks_and_tabs() {
        for (dsl, want) in [
            (r#""a\nb\tc\rd""#, r#""a\nb\tc\rd""#),
            (r#"host="a\nb""#, r#"host="a\nb""#),
            (r#"earliest="a\tb""#, r#"earliest="a\tb""#),
            (r#"* | where m == "a\r\nb""#, "*\n| where m == \"a\\r\\nb\""),
            (r#"| from saved "a\nb""#, r#"| from saved "a\nb""#),
        ] {
            assert_eq!(fmt(dsl), want, "{dsl}");
        }
    }

    /// A hand-built AST can carry a name the grammar would never produce.
    /// The formatter sits on a response path, so it renders what it was
    /// given instead of panicking there.
    #[test]
    fn an_inexpressible_name_does_not_panic_the_formatter() {
        let mut query = crate::parser::parse("| table host").expect("parses");
        match &mut query.pipeline[0].node {
            PipeStage::Table(t) => t.fields[0] = "a\u{202e}b".to_string(),
            other => panic!("expected Table, got {other:?}"),
        }
        assert_eq!(format_query(&query), "| table a\u{202e}b");
    }

    /// …and the rule stays tight: a value whose bare spelling parses back
    /// unchanged is not quoted just for carrying a special character.
    #[test]
    fn values_that_round_trip_bare_are_not_quoted() {
        for (dsl, want) in [
            ("host=/foo/bar/", "host=/foo/bar/"),
            ("host=a/b", "host=a/b"),
            ("path=/api/*", "path=/api/*"),
            ("host=a-b.c", "host=a-b.c"),
            ("host=200", "host=200"),
            // `//` is ordinary data (ADR-0014 ruling 3), so it stays bare
            ("host=a//b", "host=a//b"),
            ("url=https://example.com/x", "url=https://example.com/x"),
            ("path=/api//v1", "path=/api//v1"),
            ("url=//cdn.example.com/x", "url=//cdn.example.com/x"),
        ] {
            assert_eq!(fmt(dsl), want);
        }
    }

    /// A quoted search phrase prints escaped as `quoted_string` reads it,
    /// so a phrase holding quotes or backslashes parses back as the same
    /// one phrase instead of splitting into other search terms or groups.
    #[test]
    fn quoted_phrases_with_quotes_and_backslashes_round_trip() {
        for dsl in [
            r#""alpha\" OR service=other OR \"omega" service=api"#,
            r#""say \"hi\"""#,
            r#""c:\\tmp\\file""#,
            r#""trail\\""#,
            r#""\\\"" host=a"#,
            r#"NOT "a\"b" OR "x\\""#,
        ] {
            let original = crate::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
            let printed = format_query(&original);
            let reparsed = crate::parser::parse(&printed)
                .unwrap_or_else(|e| panic!("{dsl} printed as {printed:?} must parse: {e:?}"));
            let shape = |query: &Query| {
                query
                    .search
                    .groups
                    .iter()
                    .map(|group| {
                        group
                            .iter()
                            .map(|token| match &token.node {
                                SearchToken::Not(inner) => format!("NOT {:?}", inner.node),
                                other => format!("{other:?}"),
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                shape(&reparsed),
                shape(&original),
                "{dsl} printed as {printed:?} must parse back to the same search"
            );
            assert_eq!(format_query(&reparsed), printed, "{dsl}: not idempotent");
        }
    }

    /// The absolute bounds are quoted strings too, so they print escaped
    /// the same way and keep the text they were written with.
    #[test]
    fn quoted_time_bounds_round_trip() {
        let original =
            crate::parser::parse(r#"error earliest="a\" OR x" latest="b\\""#).expect("parses");
        let printed = format_query(&original);
        let reparsed = crate::parser::parse(&printed)
            .unwrap_or_else(|e| panic!("printed as {printed:?} must parse: {e:?}"));
        let bounds = |query: &Query| {
            (
                query.search.groups.len(),
                query.search.earliest.as_ref().map(|b| b.node.clone()),
                query.search.latest.as_ref().map(|b| b.node.clone()),
            )
        };
        assert_eq!(
            bounds(&reparsed),
            bounds(&original),
            "printed as {printed:?}"
        );
    }

    #[test]
    fn backslashes_survive_filter_and_expression_round_trips() {
        for dsl in [
            r#"host="a\\nb""#,
            r#"host="trail\\""#,
            r#"* | where message == "c:\\tmp\\file""#,
            r#"* | let x = "a\\\"b""#,
        ] {
            let once = fmt(dsl);
            let twice = fmt(&once);
            assert_eq!(once, twice, "{dsl}: backslash meaning drifted");
        }
    }

    #[test]
    fn field_filter_with_list() {
        insta::assert_snapshot!(fmt("status=200,301,404"), @"status=200,301,404");
    }

    #[test]
    fn field_filter_with_glob() {
        insta::assert_snapshot!(fmt("path=/api/*"), @"path=/api/*");
    }

    #[test]
    fn field_filter_with_regex() {
        insta::assert_snapshot!(fmt("message=/error.*/"), @"message=/error.*/");
    }

    /// A regex closes its group tight (ADR-0060).
    #[test]
    fn a_regex_closes_its_group_tight() {
        insta::assert_snapshot!(fmt("(a OR msg=/x/ )"), @"(a OR msg=/x/)");
    }

    #[test]
    fn quoted_value_with_space() {
        insta::assert_snapshot!(fmt(r#"service="Activity Monitor""#), @r#"service="Activity Monitor""#);
    }

    #[test]
    fn text_search() {
        insta::assert_snapshot!(fmt("error"), @"error");
    }

    #[test]
    fn negated_text_search() {
        insta::assert_snapshot!(fmt("-debug"), @"-debug");
    }

    #[test]
    fn quoted_phrase() {
        insta::assert_snapshot!(fmt(r#""connection refused""#), @r#""connection refused""#);
    }

    #[test]
    fn time_filter_first() {
        insta::assert_snapshot!(fmt("service=nginx last=2h _severity=error"), @"last=2h service=nginx _severity=error");
    }

    #[test]
    fn time_filter_only() {
        insta::assert_snapshot!(fmt("last=7d"), @"last=7d");
    }

    #[test]
    fn or_groups_inline() {
        insta::assert_snapshot!(fmt("service=nginx OR service=apache"), @"service=nginx OR service=apache");
    }

    #[test]
    fn or_groups_with_time_filter() {
        insta::assert_snapshot!(fmt("service=nginx last=2h OR service=apache"), @"last=2h service=nginx OR service=apache");
    }

    #[test]
    fn earliest_latest() {
        insta::assert_snapshot!(
            fmt(r#"service=nginx latest="2026-01-02" earliest="2026-01-01""#),
            @r#"earliest="2026-01-01" latest="2026-01-02" service=nginx"#
        );
    }

    #[test]
    fn not_token() {
        insta::assert_snapshot!(fmt("NOT service=nginx"), @"NOT service=nginx");
    }

    // ── pipe stages ───────────────────────────────────────────────

    #[test]
    fn full_pipeline() {
        insta::assert_snapshot!(
            fmt("service=nginx   _severity=error   last=2h  |stats count() by host|where count>10|sort -count|head 20"),
            @r"
        last=2h service=nginx _severity=error
        | stats count() by host
        | where count > 10
        | sort -count
        | head 20
        "
        );
    }

    #[test]
    fn stats_with_alias() {
        insta::assert_snapshot!(
            fmt("* | stats count() as total, avg(duration) as avg_dur by service"),
            @r"
        *
        | stats count() as total, avg(duration) as avg_dur by service
        "
        );
    }

    #[test]
    fn eventstats() {
        insta::assert_snapshot!(
            fmt("* | eventstats avg(duration) by service"),
            @r"
        *
        | eventstats avg(duration) by service
        "
        );
    }

    #[test]
    fn where_complex_expr() {
        insta::assert_snapshot!(
            fmt("* | where status >= 400 and count > 10 or host matches /prod-.*/"),
            @r"
        *
        | where status >= 400 and count > 10 or host matches /prod-.*/
        "
        );
    }

    #[test]
    fn sort_multi_field() {
        insta::assert_snapshot!(
            fmt("* | sort status, -count"),
            @r"
        *
        | sort status, -count
        "
        );
    }

    #[test]
    fn limit_and_head_preserve_keyword() {
        insta::assert_snapshot!(fmt("* | limit 20"), @r"
        *
        | limit 20
        ");
        insta::assert_snapshot!(fmt("* | head 20"), @r"
        *
        | head 20
        ");
    }

    #[test]
    fn tail_stage() {
        insta::assert_snapshot!(fmt("* | tail 5"), @r"
        *
        | tail 5
        ");
    }

    #[test]
    fn table_and_fields_preserve_keyword() {
        insta::assert_snapshot!(fmt("* | table host, service"), @r"
        *
        | table host, service
        ");
        insta::assert_snapshot!(fmt("* | fields host, service"), @r"
        *
        | fields host, service
        ");
    }

    #[test]
    fn top_with_by() {
        insta::assert_snapshot!(fmt("* | top 10 host by service"), @r"
        *
        | top 10 host by service
        ");
    }

    #[test]
    fn rare_basic() {
        insta::assert_snapshot!(fmt("* | rare 5 status"), @r"
        *
        | rare 5 status
        ");
    }

    #[test]
    fn drop_fields() {
        insta::assert_snapshot!(fmt("* | drop message, raw"), @r"
        *
        | drop message, raw
        ");
    }

    #[test]
    fn let_and_eval_preserve_keyword() {
        insta::assert_snapshot!(fmt("* | let duration_ms = duration * 1000"), @r"
        *
        | let duration_ms = duration * 1000
        ");
        insta::assert_snapshot!(fmt("* | eval status_class = status / 100"), @r"
        *
        | eval status_class = status / 100
        ");
    }

    #[test]
    fn extract_regex() {
        insta::assert_snapshot!(
            fmt(r#"* | extract "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message"#),
            @r#"
        *
        | extract "(?P<ip>\d+\.\d+\.\d+\.\d+)" from message
        "#
        );
    }

    #[test]
    fn rex_preserves_keyword() {
        insta::assert_snapshot!(
            fmt(r#"* | rex "(?P<code>[A-Z]+)" from raw"#),
            @r#"
        *
        | rex "(?P<code>[A-Z]+)" from raw
        "#
        );
    }

    #[test]
    fn extract_kv() {
        insta::assert_snapshot!(fmt("* | extract kv"), @r"
        *
        | extract kv
        ");
    }

    #[test]
    fn extract_kv_with_sep() {
        insta::assert_snapshot!(fmt(r#"* | extract kv sep=":""#), @r#"
        *
        | extract kv sep=":"
        "#);
    }

    #[test]
    fn extract_kv_from_field() {
        insta::assert_snapshot!(fmt("* | extract kv from raw"), @r"
        *
        | extract kv from raw
        ");
    }

    #[test]
    fn dedup_bare() {
        insta::assert_snapshot!(fmt("* | dedup"), @r"
        *
        | dedup
        ");
    }

    #[test]
    fn dedup_with_fields() {
        insta::assert_snapshot!(fmt("* | dedup host, service"), @r"
        *
        | dedup host, service
        ");
    }

    #[test]
    fn timechart() {
        insta::assert_snapshot!(
            fmt("* | timechart span=5m count() by service"),
            @r"
        *
        | timechart span=5m count() by service
        "
        );
    }

    #[test]
    fn pivot() {
        insta::assert_snapshot!(
            fmt("* | pivot count() on status by host"),
            @r"
        *
        | pivot count() on status by host
        "
        );
    }

    #[test]
    fn rename() {
        insta::assert_snapshot!(
            fmt("* | rename service as svc, host as hostname"),
            @r"
        *
        | rename service as svc, host as hostname
        "
        );
    }

    #[test]
    fn sample_percent() {
        insta::assert_snapshot!(fmt("* | sample 10%"), @r"
        *
        | sample 10%
        ");
    }

    #[test]
    fn sample_count() {
        insta::assert_snapshot!(fmt("* | sample 1000"), @r"
        *
        | sample 1000
        ");
    }

    #[test]
    fn from_saved() {
        insta::assert_snapshot!(
            fmt("| from saved daily_report"),
            @"| from saved daily_report"
        );
    }

    #[test]
    fn from_saved_with_run() {
        insta::assert_snapshot!(
            fmt("| from saved daily_report run=all"),
            @"| from saved daily_report run=all"
        );
    }

    // ── expressions ───────────────────────────────────────────────

    #[test]
    fn expr_minimal_parens() {
        // a + b * c needs no parens: mul binds tighter
        insta::assert_snapshot!(
            fmt("* | let x = a + b * c"),
            @r"
        *
        | let x = a + b * c
        "
        );
    }

    #[test]
    fn expr_needed_parens() {
        // (a + b) * c needs parens
        insta::assert_snapshot!(
            fmt("* | let x = (a + b) * c"),
            @r"
        *
        | let x = (a + b) * c
        "
        );
    }

    #[test]
    fn expr_in_list() {
        insta::assert_snapshot!(
            fmt("* | where x in (1, 2, 3)"),
            @r"
        *
        | where x in (1, 2, 3)
        "
        );
    }

    #[test]
    fn expr_function_call() {
        insta::assert_snapshot!(
            fmt("* | let msg_len = if(isnotnull(message), length(message), 0)"),
            @r"
        *
        | let msg_len = if(isnotnull(message), length(message), 0)
        "
        );
    }

    #[test]
    fn expr_unary_not() {
        insta::assert_snapshot!(
            fmt("* | where not status == 200"),
            @r"
        *
        | where not status == 200
        "
        );
    }

    #[test]
    fn expr_unary_neg() {
        insta::assert_snapshot!(
            fmt("* | let x = -count"),
            @r"
        *
        | let x = -count
        "
        );
    }

    // ── round-trip / idempotency ──────────────────────────────────

    #[test]
    fn idempotent() {
        let input = "service=nginx   _severity=error last=2h  |stats count() by host|where count>10|sort -count|head 20";
        let first = fmt(input);
        let second = fmt(&first);
        assert_eq!(first, second, "formatter is not idempotent");
    }

    #[test]
    fn checked_format_normalizes_whitespace() {
        let query = crate::parser::parse("service=nginx  |  head 10").unwrap();
        assert_eq!(
            checked_format_query(&query).as_deref(),
            Ok("service=nginx\n| head 10")
        );
    }

    // ── empty / edge cases ────────────────────────────────────────

    #[test]
    fn pipe_only_no_search() {
        insta::assert_snapshot!(fmt("| head 10"), @"| head 10");
    }

    #[test]
    fn multiple_let_assignments() {
        insta::assert_snapshot!(
            fmt("* | let a = lower(service), b = length(service)"),
            @r"
        *
        | let a = lower(service), b = length(service)
        "
        );
    }

    // ── backtick-requiring names (ADR-0013 ruling 7) ──────────────

    /// Every field name prints through the one DSL renderer, so a
    /// formatted query parses back to the query it came from — including
    /// names only backticks can spell.
    #[test]
    fn hostile_names_round_trip_through_the_formatter() {
        for dsl in [
            "`x-request-id`=500",
            "`request id`>=400",
            "* | table `x-request-id`, host",
            "* | fields `a#b`",
            "* | drop `request id`",
            "* | dedup `x-request-id`, host",
            "* | sort -`x-request-id`",
            "* | stats count() as `total count` by `x-request-id`",
            "* | stats avg(`response time`)",
            "* | timechart span=1h count() as n by `x-request-id`",
            "* | eventstats count() as `n rows` by `x-request-id`",
            "* | pivot count() as n on `http-status` by `request id`",
            "* | rename `request id` as `req id`",
            "* | let `req id` = 1",
            "* | where `x-request-id` > 400",
            "* | top 5 `http-status` by `request id`",
            "* | rare 5 `http-status`",
            r#"* | extract "(?P<ip>.)" from `raw body`"#,
        ] {
            let original = crate::parser::parse(dsl).unwrap_or_else(|e| panic!("{dsl}: {e:?}"));
            let printed = format_query(&original);
            let reparsed = crate::parser::parse(&printed)
                .unwrap_or_else(|e| panic!("{dsl} printed as {printed:?} must parse: {e:?}"));
            assert_eq!(
                format_query(&reparsed),
                printed,
                "{dsl} printed as {printed:?} must round-trip"
            );
            assert!(printed.contains('`'), "{dsl} printed as {printed:?}");
        }
    }

    /// The three grammar keywords are fields only backticks can reach,
    /// so the formatter always quotes them — a printed query must not
    /// turn a field filter into a time filter.
    #[test]
    fn the_grammar_keywords_print_quoted() {
        insta::assert_snapshot!(fmt("`last`=5 last=2h"), @"last=2h `last`=5");
        insta::assert_snapshot!(
            fmt("* | table `last`, `earliest`, `latest`"),
            @r"
        *
        | table `last`, `earliest`, `latest`
        "
        );
    }
}

#[cfg(test)]
mod saved_name_tests {
    #[test]
    fn names_round_trip() {
        for name in [
            "daily_report",
            "daily-report",
            "a  b",
            "雪/報告",
            "a\\b",
            "a\"b",
            "x | head 1 #",
            "123",
            "a\\",
        ] {
            let dsl = format!("| from saved {} run=42", super::format_saved_name(name));
            let parsed = crate::parser::parse(&dsl).unwrap();
            assert_eq!(parsed.from_saved_stage().unwrap().name, name, "{dsl}");
            let formatted = super::format_query(&parsed);
            assert_eq!(
                crate::parser::parse(&formatted)
                    .unwrap()
                    .from_saved_stage()
                    .unwrap()
                    .name,
                name
            );
        }
    }
}
