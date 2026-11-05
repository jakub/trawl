// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What one prepared pattern charges against its query's regex budget
//! (ADR-0063).
//!
//! This is the accounting. A pattern's charge is the heap it can hold
//! while it matches: its program, the caches of its lazy DFAs, the Pike
//! VM's scratch and its capture slots. The build computes it once, from
//! integers it reads off the pattern's forward NFA and its program, so a
//! pattern charges the same bytes on every build: no clock and no hash
//! order enter it. Its lazy-DFA cache capacity comes from the same
//! integers ([`Profile::capacity`]).
//!
//! The formula follows regex-automata's internals, so the root
//! `Cargo.toml` pins that crate's exact version. `tests/regex_memory.rs`
//! checks each term against a counting allocator at that version, and
//! `pattern::tests::corpus_charges` snapshots what the corpus charges for
//! its caches and scratch, so a dependency bump or a changed constant
//! shows as a diff. The program term follows the CPU's choice of
//! prefilter, so that test holds it under a ceiling instead.
//!
//! A query is admitted while the sum of its patterns' charges plus one
//! [`BUILD_TRANSIENT`] fits [`REGEX_QUERY_BUDGET`]. Capacities never grow
//! into a budget a query leaves unused: each pattern's capacity follows
//! from its own minimum, and the budget refuses what does not fit.

use regex_automata::hybrid;
use regex_automata::nfa::thompson::{NFA, State};

const KIB: usize = 1 << 10;
const MIB: usize = 1 << 20;

/// The most regex memory one query's prepared patterns may charge, with
/// one [`BUILD_TRANSIENT`], in bytes. A fixed constant with no config key,
/// like `MAX_LATERAL_EXPANSION`.
///
/// At 32 MiB, the whole of [`LOG_REGEXES`] fits one query, and so do three
/// patterns near the 1 MiB compiled-size limit, such as `\w{20}`, at about
/// 7.3 MiB each. A fourth does not. An ordinary query of ASCII patterns
/// charges a few hundred KiB a pattern.
pub const REGEX_QUERY_BUDGET: usize = 32 * MIB;

/// How far over a pattern's forward lazy-DFA minimum its cache capacity
/// is set.
///
/// A pattern's reverse lazy DFA, and the reverse prefix DFA of the
/// `ReverseInner` strategy, have minimums of their own, and below any of
/// them meta turns the lazy DFA off without an error. One capacity serves
/// all three. Over the corpus and the Unicode table, the largest of those
/// minimums is at most 5.10 times the forward one, for `\d{20}`, so 6
/// keeps each of them on (`tests/regex_memory.rs`,
/// `every_lazy_dfa_minimum_fits_k_r`).
pub const K_R: usize = 6;

/// The smallest lazy-DFA cache capacity a pattern gets, in bytes. A
/// pattern with a tiny minimum would otherwise clear its cache so often
/// that meta gives the lazy DFA up.
pub const CACHE_FLOOR: usize = 16 * KIB;

/// The largest lazy-DFA cache capacity a pattern gets, in bytes: the
/// `regex` crate's default (regex 1.13.1, `builders.rs:54`), so no
/// pattern gets a larger cache than it had on that crate.
pub const CACHE_MAX: usize = 2 * MIB;

/// How much heap a lazy DFA can hold for each byte of its capacity.
///
/// A lazy DFA's `memory_usage` counts the length of its vectors, not
/// their capacity, and a clear frees nothing (regex-automata 0.4.16,
/// `hybrid/dfa.rs:2021-2038`, `:2355`), so its heap can pass its
/// capacity. The peak measured is 1.62 times the capacity a direction
/// (`\w{20}` at 800 000 bytes, `lazy_cache_heap_stays_under_slack`), so
/// 2 leaves a 23 % margin.
pub const CACHE_SLACK: usize = 2;

/// The lazy DFAs one pattern can build: forward, reverse, and the reverse
/// prefix of the `ReverseInner` strategy (`meta/strategy.rs:1789`). Every
/// pattern is charged for three, whatever strategy meta picks for it.
pub const LAZY_DFAS: usize = 3;

/// The heap a built program holds past what its `memory_usage` reports,
/// in bytes. The retained heap was measured about 4.3 KB over it.
pub const PROGRAM_OVERHEAD: usize = 8 * KIB;

/// The heap one build holds while it runs and frees when it returns, in
/// bytes: the parse, the profile's NFA and meta's own compile. Charged
/// once a query, because a ledger builds one pattern at a time. The worst
/// measured is 2.25 MiB, at the largest bodies the compiled-size limit
/// admits (`build_transient_fits_t`).
pub const BUILD_TRANSIENT: usize = 4 * MIB;

/// The integers a pattern's charge and capacity are computed from, read
/// off its forward Thompson NFA, the one meta compiles first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// The NFA's states.
    pub states: usize,
    /// Its capture slots, two for each group, the whole match included.
    pub slots: usize,
    /// The epsilon splits the Pike VM's stack can hold at once: an
    /// alternation's branches after the first, each binary split and each
    /// capture.
    pub frames: usize,
    /// The smallest cache capacity its forward lazy DFA builds with.
    pub min_forward: usize,
}

impl Profile {
    /// Read the profile of `nfa`, a forward NFA compiled as meta compiles
    /// one.
    ///
    /// The minimum is computed under the lazy-DFA settings meta builds
    /// with (`meta/wrappers.rs:552-587`): start states for each pattern,
    /// byte classes, and heuristic Unicode word boundaries. That
    /// computation fails only for a Unicode word boundary with the
    /// heuristic off. Meta turns it on, so its own lazy DFA would fail in
    /// the same case, and no cache would exist to size; the minimum reads
    /// as 0 then.
    #[must_use]
    pub fn of(nfa: &NFA) -> Self {
        let frames = nfa
            .states()
            .iter()
            .map(|state| match state {
                State::Union { alternates } => alternates.len().saturating_sub(1),
                State::BinaryUnion { .. } | State::Capture { .. } => 1,
                _ => 0,
            })
            .sum();
        let min_forward = hybrid::dfa::Config::new()
            .starts_for_each_pattern(true)
            .byte_classes(true)
            .unicode_word_boundary(true)
            .get_minimum_cache_capacity(nfa)
            .unwrap_or(0);
        Self {
            states: nfa.states().len(),
            slots: nfa.group_info().slot_len(),
            frames,
            min_forward,
        }
    }

    /// The lazy-DFA cache capacity the pattern builds with, in bytes:
    /// `K_R` times its forward minimum, within [`CACHE_FLOOR`] and
    /// [`CACHE_MAX`].
    #[must_use]
    pub fn capacity(&self) -> usize {
        K_R.saturating_mul(self.min_forward)
            .clamp(CACHE_FLOOR, CACHE_MAX)
    }

    /// The Pike VM's scratch, in bytes: two sets of active states, each a
    /// sparse set of 8 bytes a state and a slot table of `states × slots +
    /// slots` 8-byte slots (`nfa/thompson/pikevm.rs:2087-2124`), and a
    /// 16-byte stack frame for each split, doubled for vector growth.
    #[must_use]
    pub fn pike(&self) -> usize {
        let (n, s) = (self.states, self.slots);
        let slot_table = n.saturating_mul(s).saturating_add(s);
        let one_set = n.saturating_add(slot_table).saturating_mul(8);
        one_set
            .saturating_mul(2)
            .saturating_add(self.frames.saturating_mul(2 * 16))
    }
}

/// What one prepared pattern charges, in bytes, term by term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Charge {
    /// The lazy-DFA cache capacity the pattern built with
    /// ([`Profile::capacity`]). It sizes `lazy` and is no term of its own.
    pub capacity: usize,
    /// The program: its `memory_usage` and [`PROGRAM_OVERHEAD`].
    pub program: usize,
    /// [`LAZY_DFAS`] caches of `capacity`, each with [`CACHE_SLACK`].
    pub lazy: usize,
    /// The Pike VM's scratch ([`Profile::pike`]).
    pub pike: usize,
    /// 16 bytes a slot: the one-pass DFA's cache and the `Captures` each
    /// call makes.
    pub captures: usize,
}

impl Charge {
    /// The charge of a pattern with `profile` whose program reports
    /// `program_memory` bytes.
    #[must_use]
    pub fn new(profile: &Profile, program_memory: usize) -> Self {
        let capacity = profile.capacity();
        Self {
            capacity,
            program: program_memory.saturating_add(PROGRAM_OVERHEAD),
            lazy: capacity.saturating_mul(LAZY_DFAS * CACHE_SLACK),
            pike: profile.pike(),
            captures: profile.slots.saturating_mul(16),
        }
    }

    /// The whole charge, in bytes.
    #[must_use]
    pub fn total(&self) -> usize {
        self.program
            .saturating_add(self.lazy)
            .saturating_add(self.pike)
            .saturating_add(self.captures)
    }
}

/// The log-pattern corpus the parser must admit and each pattern's lazy
/// DFA must stay on for. The charge's constants are calibrated over it
/// and [`UNICODE_REGEXES`].
pub const LOG_REGEXES: &[&str] = &[
    r"\w{10}",
    r"\w+@\w+\.\w{2,10}",
    r"[\w.+-]+@[\w-]+\.[\w.]{2,10}",
    r"[A-Za-z0-9_]{3,30}",
    r"\d{1,3}(\.\d{1,3}){3}",
    r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:\d{2})",
    r"([0-9A-Fa-f]{2}:){5}[0-9A-Fa-f]{2}",
    r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}",
    r"(?i)(error|fail(ed|ure)?|exception|timeout|refused|denied)",
    r#"(?P<key>\w+)=(?P<val>"[^"]*"|\S+)"#,
    r"^(?P<ip>\d{1,3}(\.\d{1,3}){3}) - (?P<user>\S+) \[(?P<ts>[^\]]+)\] .(?P<method>[A-Z]+) (?P<path>\S+) HTTP\d\.\d. (?P<status>\d{3}) (?P<bytes>\d+|-)",
    r"(\w+\s+){5}\w+",
];

/// Unicode-heavy bodies the parser admits: the large classes at counts
/// that reach the largest lazy DFAs an admitted pattern builds. `\pL{12}`
/// is the most letters a body holds under the batch engine's cap.
pub const UNICODE_REGEXES: &[&str] = &[
    r"\pL{12}",
    r"\p{Han}+",
    r"(?i)\w{20}",
    r"\d{20}",
    r"\s{20}",
    r"(?s).{20}",
    r"\w{20}",
];
