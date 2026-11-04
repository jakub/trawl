// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! How many RE2 programs the regexes the emitter writes itself can make
//! `DuckDB` build for one statement (ADR-0063).
//!
//! A query's own regexes bind as parameters, and the emit-time counter
//! counts them and their copies ([`super::MAX_REGEX_PARAMS`]). The
//! emitter also writes regex calls whose patterns are SQL literals:
//! `sev()`'s severity reading and the hot branch's conform of a
//! `SEVERITY` pin (`crate::conform::FIXED_REGEXES`). Each call compiles
//! an RE2 program at bind and again on every thread that runs it, so the
//! batch lane's RE2 allowance has to count them too.
//!
//! # Why the finished SQL, and why a weight instead of a tally
//!
//! The SQL text holds each such call once per place the emitter wrote
//! it, but `DuckDB` makes more of them than the text shows:
//!
//! - its parser rewrites a simple `CASE subject WHEN …` into one
//!   comparison per arm, each with its own copy of the subject, so the
//!   reading's ASCII gate is 38 calls, not one;
//! - its binder copies a same-`SELECT` alias's expression into every
//!   place a later item names it, copies a built-in macro's argument
//!   into every place the macro's body names it (`nullif` writes its
//!   first argument twice), writes a `BETWEEN`'s operand into both of
//!   its comparisons, and copies a `PIVOT`'s aggregate once per pivot
//!   value, up to [`crate::complexity::PIVOT_FILTER_THRESHOLD`];
//! - its optimizer substitutes a projection's expression into a filter
//!   or a window that names the column (filter pushdown), and pushes a
//!   filter into both branches of a `UNION`, so `severity matches "x"`
//!   over the hot union names a 199-call conform 24 times through the
//!   token-text `CASE` and builds about 4800 programs.
//!
//! A tally at the call sites would have to repeat every one of those
//! rules for every rendering that might carry a call. Weighing the
//! finished statement sees every call in one place, whichever rendering
//! wrote it, so it is immune to how the text came to repeat it. Each
//! rule above becomes a multiplier on the text, and every multiplier is
//! the most `DuckDB` makes:
//!
//! - a call whose pattern is not a bare `?` weighs 1, and a parameter's
//!   call weighs nothing here, because the emit-time counter counts it;
//! - a simple `CASE` weighs its subject once per `WHEN` arm;
//! - a call of a macro weighs each argument as many times as the
//!   macro's body names it ([`macro_copies`]);
//! - a `BETWEEN` weighs its operand twice;
//! - a `PIVOT`'s `USING` list weighs
//!   [`crate::complexity::PIVOT_FILTER_THRESHOLD`] times;
//! - a common table expression weighs once per place the statement
//!   names it;
//! - every column a relation carries has a weight, the calls one copy of
//!   its expression brings when the optimizer substitutes it, and every
//!   name an expression reads weighs its column's weight, except an item
//!   that only passes an input column on: a later item naming an earlier
//!   alias pays the alias's copy, as the binder makes it;
//! - a filter's own calls weigh once per scan path beneath it, because
//!   pushdown copies a filter into every branch of a `UNION`.
//!
//! Every weight saturates, and an expression `COLUMNS(…)` expands
//! around a weighed part reads as saturated, because the expansion's
//! width is the input's. A saturated count is an allowance no
//! reservation holds, so the query is refused instead of under-counted.
//!
//! The weight over-counts wherever `DuckDB` makes fewer copies than the
//! rule allows: its common-subexpression pass folds identical copies in
//! a projection, and a pivot's copies of a name it reads are copies of
//! the name, not of the expression behind it. An over-count costs
//! allowance; an under-count would leave RE2 memory outside the
//! reservation. `trawl-engine/tests/fixed_regex_sites.rs` holds the
//! weight against the calls in `DuckDB`'s bound and optimized plans.

use std::collections::HashMap;
use std::ops::Range;

use crate::complexity::PIVOT_FILTER_THRESHOLD;

/// The RE2 programs the regex calls `sql` writes with literal patterns
/// can make `DuckDB` build, every copy its parser, binder and optimizer
/// can make included: what [`crate::pattern::memory::RegexSites::fixed`]
/// holds for an emitted statement.
///
/// `sql` is one statement the emitter wrote. Saturates at `u64::MAX`.
#[must_use]
pub fn fixed_regex_calls(sql: &str) -> u64 {
    let tokens = tokenize(sql);
    let pairs = Pairs::of(&tokens);
    let weigher = Weigher {
        tokens: &tokens,
        pairs: &pairs,
    };
    let scope = Scope::default();
    weigher.query(0..tokens.len(), &Env::default(), &scope).0
}

/// Whether `name` is a `DuckDB` function that compiles its pattern into
/// an RE2 program. Every `regexp_` function counts, `regexp_escape`
/// included, which compiles nothing and so only over-counts.
#[must_use]
pub fn is_re2_function(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("regexp_")
        || matches!(
            name.as_str(),
            "string_split_regex" | "str_split_regex" | "~"
        )
}

/// How many times the body of `DuckDB`'s built-in macro `name` names the
/// argument at `position`, counting through any macro it calls: what the
/// binder copies of that argument. One for every function that is no
/// such macro.
///
/// `trawl-engine/tests/fixed_regex_sites.rs` holds this table against
/// the macros the bundled `DuckDB` defines.
#[must_use]
pub fn macro_copies(name: &str, position: usize) -> u64 {
    let copies: &[u64] = match name.to_ascii_lowercase().as_str() {
        "array_pop_back" | "json_group_array" | "json_group_structure" | "pg_get_constraintdef" => {
            &[2]
        }
        "array_to_string" | "array_to_string_comma_default" | "nullif" => &[2, 1],
        "fmod" | "json_group_object" | "weighted_avg" | "wavg" => &[2, 2],
        "format_pg_type" => &[15, 1],
        "format_type" => &[2, 3],
        "map_contains_entry" => &[1, 2, 2],
        "map_to_pg_oid" => &[19],
        "round_even" | "roundbankers" => &[3, 3],
        "split_part" => &[2, 2, 2],
        _ => &[],
    };
    copies.get(position).copied().unwrap_or(1)
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// A bare word, keyword or identifier, folded to lower case.
    Word(String),
    /// A quoted identifier, unescaped and folded to lower case.
    Name(String),
    /// A string literal of any spelling.
    Str,
    /// A parameter placeholder.
    Param,
    Open(char),
    Close,
    Comma,
    /// `->`, a lambda.
    Arrow,
    Star,
    Dot,
    Colon,
    /// `~`, `!~` and the like: an RE2 match.
    RegexOp,
    Other,
}

/// Fold a name the way a lookup compares it: Unicode lower case, which
/// matches every pair `DuckDB`'s ASCII fold matches and more, so a name
/// is never missed.
fn fold(name: &str) -> String {
    name.to_lowercase()
}

#[allow(clippy::too_many_lines)]
fn tokenize(sql: &str) -> Vec<Tok> {
    let chars: Vec<char> = sql.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    let len = chars.len();
    while i < len {
        let ch = chars[i];
        if ch.is_whitespace() {
            i += 1;
            continue;
        }
        // Comments.
        if ch == '-' && chars.get(i + 1) == Some(&'-') {
            while i < len && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if ch == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < len && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
            continue;
        }
        match ch {
            '\'' => {
                i = skip_string(&chars, i + 1, false);
                tokens.push(Tok::Str);
            }
            '"' => {
                let mut name = String::new();
                i += 1;
                while i < len {
                    if chars[i] == '"' {
                        if chars.get(i + 1) == Some(&'"') {
                            name.push('"');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    name.push(chars[i]);
                    i += 1;
                }
                tokens.push(Tok::Name(fold(&name)));
            }
            '$' => {
                // `$tag$ … $tag$`, `$$ … $$`, or a `$1` parameter.
                let mut tag_end = i + 1;
                while tag_end < len && (chars[tag_end].is_alphanumeric() || chars[tag_end] == '_') {
                    tag_end += 1;
                }
                if tag_end < len && chars[tag_end] == '$' {
                    let tag: Vec<char> = chars[i..=tag_end].to_vec();
                    let mut scan = tag_end + 1;
                    while scan < len && !chars[scan..].starts_with(&tag) {
                        scan += 1;
                    }
                    i = (scan + tag.len()).min(len);
                    tokens.push(Tok::Str);
                } else if tag_end > i + 1 && chars[i + 1..tag_end].iter().all(char::is_ascii_digit)
                {
                    i = tag_end;
                    tokens.push(Tok::Param);
                } else {
                    i += 1;
                    tokens.push(Tok::Other);
                }
            }
            '?' => {
                i += 1;
                while i < len && chars[i].is_ascii_digit() {
                    i += 1;
                }
                tokens.push(Tok::Param);
            }
            '(' | '[' | '{' => {
                tokens.push(Tok::Open(ch));
                i += 1;
            }
            ')' | ']' | '}' => {
                tokens.push(Tok::Close);
                i += 1;
            }
            ',' => {
                tokens.push(Tok::Comma);
                i += 1;
            }
            '*' => {
                tokens.push(Tok::Star);
                i += 1;
            }
            '.' if !chars.get(i + 1).is_some_and(char::is_ascii_digit) => {
                tokens.push(Tok::Dot);
                i += 1;
            }
            ':' if chars.get(i + 1) != Some(&':') => {
                tokens.push(Tok::Colon);
                i += 1;
            }
            '-' if chars.get(i + 1) == Some(&'>') && chars.get(i + 2) != Some(&'>') => {
                tokens.push(Tok::Arrow);
                i += 2;
            }
            '~' | '!' => {
                let start = i;
                while i < len && matches!(chars[i], '~' | '!' | '*') {
                    i += 1;
                }
                let run: String = chars[start..i].iter().collect();
                let like = matches!(run.as_str(), "~~" | "~~*" | "!~~" | "!~~*" | "~~~");
                tokens.push(if run.contains('~') && !like {
                    Tok::RegexOp
                } else {
                    Tok::Other
                });
            }
            _ if ch.is_alphabetic() || ch == '_' => {
                let start = i;
                while i < len && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
                {
                    i += 1;
                }
                let word: String = chars[start..i].iter().collect();
                // `E'…'` takes backslash escapes; every other prefix
                // (`X'…'`, `B'…'`) is an ordinary literal after its letter.
                if chars.get(i) == Some(&'\'') && word.eq_ignore_ascii_case("e") {
                    i = skip_string(&chars, i + 1, true);
                    tokens.push(Tok::Str);
                } else {
                    tokens.push(Tok::Word(fold(&word)));
                }
            }
            _ if ch.is_ascii_digit() || ch == '.' => {
                while i < len && (chars[i].is_ascii_alphanumeric() || chars[i] == '.') {
                    i += 1;
                }
                tokens.push(Tok::Other);
            }
            _ => {
                tokens.push(Tok::Other);
                i += 1;
            }
        }
    }
    tokens
}

/// The index just past the string literal whose body starts at `i`.
fn skip_string(chars: &[char], mut i: usize, backslashes: bool) -> usize {
    while i < chars.len() {
        match chars[i] {
            '\\' if backslashes => i += 2,
            '\'' if chars.get(i + 1) == Some(&'\'') => i += 2,
            '\'' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Where each bracket and each `CASE` closes, so a walk skips a nested
/// group in one step.
struct Pairs {
    close: HashMap<usize, usize>,
    end: HashMap<usize, usize>,
}

impl Pairs {
    fn of(tokens: &[Tok]) -> Self {
        let mut close = HashMap::new();
        let mut end = HashMap::new();
        let mut opens = Vec::new();
        let mut cases = Vec::new();
        for (i, token) in tokens.iter().enumerate() {
            match token {
                Tok::Open(_) => opens.push(i),
                Tok::Close => {
                    if let Some(open) = opens.pop() {
                        close.insert(open, i);
                    }
                }
                Tok::Word(w) if w == "case" => cases.push(i),
                Tok::Word(w) if w == "end" => {
                    if let Some(case) = cases.pop() {
                        end.insert(case, i);
                    }
                }
                _ => {}
            }
        }
        Self { close, end }
    }
}

// ---------------------------------------------------------------------------
// Weights
// ---------------------------------------------------------------------------

/// What one expression weighs: the regex calls it writes, every copy the
/// parser and binder make of them included, and the weight of the
/// columns it names.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Weight {
    calls: u64,
    names: u64,
}

impl Weight {
    const ONE_CALL: Self = Self { calls: 1, names: 0 };

    fn total(self) -> u64 {
        self.calls.saturating_add(self.names)
    }

    fn plus(self, other: Self) -> Self {
        Self {
            calls: self.calls.saturating_add(other.calls),
            names: self.names.saturating_add(other.names),
        }
    }

    fn times(self, n: u64) -> Self {
        Self {
            calls: self.calls.saturating_mul(n),
            names: self.names.saturating_mul(n),
        }
    }

    fn named(weight: u64) -> Self {
        Self {
            calls: 0,
            names: weight,
        }
    }
}

/// A relation's columns, each with what one substituted copy of its
/// expression weighs, and how many scan paths lie beneath it.
#[derive(Debug, Clone)]
struct Rel {
    columns: HashMap<String, u64>,
    paths: u64,
}

impl Rel {
    fn scan() -> Self {
        Self {
            columns: HashMap::new(),
            paths: 1,
        }
    }

    /// Two relations read side by side (a `UNION` or a join): a column's
    /// copies reach every branch that carries it, and a filter reaches
    /// every path.
    fn merged(mut self, other: Self) -> Self {
        for (name, weight) in other.columns {
            let entry = self.columns.entry(name).or_insert(0);
            *entry = entry.saturating_add(weight);
        }
        self.paths = self.paths.saturating_add(other.paths);
        self
    }
}

/// The names an expression can read, innermost scope first.
#[derive(Default)]
struct Env<'a> {
    maps: Vec<&'a HashMap<String, u64>>,
    shadowed: Vec<String>,
}

impl<'a> Env<'a> {
    fn with(&self, map: &'a HashMap<String, u64>) -> Self {
        let mut maps = self.maps.clone();
        maps.push(map);
        Self {
            maps,
            shadowed: self.shadowed.clone(),
        }
    }

    fn shadowing(&self, names: Vec<String>) -> Self {
        let mut shadowed = self.shadowed.clone();
        shadowed.extend(names);
        Self {
            maps: self.maps.clone(),
            shadowed,
        }
    }

    /// The heaviest column `name` can mean here. A lambda's parameter is
    /// no column.
    fn lookup(&self, name: &str) -> u64 {
        if self.shadowed.iter().any(|s| s == name) {
            return 0;
        }
        self.maps
            .iter()
            .filter_map(|map| map.get(name))
            .copied()
            .max()
            .unwrap_or(0)
    }
}

/// The common table expressions in scope, by name.
#[derive(Default, Clone)]
struct Scope {
    ctes: HashMap<String, Rel>,
}

struct Weigher<'t> {
    tokens: &'t [Tok],
    pairs: &'t Pairs,
}

/// The clause keywords that end a `SELECT` list or another clause.
const CLAUSES: &[&str] = &[
    "from", "where", "group", "having", "qualify", "window", "order", "limit", "offset",
];

const QUERY_STARTS: &[&str] = &["select", "with", "pivot", "from", "values", "unpivot"];

impl Weigher<'_> {
    fn word(&self, i: usize) -> Option<&str> {
        match self.tokens.get(i) {
            Some(Tok::Word(w)) => Some(w.as_str()),
            _ => None,
        }
    }

    fn is_word(&self, i: usize, word: &str) -> bool {
        self.word(i) == Some(word)
    }

    /// The name token `i` spells, bare or quoted.
    fn name(&self, i: usize) -> Option<&str> {
        match self.tokens.get(i) {
            Some(Tok::Word(w) | Tok::Name(w)) => Some(w.as_str()),
            _ => None,
        }
    }

    /// The index after the group or `CASE` starting at `i`, or `i + 1`.
    fn skip(&self, i: usize) -> usize {
        match self.tokens.get(i) {
            Some(Tok::Open(_)) => self.pairs.close.get(&i).map_or(i + 1, |c| c + 1),
            Some(Tok::Word(w)) if w == "case" => self.pairs.end.get(&i).map_or(i + 1, |e| e + 1),
            _ => i + 1,
        }
    }

    /// The positions in `range` outside every nested group and `CASE`.
    fn top_level(&self, range: Range<usize>) -> Vec<usize> {
        let mut out = Vec::new();
        let mut i = range.start;
        while i < range.end {
            out.push(i);
            i = self.skip(i).min(range.end).max(i + 1);
        }
        out
    }

    /// `range` split at its top-level commas.
    fn split_commas(&self, range: Range<usize>) -> Vec<Range<usize>> {
        let mut parts = Vec::new();
        let mut start = range.start;
        for i in self.top_level(range.clone()) {
            if self.tokens[i] == Tok::Comma {
                parts.push(start..i);
                start = i + 1;
            }
        }
        if start < range.end || !parts.is_empty() {
            parts.push(start..range.end);
        }
        parts
    }

    /// `range` without the brackets around all of it.
    fn unwrap(&self, mut range: Range<usize>) -> Range<usize> {
        while range.len() >= 2
            && matches!(self.tokens[range.start], Tok::Open('('))
            && self.pairs.close.get(&range.start) == Some(&(range.end - 1))
        {
            range = range.start + 1..range.end - 1;
        }
        range
    }

    fn starts_query(&self, range: &Range<usize>) -> bool {
        let start = self.unwrap(range.clone()).start;
        self.word(start).is_some_and(|w| QUERY_STARTS.contains(&w)) && !range.is_empty()
    }

    // -----------------------------------------------------------------------
    // Expressions
    // -----------------------------------------------------------------------

    /// What `range`, an expression, weighs where `env` names its columns.
    fn expr(&self, range: Range<usize>, env: &Env<'_>, scope: &Scope) -> Weight {
        if range.is_empty() {
            return Weight::default();
        }
        if let Some(lambda) = self.lambda(range.clone()) {
            let (params, body) = lambda;
            return self.expr(body, &env.shadowing(params), scope);
        }
        if self.starts_query(&range) {
            return Weight {
                calls: self.query(self.unwrap(range), env, scope).0,
                names: 0,
            };
        }
        let mut weight = Weight::default();
        // Where the operand a BETWEEN reads would start: just after the
        // last word that ends an operand at this level. It may start
        // earlier than the operand does (`a = b BETWEEN …` reads `b`),
        // which only weighs more.
        let mut operand = range.start;
        let mut between_and = false;
        let mut i = range.start;
        while i < range.end {
            let next = self.skip(i).min(range.end).max(i + 1);
            match &self.tokens[i] {
                Tok::Word(w) if w == "between" => {
                    // The binder writes the operand into both bounds'
                    // comparisons: one copy more than the text holds.
                    let end = if i > range.start && self.is_word(i - 1, "not") {
                        i - 1
                    } else {
                        i
                    };
                    weight = weight.plus(self.expr(operand..end, env, scope));
                    between_and = true;
                }
                Tok::Word(w) if w == "and" && between_and => between_and = false,
                Tok::Word(w)
                    if matches!(w.as_str(), "and" | "or" | "when" | "then" | "else")
                        || w == "not" && !self.is_word(i + 1, "between") =>
                {
                    operand = i + 1;
                }
                Tok::Comma => operand = i + 1,
                Tok::Word(w) if w == "case" => {
                    let end = self.pairs.end.get(&i).copied().unwrap_or(range.end);
                    weight = weight.plus(self.case(i + 1..end.min(range.end), env, scope));
                }
                Tok::Word(_) | Tok::Name(_)
                    if matches!(self.tokens.get(i + 1), Some(Tok::Open('('))) =>
                {
                    let close = self.pairs.close.get(&(i + 1)).copied().unwrap_or(range.end);
                    let name = self.name(i).unwrap_or_default().to_owned();
                    weight = weight.plus(self.call(&name, i + 2..close.min(range.end), env, scope));
                    i = (close + 1).min(range.end).max(i + 1);
                    continue;
                }
                Tok::Word(w) if w == "similar" => weight = weight.plus(Weight::ONE_CALL),
                Tok::RegexOp => weight = weight.plus(Weight::ONE_CALL),
                Tok::Word(w) | Tok::Name(w) => weight = weight.plus(Weight::named(env.lookup(w))),
                Tok::Open(_) => {
                    let close = self.pairs.close.get(&i).copied().unwrap_or(range.end);
                    weight = weight.plus(self.expr(i + 1..close.min(range.end), env, scope));
                }
                _ => {}
            }
            i = next;
        }
        weight
    }

    /// A lambda's parameters and body, when `range` is one:
    /// `x -> body`, `(x, y) -> body` or `lambda x, y: body`.
    fn lambda(&self, range: Range<usize>) -> Option<(Vec<String>, Range<usize>)> {
        let start = range.start;
        if self.is_word(start, "lambda") {
            let colon = (start..range.end).find(|&i| self.tokens[i] == Tok::Colon)?;
            let params = (start + 1..colon)
                .filter_map(|i| self.name(i).map(str::to_owned))
                .collect();
            return Some((params, colon + 1..range.end));
        }
        if self.name(start).is_some() && self.tokens.get(start + 1) == Some(&Tok::Arrow) {
            let param = self.name(start)?.to_owned();
            return Some((vec![param], start + 2..range.end));
        }
        if matches!(self.tokens.get(start), Some(Tok::Open('('))) {
            let close = *self.pairs.close.get(&start)?;
            if self.tokens.get(close + 1) == Some(&Tok::Arrow) {
                let params = (start + 1..close)
                    .filter_map(|i| self.name(i).map(str::to_owned))
                    .collect();
                return Some((params, close + 2..range.end));
            }
        }
        None
    }

    /// A `CASE` from just after its keyword to its `END`.
    fn case(&self, body: Range<usize>, env: &Env<'_>, scope: &Scope) -> Weight {
        let whens: Vec<usize> = self
            .top_level(body.clone())
            .into_iter()
            .filter(|&i| self.is_word(i, "when"))
            .collect();
        let Some(&first) = whens.first() else {
            return self.expr(body, env, scope);
        };
        if first == body.start {
            return self.expr(body, env, scope);
        }
        // A simple CASE: the parser writes the subject into every arm.
        let arms = u64::try_from(whens.len()).unwrap_or(u64::MAX);
        self.expr(body.start..first, env, scope)
            .times(arms)
            .plus(self.expr(first..body.end, env, scope))
    }

    /// A call of `name` with its arguments in `args`.
    fn call(&self, name: &str, args: Range<usize>, env: &Env<'_>, scope: &Scope) -> Weight {
        let parts = self.split_commas(args.clone());
        if is_re2_function(name) {
            let parameter = parts
                .get(1)
                .is_some_and(|p| p.len() == 1 && self.tokens[p.start] == Tok::Param);
            let own = if parameter {
                Weight::default()
            } else {
                Weight::ONE_CALL
            };
            return parts
                .into_iter()
                .fold(own, |w, part| w.plus(self.expr(part, env, scope)));
        }
        if name == "columns"
            && parts.len() == 1
            && args.len() == 1
            && self.tokens[args.start] == Tok::Str
        {
            // `COLUMNS('regex')` selects columns with an RE2 pattern.
            return Weight::ONE_CALL;
        }
        parts
            .into_iter()
            .enumerate()
            .fold(Weight::default(), |w, (position, part)| {
                w.plus(
                    self.expr(part, env, scope)
                        .times(macro_copies(name, position)),
                )
            })
    }

    /// Whether `range` expands a `COLUMNS(…)` around anything that
    /// weighs: the expansion copies the expression once per column.
    fn expands_around_weight(&self, range: Range<usize>, env: &Env<'_>, scope: &Scope) -> bool {
        let columns: Vec<usize> = (range.start..range.end)
            .filter(|&i| {
                self.is_word(i, "columns") && matches!(self.tokens.get(i + 1), Some(Tok::Open('(')))
            })
            .collect();
        if columns.is_empty() {
            return false;
        }
        let inside = columns.iter().fold(0_u64, |sum, &i| {
            let close = self.pairs.close.get(&(i + 1)).copied().unwrap_or(range.end);
            sum.saturating_add(self.call("columns", i + 2..close, env, scope).total())
        });
        self.expr(range, env, scope).total() > inside
    }

    // -----------------------------------------------------------------------
    // Queries
    // -----------------------------------------------------------------------

    /// What the query in `range` weighs, and the relation it makes.
    fn query(&self, range: Range<usize>, env: &Env<'_>, scope: &Scope) -> (u64, Rel) {
        let range = self.unwrap(range);
        if range.is_empty() {
            return (0, Rel::scan());
        }
        if self.is_word(range.start, "with") {
            return self.with(range, env, scope);
        }
        // Set operations: each side weighs, and the relations merge.
        let mut cores = Vec::new();
        let mut start = range.start;
        let tops = self.top_level(range.clone());
        let mut k = 0;
        while k < tops.len() {
            let i = tops[k];
            if matches!(self.word(i), Some("union" | "except" | "intersect")) {
                cores.push(start..i);
                k += 1;
                while k < tops.len()
                    && matches!(self.word(tops[k]), Some("all" | "distinct" | "by" | "name"))
                {
                    k += 1;
                }
                start = tops.get(k).copied().unwrap_or(range.end);
                continue;
            }
            k += 1;
        }
        cores.push(start..range.end);
        let mut total = 0_u64;
        let mut rel: Option<Rel> = None;
        for core in cores {
            let (weight, core_rel) = self.core(core, env, scope);
            total = total.saturating_add(weight);
            rel = Some(match rel {
                Some(rel) => rel.merged(core_rel),
                None => core_rel,
            });
        }
        (total, rel.unwrap_or_else(Rel::scan))
    }

    /// `WITH … AS (…), … <body>`: each body weighs once per place the
    /// rest of the statement names it.
    fn with(&self, range: Range<usize>, env: &Env<'_>, scope: &Scope) -> (u64, Rel) {
        let mut ctes: Vec<(String, Range<usize>)> = Vec::new();
        let mut i = range.start + 1;
        if self.is_word(i, "recursive") {
            i += 1;
        }
        while let Some(name) = self.name(i).map(str::to_owned) {
            let mut j = i + 1;
            while j < range.end && !matches!(self.tokens[j], Tok::Open('(')) {
                j += 1;
            }
            let Some(&close) = self.pairs.close.get(&j) else {
                break;
            };
            ctes.push((name, j + 1..close));
            i = close + 1;
            if self.tokens.get(i) == Some(&Tok::Comma) {
                i += 1;
            } else {
                break;
            }
        }
        let body = i.min(range.end)..range.end;

        // How many times each body binds: once per place a later body or
        // the main query names it, those places counted as often as
        // their own body binds.
        let mut copies = vec![0_u64; ctes.len()];
        for (index, (name, _)) in ctes.iter().enumerate().rev() {
            let mut n = self.count_names(body.clone(), name);
            for (later, (_, later_range)) in ctes.iter().enumerate().skip(index + 1) {
                n = n.saturating_add(
                    self.count_names(later_range.clone(), name)
                        .saturating_mul(copies[later]),
                );
            }
            copies[index] = n.max(1);
        }

        let mut inner = scope.clone();
        let mut total = 0_u64;
        for ((name, cte_range), times) in ctes.into_iter().zip(copies) {
            let (weight, rel) = self.query(cte_range, env, &inner);
            total = total.saturating_add(weight.saturating_mul(times));
            inner.ctes.insert(name, rel);
        }
        let (weight, rel) = self.query(body, env, &inner);
        (total.saturating_add(weight), rel)
    }

    fn count_names(&self, range: Range<usize>, name: &str) -> u64 {
        let n = (range.start..range.end)
            .filter(|&i| self.name(i) == Some(name))
            .count();
        u64::try_from(n).unwrap_or(u64::MAX)
    }

    /// One `SELECT`, `PIVOT` or other query core.
    fn core(&self, range: Range<usize>, env: &Env<'_>, scope: &Scope) -> (u64, Rel) {
        let range = self.unwrap(range);
        match self.word(range.start) {
            Some("select") => self.select(range, env, scope),
            Some("pivot") => self.pivot(range, env, scope),
            Some("with") => self.with(range, env, scope),
            _ => (self.expr(range, env, scope).total(), Rel::scan()),
        }
    }

    /// The first top-level position of each clause keyword in `range`.
    fn clauses(&self, range: Range<usize>) -> Vec<(usize, &'static str)> {
        let mut found: Vec<(usize, &'static str)> = Vec::new();
        for i in self.top_level(range) {
            let Some(word) = self.word(i) else { continue };
            let Some(&clause) = CLAUSES.iter().find(|c| **c == word) else {
                continue;
            };
            // `IS [NOT] DISTINCT FROM` is an operator, not a clause.
            if clause == "from" && i > 0 && self.is_word(i - 1, "distinct") {
                continue;
            }
            if matches!(clause, "group" | "order") && !self.is_word(i + 1, "by") {
                continue;
            }
            if !found.iter().any(|(_, c)| *c == clause) {
                found.push((i, clause));
            }
        }
        found
    }

    fn select(&self, range: Range<usize>, env: &Env<'_>, scope: &Scope) -> (u64, Rel) {
        let clauses = self.clauses(range.start + 1..range.end);
        let clause_end = |at: usize| {
            clauses
                .iter()
                .map(|(i, _)| *i)
                .filter(|&i| i > at)
                .min()
                .unwrap_or(range.end)
        };
        let list_end = clauses.first().map_or(range.end, |(i, _)| *i);
        let mut total = 0_u64;

        let (from_weight, input) = match clauses.iter().find(|(_, c)| *c == "from") {
            Some(&(at, _)) => self.from(at + 1..clause_end(at), env, scope),
            None => (0, Rel::scan()),
        };
        total = total.saturating_add(from_weight);
        let input_env = env.with(&input.columns);

        // The SELECT list: DISTINCT [ON (…)] first, then the items.
        let mut list = range.start + 1..list_end;
        let mut modifiers = Weight::default();
        if matches!(self.word(list.start), Some("all")) {
            list.start += 1;
        } else if self.is_word(list.start, "distinct") {
            list.start += 1;
            if self.is_word(list.start, "on")
                && matches!(self.tokens.get(list.start + 1), Some(Tok::Open('(')))
            {
                let close = self
                    .pairs
                    .close
                    .get(&(list.start + 1))
                    .copied()
                    .unwrap_or(list.end);
                modifiers = self.expr(list.start + 1..close + 1, &input_env, scope);
                list.start = close + 1;
            }
        }

        let mut lateral: HashMap<String, u64> = HashMap::new();
        let mut outputs: HashMap<String, u64> = HashMap::new();
        for item in self.split_commas(list) {
            let (weight, defines) = {
                let item_env = input_env.with(&lateral);
                if self.expands_around_weight(item.clone(), &item_env, scope) {
                    return (u64::MAX, input);
                }
                self.item(item, &item_env, &lateral, input.paths, scope)
            };
            total = total.saturating_add(weight);
            for (name, column) in defines {
                lateral
                    .entry(name.clone())
                    .and_modify(|w| *w = (*w).max(column))
                    .or_insert(column);
                outputs.insert(name, column);
            }
        }
        total = total.saturating_add(modifiers.total());

        let all_env = input_env.with(&lateral);
        for &(at, clause) in &clauses {
            if clause == "from" {
                continue;
            }
            let part = at + 1..clause_end(at);
            if self.expands_around_weight(part.clone(), &all_env, scope) {
                return (u64::MAX, input);
            }
            let weight = self.expr(part, &all_env, scope);
            let weight = if matches!(clause, "where" | "having" | "qualify") {
                // Pushdown copies a filter into every path beneath it;
                // the names it reads already weigh every branch.
                Weight {
                    calls: weight.calls.saturating_mul(input.paths),
                    names: weight.names,
                }
            } else {
                weight
            };
            total = total.saturating_add(weight.total());
        }

        let paths = input.paths;
        let mut columns = input.columns;
        columns.extend(outputs);
        (total, Rel { columns, paths })
    }

    /// One SELECT item: what it weighs and the columns it defines, each
    /// with what one substituted copy of it weighs.
    fn item(
        &self,
        item: Range<usize>,
        env: &Env<'_>,
        lateral: &HashMap<String, u64>,
        paths: u64,
        scope: &Scope,
    ) -> (u64, Vec<(String, u64)>) {
        if item.is_empty() {
            return (0, Vec::new());
        }
        // `*`, `t.*`, with EXCLUDE and REPLACE.
        let star = match self.tokens[item.start] {
            Tok::Star => Some(item.start),
            _ if self.tokens.get(item.start + 1) == Some(&Tok::Dot)
                && self.tokens.get(item.start + 2) == Some(&Tok::Star) =>
            {
                Some(item.start + 2)
            }
            _ => None,
        };
        if let Some(star) = star {
            let mut total = 0_u64;
            let mut defines = Vec::new();
            let mut i = star + 1;
            while i < item.end {
                if self.is_word(i, "replace")
                    && matches!(self.tokens.get(i + 1), Some(Tok::Open('(')))
                {
                    let close = self.pairs.close.get(&(i + 1)).copied().unwrap_or(item.end);
                    for entry in self.split_commas(i + 2..close) {
                        let (weight, entry_defines) =
                            self.aliased(entry, env, lateral, paths, scope);
                        total = total.saturating_add(weight);
                        defines.extend(entry_defines);
                    }
                    i = close + 1;
                    continue;
                }
                i = self.skip(i).max(i + 1);
            }
            return (total, defines);
        }
        self.aliased(item, env, lateral, paths, scope)
    }

    /// `expr [AS] alias`, or a bare expression.
    fn aliased(
        &self,
        item: Range<usize>,
        env: &Env<'_>,
        lateral: &HashMap<String, u64>,
        paths: u64,
        scope: &Scope,
    ) -> (u64, Vec<(String, u64)>) {
        let last = item.end - 1;
        let (expr, alias) =
            if item.len() >= 3 && self.is_word(last - 1, "as") && self.name(last).is_some() {
                (item.start..last - 1, self.name(last).map(str::to_owned))
            } else if item.len() >= 2
                && self.name(last).is_some()
                && matches!(
                    self.tokens.get(last - 1),
                    Some(Tok::Close | Tok::Name(_) | Tok::Str)
                )
                || item.len() >= 2 && self.name(last).is_some() && self.is_word(last - 1, "end")
            {
                (item.start..last, self.name(last).map(str::to_owned))
            } else {
                (item.clone(), None)
            };

        // An item that only names a column passes it on: no copy unless
        // the name is an earlier alias of this list, which the binder
        // copies in.
        let bare = self.unwrap(expr.clone());
        if bare.len() == 1
            && let Some(name) = self.name(bare.start)
        {
            let copied = lateral.get(name).copied().unwrap_or(0);
            let column = env.lookup(name);
            let output = alias.unwrap_or_else(|| name.to_owned());
            return (copied, vec![(output, column)]);
        }

        let weight = self.expr(expr, env, scope);
        let column = weight
            .calls
            .saturating_mul(paths)
            .saturating_add(weight.names);
        (
            weight.total(),
            alias.into_iter().map(|a| (a, column)).collect(),
        )
    }

    /// A FROM clause: its items side by side, and their join conditions.
    fn from(&self, range: Range<usize>, env: &Env<'_>, scope: &Scope) -> (u64, Rel) {
        let mut items = Vec::new();
        let mut conditions = Vec::new();
        let mut start = range.start;
        let mut condition: Option<usize> = None;
        for i in self.top_level(range.clone()) {
            let boundary = self.tokens[i] == Tok::Comma || self.is_word(i, "join");
            if boundary {
                match condition.take() {
                    Some(at) => {
                        conditions.push(at..i);
                    }
                    None => items.push(start..i),
                }
                start = i + 1;
            } else if condition.is_none() && matches!(self.word(i), Some("on" | "using")) {
                items.push(start..i);
                condition = Some(i + 1);
            }
        }
        match condition {
            Some(at) => conditions.push(at..range.end),
            None => items.push(start..range.end),
        }

        let mut total = 0_u64;
        let mut rel: Option<Rel> = None;
        for item in items {
            let (weight, item_rel) = self.source_item(item, env, scope);
            total = total.saturating_add(weight);
            rel = Some(match rel {
                Some(rel) => rel.merged(item_rel),
                None => item_rel,
            });
        }
        let rel = rel.unwrap_or_else(Rel::scan);
        let condition_env = env.with(&rel.columns);
        for condition in conditions {
            let weight = self.expr(condition, &condition_env, scope);
            total = total
                .saturating_add(weight.calls.saturating_mul(rel.paths))
                .saturating_add(weight.names);
        }
        (total, rel)
    }

    /// One relation a FROM clause reads, any alias after it ignored.
    fn source_item(&self, range: Range<usize>, env: &Env<'_>, scope: &Scope) -> (u64, Rel) {
        // Join qualifiers before the relation.
        let mut start = range.start;
        while matches!(
            self.word(start),
            Some(
                "left"
                    | "right"
                    | "full"
                    | "inner"
                    | "outer"
                    | "cross"
                    | "natural"
                    | "lateral"
                    | "anti"
                    | "semi"
                    | "positional"
                    | "asof"
            )
        ) {
            start += 1;
        }
        if start >= range.end {
            return (0, Rel::scan());
        }
        match &self.tokens[start] {
            Tok::Open('(') => {
                let close = self.pairs.close.get(&start).copied().unwrap_or(range.end);
                self.query(start + 1..close, env, scope)
            }
            Tok::Word(_) | Tok::Name(_)
                if matches!(self.tokens.get(start + 1), Some(Tok::Open('('))) =>
            {
                // A table function: its arguments weigh, its columns are
                // read from storage.
                let close = self
                    .pairs
                    .close
                    .get(&(start + 1))
                    .copied()
                    .unwrap_or(range.end);
                let name = self.name(start).unwrap_or_default().to_owned();
                let weight = self.call(&name, start + 2..close, env, scope).total();
                (weight, Rel::scan())
            }
            Tok::Word(w) if w == "pivot" => self.pivot(start..range.end, env, scope),
            Tok::Word(_) | Tok::Name(_) => {
                let name = self.name(start).unwrap_or_default();
                (0, scope.ctes.get(name).cloned().unwrap_or_else(Rel::scan))
            }
            _ => (self.expr(start..range.end, env, scope).total(), Rel::scan()),
        }
    }

    /// `PIVOT rel ON … USING … [GROUP BY …]`: the `USING` list binds once
    /// per pivot value, up to the threshold.
    fn pivot(&self, range: Range<usize>, env: &Env<'_>, scope: &Scope) -> (u64, Rel) {
        let tops = self.top_level(range.start + 1..range.end);
        let at = |word: &str| tops.iter().copied().find(|&i| self.is_word(i, word));
        let on = at("on").unwrap_or(range.end);
        let using = at("using").unwrap_or(range.end);
        let rest = [at("group"), at("order"), at("limit")]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(range.end);
        let (mut total, input) = self.from(range.start + 1..on.min(using), env, scope);
        let input_env = env.with(&input.columns);
        if on < range.end {
            total = total.saturating_add(
                self.expr(on + 1..using.min(rest), &input_env, scope)
                    .total(),
            );
        }
        if using < range.end {
            total = total.saturating_add(
                self.expr(using + 1..rest, &input_env, scope)
                    .times(PIVOT_FILTER_THRESHOLD)
                    .total(),
            );
        }
        total = total.saturating_add(self.expr(rest..range.end, &input_env, scope).total());
        (total, input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::severity::Dialect;

    fn calls(sql: &str) -> u64 {
        fixed_regex_calls(sql)
    }

    /// A call whose pattern is a literal weighs one; a parameter's call
    /// weighs nothing here, the emit-time counter having counted it.
    #[test]
    fn a_literal_pattern_counts_and_a_parameter_does_not() {
        assert_eq!(calls("SELECT regexp_full_match(a, 'x') FROM t"), 1);
        assert_eq!(calls("SELECT REGEXP_REPLACE(a, 'x', '') FROM t"), 1);
        assert_eq!(calls("SELECT regexp_matches(a, ?) FROM t"), 0);
        assert_eq!(calls("SELECT regexp_extract(a, ?, ?::VARCHAR[]) FROM t"), 0);
        assert_eq!(calls("SELECT string_split_regex(a, ',') FROM t"), 1);
        assert_eq!(
            calls("SELECT a ~ 'x', a !~ 'y', a SIMILAR TO 'z' FROM t"),
            3
        );
        assert_eq!(calls("SELECT a ~~ 'x', a !~~ 'y', a ~~~ 'z' FROM t"), 0);
        assert_eq!(calls("SELECT * FROM t WHERE \"a\" != 'b'"), 0);
    }

    /// Text that only spells a call is no call.
    #[test]
    fn strings_names_and_comments_are_no_calls() {
        assert_eq!(calls("SELECT 'regexp_replace(a, ''b'', '''')' FROM t"), 0);
        assert_eq!(
            calls("SELECT E'it\\'s regexp_replace(a, ''b'', '''')' FROM t"),
            0
        );
        assert_eq!(calls("SELECT \"regexp_replace\" FROM t"), 0);
        assert_eq!(calls("SELECT $$regexp_replace(a, 'b', '')$$ FROM t"), 0);
        assert_eq!(
            calls("-- regexp_replace(a, 'b', '')\nSELECT 1 /* regexp_matches(a, 'b') */"),
            0
        );
    }

    /// The parser writes a simple CASE's subject into every arm; a
    /// searched CASE has no subject.
    #[test]
    fn a_simple_case_weighs_its_subject_once_an_arm() {
        assert_eq!(
            calls(
                "SELECT CASE regexp_replace(a, 'x', '') WHEN 'a' THEN 1 WHEN 'b' THEN 2 END FROM t"
            ),
            2
        );
        assert_eq!(
            calls(
                "SELECT CASE WHEN regexp_full_match(a, 'x') THEN 1 WHEN regexp_full_match(a, 'y') THEN 2 END FROM t"
            ),
            2
        );
        // Nested: the inner simple CASE's subject in the outer's subject.
        assert_eq!(
            calls(
                "SELECT CASE (CASE regexp_replace(a, 'x', '') WHEN 'a' THEN 'b' WHEN 'c' THEN 'd' END) \
                 WHEN 'b' THEN 1 WHEN 'd' THEN 2 WHEN 'e' THEN 3 END FROM t"
            ),
            6
        );
    }

    /// A macro copies its argument once per place its body names it.
    #[test]
    fn a_macro_weighs_each_argument_as_often_as_its_body_names_it() {
        assert_eq!(
            calls("SELECT nullif(regexp_replace(a, 'x', ''), '') FROM t"),
            2
        );
        assert_eq!(
            calls("SELECT nullif('', regexp_replace(a, 'x', '')) FROM t"),
            1
        );
        assert_eq!(
            calls("SELECT split_part(a, regexp_replace(a, 'x', ''), 1) FROM t"),
            2
        );
        assert_eq!(macro_copies("NULLIF", 0), 2);
        assert_eq!(macro_copies("lower", 0), 1);
    }

    /// A later item naming an earlier alias pays the alias's copy; an
    /// item that only passes an input column on pays nothing.
    #[test]
    fn a_lateral_alias_is_copied_into_every_item_naming_it() {
        assert_eq!(
            calls(
                "SELECT regexp_replace(a, 'x', '') AS \"b\", (\"b\") AS \"c\", \"B\" || \"c\" AS \"d\", \
                 \"e\" FROM t"
            ),
            4
        );
    }

    /// A projection's expression is substituted into every filter and
    /// window that names its column, through every layer.
    #[test]
    fn a_named_column_weighs_its_expression_where_it_can_be_substituted() {
        let cte = "WITH s AS (SELECT regexp_replace(a, 'x', '') AS \"b\" FROM t)";
        assert_eq!(calls(&format!("{cte} SELECT * FROM s")), 1);
        assert_eq!(
            calls(&format!(
                "{cte} SELECT * FROM s WHERE \"b\" = 'q' OR \"b\" = 'r'"
            )),
            3
        );
        // `s`'s call, `u`'s copy of it, and the filter's copy of `c`'s.
        assert_eq!(
            calls(&format!(
                "{cte}, u AS (SELECT \"b\" || 'z' AS \"c\" FROM s) SELECT * FROM u WHERE \"c\" > 'm'"
            )),
            3
        );
        assert_eq!(
            calls(&format!(
                "{cte} SELECT max(\"b\") OVER (PARTITION BY \"b\") AS \"m\" FROM s"
            )),
            3
        );
    }

    /// A filter over a UNION is pushed into each branch: its own calls
    /// once a branch, and each column it names weighs every branch's
    /// expression for it.
    #[test]
    fn a_filter_over_a_union_weighs_every_branch() {
        let sql = "WITH s AS (SELECT * FROM (SELECT * FROM read_parquet('p') UNION ALL BY NAME \
                   SELECT * REPLACE (regexp_replace(\"a\", 'x', '') AS \"a\") FROM read_json('j'))) \
                   SELECT * FROM s WHERE regexp_full_match(\"m\", 'y') AND \"a\" = 'z'";
        assert_eq!(calls(sql), 1 + 2 + 1);
    }

    /// A PIVOT binds its aggregates once per value, up to the threshold.
    #[test]
    fn a_pivot_weighs_its_aggregates_at_the_threshold() {
        assert_eq!(
            calls(
                "SELECT * FROM (PIVOT s ON \"k\" USING max(regexp_replace(\"v\", 'x', '')) GROUP BY \"g\")"
            ),
            PIVOT_FILTER_THRESHOLD
        );
    }

    /// A common table expression binds once per place it is named.
    #[test]
    fn a_common_table_expression_weighs_once_a_reference() {
        let cte = "WITH s AS (SELECT regexp_replace(a, 'x', '') AS \"b\" FROM t)";
        assert_eq!(
            calls(&format!("{cte} SELECT * FROM s UNION ALL SELECT * FROM s")),
            2
        );
        assert_eq!(
            calls(&format!(
                "{cte}, u AS (SELECT * FROM s UNION ALL SELECT * FROM s) SELECT * FROM u UNION ALL SELECT * FROM u"
            )),
            4
        );
    }

    /// A lambda's parameter is no column, so it borrows no weight.
    #[test]
    fn a_lambda_parameter_is_no_column() {
        let cte = "WITH s AS (SELECT regexp_replace(a, 'x', '') AS \"c\" FROM t)";
        assert_eq!(
            calls(&format!(
                "{cte} SELECT COLUMNS(c -> translate(c, 'A', 'a') NOT IN ('x')) FROM s"
            )),
            1
        );
        assert_eq!(
            calls(&format!(
                "{cte} SELECT list_transform([\"c\"], c -> c || 'q') AS \"l\" FROM s"
            )),
            2
        );
    }

    /// A COLUMNS expansion around a weighed part has the input's width,
    /// which the text cannot know: the count saturates.
    #[test]
    fn a_columns_expansion_around_a_call_saturates() {
        assert_eq!(
            calls("SELECT regexp_replace(COLUMNS(*), 'x', '') FROM t"),
            u64::MAX
        );
        assert_eq!(
            calls("SELECT * FROM t WHERE regexp_full_match(COLUMNS(*), 'x')"),
            u64::MAX
        );
        assert_eq!(calls("SELECT COLUMNS('^a') FROM t"), 1);
    }

    /// The severity reading through `sev()`: two trims, the ASCII gate in
    /// every one of the 38 token arms, and the digits guard, as
    /// `DuckDB`'s bound plan holds them.
    #[test]
    fn the_bind_once_severity_reading_weighs_what_duckdb_binds() {
        for dialect in [Dialect::Otel, Dialect::Syslog] {
            let sev = crate::conform::severity_reading_sql_bind_once("\"m\"", dialect);
            assert_eq!(
                calls(&format!("SELECT {sev} AS \"s\" FROM t")),
                41,
                "{dialect:?}"
            );
        }
    }

    /// The binder writes a BETWEEN's operand into both of its bounds'
    /// comparisons, NOT BETWEEN's too; the bounds and the conjunction
    /// after it bind once.
    #[test]
    fn a_between_weighs_its_operand_twice() {
        let x = "regexp_replace(m, 'a', '')";
        assert_eq!(calls(&format!("SELECT {x} BETWEEN '1' AND '2' FROM t")), 2);
        assert_eq!(
            calls(&format!("SELECT {x} NOT BETWEEN '1' AND '2' FROM t")),
            2
        );
        assert_eq!(
            calls(&format!(
                "SELECT * FROM t WHERE {x} = 'q' AND m BETWEEN {x} AND '9' AND {x} BETWEEN '1' AND '2'"
            )),
            1 + 1 + 2
        );
        assert_eq!(
            calls(&format!(
                "SELECT CASE WHEN TRY_CAST({x} AS BIGINT) BETWEEN 1 AND 24 THEN 1 END FROM t"
            )),
            2
        );
    }

    /// The repeated reading the hot branch conforms a `SEVERITY` pin
    /// with: the trimmed subject in the gate and the fold of every token
    /// arm, the digits guard, and the numeric arm's cast, whose BETWEEN
    /// writes it twice (`OTel`) or whose eight-arm CASE writes it eight
    /// times (syslog), as `DuckDB`'s bound plan holds them.
    #[test]
    fn the_repeated_severity_reading_weighs_what_duckdb_binds() {
        for (dialect, bound) in [(Dialect::Otel, 199), (Dialect::Syslog, 209)] {
            let reading = crate::conform::severity_reading_sql("\"m\"", dialect);
            assert_eq!(
                calls(&format!("SELECT {reading} AS \"s\" FROM t")),
                bound,
                "{dialect:?}"
            );
        }
    }
}
