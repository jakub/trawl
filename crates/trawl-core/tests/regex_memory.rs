//! Pins the regex-automata 0.4.16 memory facts that the regex budget relies
//! on (ADR-0063). Each test is one fact about the library itself, measured
//! with a counting global allocator, checked against the accounting in
//! `trawl_core::pattern::charge`. The last also measures a pattern built
//! through trawl's own `Preparer` against the charge it carries.
//!
//! The heap counters are process-wide. Run the binary single-threaded so the
//! test harness cannot interleave allocations with a measurement:
//!
//! ```text
//! cargo test -p trawl-core --test regex_memory -- --test-threads=1 --nocapture
//! ```
//!
//! Every test also takes [`serial`], so a parallel run cannot mix two
//! measurements.
//!
//! `peak_alloc` grows a block with a fresh allocation and frees the old one
//! after the copy, so a measured peak counts both. Measured peaks therefore
//! run a little high, which errs against the charge.

#![expect(
    clippy::disallowed_methods,
    reason = "this file measures regex-automata's own builders and engines, outside the pattern owner on purpose (ADR-0063)"
)]

use std::fmt::Write as _;
use std::sync::{Mutex, MutexGuard, PoisonError};

use peak_alloc::PeakAlloc;
use regex_automata::nfa::thompson::{self, NFA, WhichCaptures};
use regex_automata::util::look::LookMatcher;
use regex_automata::util::syntax;
use regex_automata::{Input, MatchKind, hybrid, meta};
use regex_syntax::hir::{self, Hir, HirKind};
use trawl_core::pattern::charge::{
    self, BUILD_TRANSIENT, CACHE_MAX, CACHE_SLACK, Charge, K_R, LAZY_DFAS,
};
use trawl_core::pattern::{PatternKind, Preparer};

#[global_allocator]
static HEAP: PeakAlloc = PeakAlloc;

static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(PoisonError::into_inner)
}

const KIB: usize = 1 << 10;
const MIB: usize = 1 << 20;

/// Part 1's compiled-size limit, `MAX_REGEX_COMPILED_BYTES`.
const NFA_SIZE_LIMIT: usize = trawl_core::pattern::MAX_REGEX_COMPILED_BYTES;

/// A body within the repeat budget whose program is over the 1 MiB limit,
/// so meta refuses it.
const OVER_LIMIT: &str = r"[\pL\pN]{64}";

/// The DFA blowup family: `2^17` lazy states over `a` and `b`.
const BLOWUP: &str = r"(a|b)*a(a|b){16}";

/// The calibration corpus and Unicode table the charge is calibrated
/// over, then a body over the size limit.
fn every_body() -> impl Iterator<Item = &'static str> {
    charge::LOG_REGEXES
        .iter()
        .chain(charge::UNICODE_REGEXES)
        .copied()
        .chain([OVER_LIMIT])
}

// --- Heap window ---------------------------------------------------------

/// Live bytes and the peak since [`Window::open`].
struct Window {
    base: usize,
}

impl Window {
    fn open() -> Self {
        HEAP.reset_peak_usage();
        Self {
            base: HEAP.current_usage(),
        }
    }

    fn live(&self) -> usize {
        HEAP.current_usage().saturating_sub(self.base)
    }

    fn peak(&self) -> usize {
        HEAP.peak_usage().saturating_sub(self.base)
    }

    /// Restarts the peak at the current live bytes, keeping the base.
    fn restart_peak() {
        HEAP.reset_peak_usage();
    }
}

#[expect(clippy::cast_precision_loss, reason = "byte counts in a printed table")]
fn ratio(a: usize, b: usize) -> f64 {
    a as f64 / b as f64
}

fn mib(bytes: usize) -> f64 {
    ratio(bytes, MIB)
}

// --- The program meta builds ---------------------------------------------

/// The regex crate's syntax config (regex-1.13.1 builders.rs:81).
fn parse(body: &str) -> Hir {
    syntax::parse_with(body, &syntax::Config::new().utf8(true)).expect("the body parses")
}

/// The plan's R1 meta config at lazy-DFA capacity `cap`.
fn meta_config(cap: usize) -> meta::Config {
    meta::Config::new()
        .match_kind(MatchKind::LeftmostFirst)
        .utf8_empty(true)
        .nfa_size_limit(Some(NFA_SIZE_LIMIT))
        .hybrid_cache_capacity(cap)
        .dfa(false)
        .backtrack(false)
}

/// `None` over the 1 MiB size limit, the only way these bodies fail.
fn build(hir: &Hir, config: meta::Config) -> Option<meta::Regex> {
    meta::Builder::new()
        .configure(config)
        .build_from_hir(hir)
        .ok()
}

/// `meta::Config::to_thompson_config` (meta/regex.rs:3283-3292).
fn thompson_config(config: &meta::Config) -> thompson::Config {
    let mut look = LookMatcher::new();
    look.set_line_terminator(config.get_line_terminator());
    thompson::Config::new()
        .utf8(config.get_utf8_empty())
        .reverse(false)
        .nfa_size_limit(config.get_nfa_size_limit())
        .shrink(false)
        .which_captures(config.get_which_captures())
        .look_matcher(look)
}

/// The forward NFA `Core::new` compiles (strategy.rs:473-476), or `None`
/// over the size limit.
fn forward_nfa(hir: &Hir, config: &meta::Config) -> Option<NFA> {
    thompson::Compiler::new()
        .configure(thompson_config(config))
        .build_from_hir(hir)
        .ok()
}

/// A reverse NFA without captures, as `Core::new` (strategy.rs:511-523) and
/// `ReverseInner::new` (strategy.rs:1617-1625) compile it, or `None` over the
/// size limit.
fn reverse_nfa(hir: &Hir, config: &meta::Config) -> Option<NFA> {
    thompson::Compiler::new()
        .configure(
            thompson_config(config)
                .which_captures(WhichCaptures::None)
                .reverse(true),
        )
        .build_from_hir(hir)
        .ok()
}

/// The lazy-DFA knobs that move the minimum: the quit set (from
/// `unicode_word_boundary`), byte classes, and start states. The core lazy
/// DFAs start per pattern (wrappers.rs:552-587); `ReverseInner`'s prefix DFA
/// does not (wrappers.rs:1103-1113).
fn minimum(nfa: &NFA, starts_for_each_pattern: bool) -> usize {
    hybrid::dfa::Config::new()
        .starts_for_each_pattern(starts_for_each_pattern)
        .byte_classes(true)
        .unicode_word_boundary(true)
        .get_minimum_cache_capacity(nfa)
        .expect("the minimum computes")
}

/// `reverse_inner::top_concat` (meta/reverse_inner.rs:249-283).
fn top_concat(mut hir: &Hir) -> Option<Vec<Hir>> {
    loop {
        hir = match hir.kind() {
            HirKind::Empty
            | HirKind::Literal(_)
            | HirKind::Class(_)
            | HirKind::Look(_)
            | HirKind::Repetition(_)
            | HirKind::Alternation(_) => return None,
            HirKind::Capture(hir::Capture { sub, .. }) => sub,
            HirKind::Concat(subs) => {
                return match Hir::concat(subs.iter().map(flatten).collect()).into_kind() {
                    HirKind::Concat(xs) => Some(xs),
                    _ => None,
                };
            }
        };
    }
}

/// `reverse_inner::flatten` (meta/reverse_inner.rs:286-303).
fn flatten(hir: &Hir) -> Hir {
    match hir.kind() {
        HirKind::Empty => Hir::empty(),
        HirKind::Literal(hir::Literal(x)) => Hir::literal(x.clone()),
        HirKind::Class(x) => Hir::class(x.clone()),
        HirKind::Look(x) => Hir::look(*x),
        HirKind::Repetition(x) => Hir::repetition(x.with(flatten(&x.sub))),
        HirKind::Capture(hir::Capture { sub, .. }) => flatten(sub),
        HirKind::Alternation(xs) => Hir::alternation(xs.iter().map(flatten).collect()),
        HirKind::Concat(xs) => Hir::concat(xs.iter().map(flatten).collect()),
    }
}

/// The profile the charge reads from a pattern's forward NFA, plus the
/// minimum of every other lazy DFA meta can build for it.
#[derive(Debug, Clone, Copy)]
struct Profile {
    charged: charge::Profile,
    min_reverse: usize,
    /// The largest minimum over every reverse prefix `ReverseInner` could
    /// pick: `InnerPrefilter::new` splits the top concatenation at some
    /// `i >= 1` (meta/reverse_inner.rs:130-199), so this covers the split
    /// it picks without copying its literal heuristics. `None` when the
    /// pattern has no top-level concatenation.
    min_inner: Option<usize>,
}

impl Profile {
    /// `None` when an NFA meta needs is over 1 MiB, so meta refuses the
    /// body too.
    fn of(hir: &Hir) -> Option<Self> {
        let config = meta_config(CACHE_MAX);
        let forward = forward_nfa(hir, &config)?;
        let reverse = reverse_nfa(hir, &config)?;
        let charged = charge::Profile::of(&forward);
        assert_eq!(
            charged.min_forward,
            minimum(&forward, true),
            "the charge reads the forward minimum as meta's lazy DFA does"
        );
        let min_inner = top_concat(hir).map(|concat| {
            // A prefix NFA over the limit makes meta skip ReverseInner
            // (strategy.rs:1626-1635).
            (1..concat.len())
                .filter_map(|i| reverse_nfa(&Hir::concat(concat[..i].to_vec()), &config))
                .map(|nfa| minimum(&nfa, false))
                .max()
                .unwrap_or(0)
        });
        Some(Self {
            charged,
            min_reverse: minimum(&reverse, true),
            min_inner,
        })
    }

    fn min_forward(&self) -> usize {
        self.charged.min_forward
    }

    fn max_minimum(&self) -> usize {
        self.min_forward()
            .max(self.min_reverse)
            .max(self.min_inner.unwrap_or(0))
    }

    /// The capacity the charge sets: `clamp(K_R × minF, CACHE_FLOOR,
    /// CACHE_MAX)`.
    fn capacity(&self) -> usize {
        self.charged.capacity()
    }

    /// The charge's Pike VM scratch ([`charge::Profile::pike`]).
    fn pike(&self) -> usize {
        self.charged.pike()
    }

    /// What the charge sets for a program `re` built at [`Self::capacity`].
    fn charge(&self, re: &meta::Regex) -> Charge {
        Charge::new(&self.charged, re.memory_usage())
    }
}

/// Which engines a built regex holds, read from its `Debug` output. `Hybrid`
/// and `ReverseHybrid` derive `Debug` over an `Option` (wrappers.rs:509,
/// :1059), so `Some(` there means the lazy DFA built.
#[derive(Debug)]
struct Engines {
    strategy: String,
    core_lazy: bool,
    inner_lazy: Option<bool>,
}

impl Engines {
    fn of(re: &meta::Regex) -> Self {
        let debug = format!("{re:?}");
        let strategy: String = debug
            .split_once("strat: ")
            .map(|(_, rest)| {
                rest.chars()
                    .take_while(char::is_ascii_alphanumeric)
                    .collect()
            })
            .unwrap_or_default();
        let inner_lazy =
            (strategy == "ReverseInner").then(|| debug.contains("hybrid: ReverseHybrid(Some("));
        Self {
            core_lazy: debug.contains("hybrid: Hybrid(Some("),
            strategy,
            inner_lazy,
        }
    }

    /// `Pre` strategies match without a core, so they have no lazy DFA.
    fn has_core(&self) -> bool {
        !self.strategy.starts_with("Pre")
    }
}

// --- Inputs --------------------------------------------------------------

/// xorshift64, so every run searches the same bytes.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(6_364_136_223_846_793_005) | 1)
    }

    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 >> 11).expect("53 bits fit usize") % n
    }
}

/// Words, digits, punctuation, Latin-1, Greek, Cyrillic, CJK, Deseret and
/// emoji, with spaces and newlines.
const MIXED: &[char] = &[
    'a', 'b', 'c', 'x', 'y', 'z', 'E', 'R', 'O', '_', '0', '1', '2', '9', ':', '.', '-', '@', '=',
    '"', '[', ']', 'é', 'ß', 'ø', 'ñ', 'α', 'β', 'Ω', 'Ж', '漢', '字', '文', 'ア', '𐐀', '𐐁', '😀',
    '🦀', ' ', ' ', ' ', '\t', '\n',
];

fn text(seed: u64, len: usize, alphabet: &[char]) -> String {
    let mut rng = Rng::new(seed);
    (0..len)
        .map(|_| alphabet[rng.below(alphabet.len())])
        .collect()
}

/// Scalar values drawn from wide blocks (Latin, Greek, Cyrillic, Arabic,
/// Devanagari, CJK, Hangul and the supplementary planes), one in eight a
/// space, so a `\w` lazy DFA walks far more of its UTF-8 trie than a fixed
/// alphabet reaches.
fn wide(seed: u64, len: usize) -> String {
    const BLOCKS: &[(u32, u32)] = &[
        (0x30, 0x7B),
        (0xC0, 0x250),
        (0x370, 0x530),
        (0x600, 0x700),
        (0x900, 0xD80),
        (0x4E00, 0xA000),
        (0xAC00, 0xD7A4),
        (0x1_0000, 0x1_FA00),
        (0x2_0000, 0x2_A6E0),
    ];
    let mut rng = Rng::new(seed);
    let mut out = String::with_capacity(len * 3);
    while out.chars().count() < len {
        if rng.below(8) == 0 {
            out.push(' ');
            continue;
        }
        let (lo, hi) = BLOCKS[rng.below(BLOCKS.len())];
        let span = usize::try_from(hi - lo).expect("a block fits usize");
        let offset = u32::try_from(rng.below(span)).expect("a block offset fits u32");
        if let Some(c) = char::from_u32(lo + offset) {
            out.push(c);
        }
    }
    out
}

const LOG_LINES: &[&str] = &[
    r#"10.1.2.3 - alice [04/Nov/2026:10:00:00 +0000] "GET /api/v1/query HTTP/1.1" 200 5123"#,
    r#"ts=2026-11-04T10:00:00.123Z level=info msg="request done" path=/api/v1/stream user=bob dur=12ms"#,
    "Nov  4 10:00:00 host sshd[1234]: Failed password for invalid user ädmin from 192.0.2.7 — 失敗",
    "mac 00:1a:2B:3c:4D:5e uuid 123e4567-e89b-12d3-a456-426614174000 mail a.b+c@d-e.fg timeout",
];

/// Varied Unicode and ASCII text that drives each lazy DFA through many
/// states.
fn saturating_inputs() -> Vec<String> {
    let mut inputs: Vec<String> = (0..24).map(|seed| text(seed, 4096, MIXED)).collect();
    inputs.extend((300..316).map(|seed| wide(seed, 4096)));
    inputs.extend((100..108).map(|seed| text(seed, 4096, &['a', 'b', 'a', 'b', 'c', ' '])));
    inputs
        .extend((200..204).map(|seed| text(seed, 4096, &['0', '1', '9', 'f', 'A', ':', '.', '-'])));
    inputs.extend(LOG_LINES.iter().map(|line| line.repeat(32)));
    inputs
}

/// Runs both kinds of match a `Pattern` runs: `is_match` as a half search
/// that stops early, and `captures`.
fn saturate(re: &meta::Regex, cache: &mut meta::Cache, inputs: &[String]) {
    let mut caps = re.create_captures();
    for haystack in inputs {
        let _ = re.search_half_with(cache, &Input::new(haystack).earliest(true));
        re.search_captures_with(cache, &Input::new(haystack), &mut caps);
    }
}

// --- Facts ---------------------------------------------------------------

/// Below its minimum capacity a lazy DFA does not build, and meta builds the
/// regex anyway with the lazy DFA off: `HybridEngine::new` suppresses the
/// build error (wrappers.rs:569-614, `skip_cache_capacity_check(false)` at
/// :583). The forward and reverse DFAs build as one engine, so a capacity
/// that fits the forward minimum but not the reverse one turns both off.
///
/// Detection for `lazy_dfa_stays_enabled` (CP3): build the same Hir with
/// `hybrid(false)` and compare `create_cache().memory_usage()`. `Cache`
/// sums every engine's cache (meta/regex.rs:2426-2434), a missing lazy DFA
/// adds zero, and a built one adds its fresh sentinel and start states. So
/// the lazy DFA is on exactly when the fresh cache is larger than its
/// `hybrid(false)` twin. The `Debug` reading in [`Engines`] agrees and also
/// tells `ReverseInner`'s prefix DFA apart.
#[test]
fn hybrid_below_minimum_is_silently_disabled() {
    let _serial = serial();
    let hir = parse(r"\w{20}");
    let profile = Profile::of(&hir).expect("\\w{20} compiles under 1 MiB");
    assert!(
        profile.min_reverse > profile.min_forward(),
        "the reverse minimum dominates: {profile:?}"
    );
    let fresh = |config: meta::Config| {
        let re = build(&hir, config).expect("meta builds below the minimum");
        let bytes = re.create_cache().memory_usage();
        (bytes, Engines::of(&re))
    };
    let (off, _) = fresh(meta_config(CACHE_MAX).hybrid(false));
    let (below, below_engines) = fresh(meta_config(profile.min_forward() - 1));
    let (between, between_engines) = fresh(meta_config(profile.min_reverse - 1));
    let (at, at_engines) = fresh(meta_config(profile.min_reverse));
    println!(
        "\\w{{20}}: minF={} minR={} fresh cache: hybrid(false)={off} below minF={below} \
         minF<=C<minR={between} C=minR={at}",
        profile.min_forward(),
        profile.min_reverse
    );
    assert_eq!(below, off, "below minF the lazy DFA is off");
    assert_eq!(between, off, "below minR both lazy DFAs are off");
    assert!(at > off, "at minR the lazy DFA is on");
    assert!(!below_engines.core_lazy && !between_engines.core_lazy);
    assert!(at_engines.core_lazy);
}

/// Every lazy DFA meta can build for a pattern has a minimum within
/// `K_R × minF`, so R3's capacity keeps them on. Checked for the forward
/// and reverse DFAs and every `ReverseInner` prefix split, and confirmed by
/// building each pattern at its capacity.
#[test]
fn every_lazy_dfa_minimum_fits_k_r() {
    let _serial = serial();
    let mut table = String::from(
        "pattern                                   minF     minR    inner  ratio        C strategy      lazy@C\n",
    );
    let mut failures = Vec::new();
    for body in every_body() {
        let hir = parse(body);
        let Some(profile) = Profile::of(&hir) else {
            writeln!(table, "{:<38} over 1 MiB", short(body)).expect("writes to a String");
            continue;
        };
        let worst = profile.max_minimum();
        let capacity = profile.capacity();
        let built = build(&hir, meta_config(capacity));
        let engines = built.as_ref().map(Engines::of);
        let lazy = match &engines {
            Some(e) if e.has_core() => format!("{}/{:?}", e.core_lazy, e.inner_lazy),
            Some(_) => "no core".to_owned(),
            None => "over 1 MiB".to_owned(),
        };
        let strategy = engines.as_ref().map_or("-", |e| e.strategy.as_str());
        writeln!(
            table,
            "{:<38} {:>8} {:>8} {:>8} {:>6.2} {:>8} {:<13} {lazy}",
            short(body),
            profile.min_forward(),
            profile.min_reverse,
            profile.min_inner.map_or("-".to_owned(), |m| m.to_string()),
            ratio(worst, profile.min_forward()),
            capacity,
            strategy,
        )
        .expect("writes to a String");
        if worst > K_R * profile.min_forward() || worst > capacity {
            failures.push(body);
        }
        if let Some(e) = &engines
            && e.has_core()
            && (!e.core_lazy || e.inner_lazy == Some(false))
        {
            failures.push(body);
        }
    }
    println!("{table}");
    assert!(
        failures.is_empty(),
        "lazy DFA minimum over K_R × minF or C: {failures:?}"
    );
}

fn short(body: &str) -> String {
    if body.chars().count() <= 38 {
        body.to_owned()
    } else {
        format!("{}…", body.chars().take(37).collect::<String>())
    }
}

/// A saturated lazy cache keeps its peak heap within `CACHE_SLACK × C` per
/// lazy DFA. A clear keeps every `Vec`'s capacity (hybrid/dfa.rs:2353-2356)
/// and `memory_usage` counts lengths (:2021-2038), so the heap is the
/// high-water mark, not `memory_usage`.
///
/// Meta does not expose its lazy caches, and once a lazy DFA gives up (three
/// clears at under 10 bytes a state, wrappers.rs:586-587) it stays full, so
/// clears are not visible between searches. A forward lazy DFA built
/// alone, with meta's knobs at the same capacity, counts how often the same
/// inputs clear it. A DFA whose whole reachable state set fits in `C`, as
/// `\w{20}`'s forward DFA does at 2 MiB, never clears: its plateau is the
/// worst case there.
#[test]
fn lazy_cache_heap_stays_under_slack() {
    let _serial = serial();
    let blowup = |seed| text(seed, 2048, &['a', 'b']);
    let cases: &[(&str, &dyn Fn(u64) -> String)] =
        &[(r"\w{20}", &|seed| wide(seed, 8192)), (BLOWUP, &blowup)];
    let mut table = String::from(
        "pattern                   C      peak  peak/C dirs per-dir  max used  clears lazy\n",
    );
    let mut failures = Vec::new();
    for &(body, input) in cases {
        let hir = parse(body);
        let inputs: Vec<String> = (0..240).map(input).collect();
        for capacity in [64 * KIB, 800_000, CACHE_MAX] {
            let re = build(&hir, meta_config(capacity)).expect("meta builds");
            let engines = Engines::of(&re);
            let dirs = 2 + usize::from(engines.inner_lazy == Some(true));
            let mut caps = re.create_captures();
            let window = Window::open();
            let mut cache = re.create_cache();
            let mut max_used = 0;
            for haystack in &inputs {
                let _ = re.search_half_with(&mut cache, &Input::new(haystack).earliest(true));
                re.search_captures_with(&mut cache, &Input::new(haystack), &mut caps);
                max_used = max_used.max(cache.memory_usage());
            }
            let peak = window.peak();
            drop(cache);
            let clears = forward_clears(&hir, capacity, &inputs);
            writeln!(
                table,
                "{body:<18} {capacity:>8} {peak:>9} {:>7.2} {dirs:>4} {:>7.2} {max_used:>9} {:>7} {}",
                ratio(peak, capacity),
                ratio(peak, dirs * capacity),
                clears.map_or("-".to_owned(), |c| c.to_string()),
                engines.core_lazy,
            )
            .expect("writes to a String");
            if peak > LAZY_DFAS * CACHE_SLACK * capacity
                || (engines.core_lazy && peak > dirs * CACHE_SLACK * capacity)
            {
                failures.push((body, capacity));
            }
            if engines.core_lazy && body == BLOWUP {
                assert!(
                    clears.is_some_and(|c| c >= 3),
                    "{body} at {capacity} cleared {clears:?} times"
                );
            }
        }
    }
    println!("{table}");
    assert!(
        failures.is_empty(),
        "peak over CACHE_SLACK × C a direction: {failures:?}"
    );
}

/// How often `inputs` clear a forward lazy DFA of `capacity`, built with
/// meta's knobs (wrappers.rs:552-587). `None` below its minimum.
fn forward_clears(hir: &Hir, capacity: usize, inputs: &[String]) -> Option<usize> {
    let nfa = forward_nfa(hir, &meta_config(capacity))?;
    let dfa = hybrid::dfa::Builder::new()
        .configure(
            hybrid::dfa::Config::new()
                .match_kind(MatchKind::LeftmostFirst)
                .starts_for_each_pattern(true)
                .unicode_word_boundary(true)
                .cache_capacity(capacity)
                .skip_cache_capacity_check(false)
                .minimum_cache_clear_count(Some(3))
                .minimum_bytes_per_state(Some(10)),
        )
        .build_from_nfa(nfa)
        .ok()?;
    let mut cache = dfa.create_cache();
    for haystack in inputs {
        let _ = dfa.try_search_fwd(&mut cache, &Input::new(haystack));
    }
    Some(cache.clear_count())
}

/// The `PikeVM`'s cache stays within R3's `pike` formula.
#[test]
fn pikevm_scratch_matches_formula() {
    let _serial = serial();
    let inputs = saturating_inputs();
    let mut table = String::from(
        "pattern                                 states slots frames  formula measured\n",
    );
    let mut failures = Vec::new();
    for body in every_body().chain([BLOWUP]) {
        let hir = parse(body);
        let config = meta_config(CACHE_MAX).hybrid(false).onepass(false);
        let (Some(profile), Some(re)) = (Profile::of(&hir), build(&hir, config)) else {
            writeln!(table, "{:<38} over 1 MiB", short(body)).expect("writes to a String");
            continue;
        };
        let mut caps = re.create_captures();
        let window = Window::open();
        let mut cache = re.create_cache();
        for haystack in inputs.iter().take(6).chain(inputs.iter().rev().take(6)) {
            re.search_captures_with(&mut cache, &Input::new(haystack), &mut caps);
        }
        let measured = window.live();
        drop(cache);
        writeln!(
            table,
            "{:<38} {:>6} {:>5} {:>6} {:>8} {measured:>8}",
            short(body),
            profile.charged.states,
            profile.charged.slots,
            profile.charged.frames,
            profile.pike(),
        )
        .expect("writes to a String");
        if profile.pike() < measured {
            failures.push(body);
        }
    }
    println!("{table}");
    assert!(
        failures.is_empty(),
        "PikeVM cache over the formula: {failures:?}"
    );
}

/// One build's transient heap, the peak during parse, profile and meta
/// build minus what the regex keeps, stays within `BUILD_TRANSIENT` for the
/// largest bodies part 1 admits.
#[test]
fn build_transient_fits_t() {
    let _serial = serial();
    let mut bodies: Vec<String> = [
        r"\w",
        r"\pL",
        r"[\pL\pN]",
        r"\p{Han}",
        r"(?s).",
        r"\d",
        r"(?i)\w",
        r"\s",
    ]
    .iter()
    .map(|atom| largest_repeat(atom))
    .collect();
    // price.rs's "worst admitted" body for the translation budget.
    bodies.push(format!("(?i){}", r"[\x00-\x{10FFFF}]".repeat(14)));
    let mut table =
        String::from("body                                     build_peak  retained transient\n");
    let mut worst = 0;
    for body in &bodies {
        let (peak, retained) = build_window(body);
        let transient = peak.saturating_sub(retained);
        worst = worst.max(transient);
        writeln!(
            table,
            "{:<38} {peak:>12} {retained:>9} {transient:>9}",
            short(body)
        )
        .expect("writes to a String");
    }
    println!(
        "{table}worst transient {:.2} MiB, T {:.2} MiB",
        mib(worst),
        mib(BUILD_TRANSIENT)
    );
    assert!(worst <= BUILD_TRANSIENT, "a build's transient is over T");
}

/// `atom{N}` for the largest `N <= 64` that compiles under 1 MiB.
fn largest_repeat(atom: &str) -> String {
    let fits =
        |n: usize| build(&parse(&format!("{atom}{{{n}}}")), meta_config(CACHE_MAX)).is_some();
    let (mut lo, mut hi) = (1, 64);
    if fits(hi) {
        return format!("{atom}{{{hi}}}");
    }
    while lo + 1 < hi {
        let mid = usize::midpoint(lo, hi);
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    format!("{atom}{{{lo}}}")
}

/// Returns the peak heap from parse through meta build and the heap the
/// built regex keeps.
fn build_window(body: &str) -> (usize, usize) {
    let window = Window::open();
    let hir = parse(body);
    let profile = Profile::of(&hir).expect("the body compiles under 1 MiB");
    let re = build(&hir, meta_config(profile.capacity())).expect("the body compiles under 1 MiB");
    drop(hir);
    let (peak, retained) = (window.peak(), window.live());
    drop(re);
    (peak, retained)
}

/// R3's charge covers the heap a pattern holds while it matches: the
/// program, one cache saturated by `is_match` and `captures`, and one
/// `Captures`. The build's own transient is T's, so the charge plus T
/// covers the peak across the build, too.
#[test]
fn charge_formula_covers_measured_peak() {
    let _serial = serial();
    let inputs = saturating_inputs();
    let mut table = String::from(
        "pattern                                charge MiB  match peak  build peak  ratio\n",
    );
    let mut failures = Vec::new();
    for body in every_body().chain([BLOWUP]) {
        let window = Window::open();
        let hir = parse(body);
        let Some(profile) = Profile::of(&hir) else {
            writeln!(table, "{:<38} over 1 MiB", short(body)).expect("writes to a String");
            continue;
        };
        let Some(re) = build(&hir, meta_config(profile.capacity())) else {
            writeln!(table, "{:<38} over 1 MiB", short(body)).expect("writes to a String");
            continue;
        };
        let charge = profile.charge(&re).total();
        drop(hir);
        let build_peak = window.peak();
        Window::restart_peak();
        let mut cache = re.create_cache();
        saturate(&re, &mut cache, &inputs);
        let match_peak = window.peak();
        drop(cache);
        drop(re);
        writeln!(
            table,
            "{:<38} {:>10.3} {:>11.3} {:>11.3} {:>6.2}",
            short(body),
            mib(charge),
            mib(match_peak),
            mib(build_peak),
            ratio(charge, match_peak),
        )
        .expect("writes to a String");
        if charge < match_peak || charge + BUILD_TRANSIENT < build_peak {
            failures.push(body);
        }
    }
    println!("{table}");
    assert!(
        failures.is_empty(),
        "measured peak over the charge: {failures:?}"
    );
}

/// A pattern built through trawl's own `Preparer` holds no more heap than
/// the charge it carries while it matches, through `is_match` and
/// `captures`, and its build no more than that charge and one
/// `BUILD_TRANSIENT`: the parse, the profile, the program, a regex body's
/// batch view and the pattern's cache together.
#[test]
fn a_prepared_pattern_stays_within_its_charge() {
    let _serial = serial();
    let inputs = saturating_inputs();
    let mut table = String::from(
        "pattern                                charge MiB  match peak  build peak  ratio\n",
    );
    let mut failures = Vec::new();
    for body in every_body().chain([BLOWUP]) {
        let window = Window::open();
        let mut ledger = Preparer::new();
        let Ok(pattern) = ledger.build(PatternKind::Regex, body) else {
            writeln!(table, "{:<38} not built", short(body)).expect("writes to a String");
            continue;
        };
        let charge = pattern.charge().total();
        let build_peak = window.peak();
        Window::restart_peak();
        for haystack in &inputs {
            let _ = pattern.is_match(haystack);
            let _ = pattern.captures(haystack);
        }
        let match_peak = window.peak();
        drop(pattern);
        drop(ledger);
        writeln!(
            table,
            "{:<38} {:>10.3} {:>11.3} {:>11.3} {:>6.2}",
            short(body),
            mib(charge),
            mib(match_peak),
            mib(build_peak),
            ratio(charge, match_peak),
        )
        .expect("writes to a String");
        if charge < match_peak || charge + BUILD_TRANSIENT < build_peak {
            failures.push(body);
        }
    }
    println!("{table}");
    assert!(
        failures.is_empty(),
        "measured peak over the charge: {failures:?}"
    );
}
