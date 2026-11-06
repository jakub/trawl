// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What one regex call costs, in Rust and in the bundled `DuckDB`'s RE2,
//! for the worst patterns ADR-0063's limits admit, over the longest value
//! ingest stores.
//!
//! ADR-0063, as amended on 2026-11-04, holds the Rust engine to a hard
//! ceiling: one call over one stored value of at most `MAX_RAW_CHARS`
//! (65 536) characters costs under 0.5 s, in the live tail and in every
//! live plan. RE2 runs every batch regex and has no ceiling: its worst
//! measured call is reported as an envelope. This harness measures both
//! and prints the transcript that `scripts/evidence/issue-334/cost.sh`
//! commits.
//!
//! Not a correctness test and not run in CI: every part is `#[ignore]`d,
//! because a wall-clock number on a shared machine is evidence for a
//! reader, not a CI gate. The script runs four parts, each its own test,
//! so that each fits in a ten-minute command:
//!
//! - `rust_gate`: the search, timed in Rust, then the gate. It fails when
//!   any call of an admitted shape takes 0.5 s or more, and it records the
//!   host's load average at its start and its end.
//! - `re2_envelope`: the same search, timed in RE2, then the envelope. It
//!   asserts nothing about time.
//! - `re2_step`: RE2's step against the batch-engine view.
//! - `admission`: what admitting the costliest queries takes.
//!
//! ```sh
//! cargo test -p trawl-engine --test regex_cost --release -- --ignored --nocapture --test-threads=1 --exact rust_gate
//! ```
//!
//! # The search
//!
//! Matching is linear in the subject for both engines, so the per-call
//! cost is the subject length times a per-byte constant, and the constant
//! is what a pattern controls. Both engines run a lazily built DFA first.
//! A DFA state is the set of pattern positions still alive, and the
//! pattern `a X{k} [^abc]`, with `X` a class that matches all of the
//! subject's characters, needs one state for every subset of the last
//! `k + 1` characters that held an `a`. Over a pseudo-random subject that
//! is up to `2^(k+1)` states, so the DFA's cache fills, is cleared, fills
//! again, and both engines give up on it: Rust falls back to its Pike VM,
//! RE2 to its NFA. Each then pays, on every byte, for every live step of
//! the program. A Unicode word assertion makes Rust's lazy DFA quit on the
//! first non-ASCII byte, so over non-ASCII text Rust runs the Pike VM from
//! the start.
//!
//! The search composes shapes from the node kinds the walk prices
//! (`trawl_core::pattern::price`). Each repeated unit is an atom with a
//! prefix before it and a wrapper around it:
//!
//! - atoms: `[abc]`, `.` under `(?s)`, `[^\n]`, `\w`, `\pL`, `\PN`,
//!   `[abc]` and `\pL` under `(?i)`, the fan class (below), and two atoms
//!   that cost the most translation: `\p{Any}` and the full-range bracket
//!   `[\x00-\x{10FFFF}]`, each under `(?i)`;
//! - prefixes: none, `\B`, `(?:\b|\B)`, `(?m:^)?`, an empty capture group,
//!   a repeat of nothing, `(?:)?`, and an empty alternation, `(?:|)`;
//! - wrappers: none, a capture group, `?`, two nested `?`, an alternation
//!   with `[abc]`, and `{0,2}`.
//!
//! The outer shapes are `a (?:unit){k} [^abc]` for every combination,
//! `\B (?:unit){k} \PL` for every wrapped atom, and the unit written out
//! `k` times, with no prefix and with `\B`. Writing a unit out compiles to
//! the same program as counting it, but it is `k` times the text and the
//! translation, so it reaches the body cap and the translation budget:
//! the two translation atoms written out reach the budget at about 14.
//! Two-unit mixes put `j` optional big classes before a counted run of
//! `[abc]` or `\B[abc]`, and the fan class padded to the body cap reaches
//! the cap with every `k`. The fan mix, `a (?:\BX{0,2}){j} (?:X{0,2}){k}
//! [^abc]` with `X` each of the mixes' large classes and `j` each even
//! count up to 18, spends the positions and the batch-engine view
//! together: each `\B` unit is three positions and two copies of the
//! class, each plain unit two of each.
//!
//! Each composed shape takes the largest `k` that
//! `trawl_core::parser::parse` admits, so it passes every check a query
//! passes and sits at the edge of a limit: the 1024-byte body cap, the
//! repeat budget of 64 positions, the translation budget of 2^24 units a
//! query, the 1 MiB limit on the Rust program, or the 512 KiB cap on the
//! program the batch engine compiles from the body's view. A body that
//! nests a class or uses a class set operator is refused outright, so
//! none is composed. The transcript counts the limit that refuses each
//! shape's next `k`.
//!
//! Named cases sit beside the composed ones: the families of the earlier
//! transcripts, which put each priced node kind in a counted repeat or
//! nest repeats outside and inside one; the shapes review round 3 and
//! FIX4 to FIX9 reported, each at the count it was reported at; and the
//! fan class.
//!
//! The fan class is FIX7's, ported from its generator, `gen_fan.py`. It
//! holds only literal code points: alternating ASCII letters and digits,
//! one character for each UTF-8 lead byte, and along the subject's own
//! UTF-8 path one character for each sibling continuation byte, every
//! other one at the last level. Every list of byte ranges the subject's
//! bytes walk through is then long, and once a DFA gives up, both
//! engines scan those lists for every live position on every byte. A
//! class is one position, so the fan costs the most a position can.
//!
//! Every shape is measured over four alphabets: Deseret letters (4-byte,
//! with case folds), CJK ideographs (3-byte, without), three characters
//! of the Specials block, U+FFFD, U+FFFC and U+FFFB, and ASCII letters.
//! The specials' UTF-8 bytes, `EF BF xx`, sort at or near the end of each
//! sorted list of byte ranges the fan compiles to, and Rust's Pike VM
//! scans each list from its start, so FIX9 found them the costliest
//! subject for the fan. None of them is a letter, so over them a needle
//! that ends in `\PL` ends in `\pL` instead, which they never hold either. The 2026-11-03
//! transcripts also measured 4-byte letters with and without a 4-byte
//! symbol, and ASCII with a space; Deseret matched or led every one of
//! them in both engines, so they are dropped.
//!
//! The subject defeats literal prefilters by holding the pattern's
//! literals everywhere: it is eight tenths `a`, which keeps many positions
//! alive at once, or three tenths, which keeps the set of live positions
//! varied. The rest is split between `b` and `c`. A composed shape ends in
//! `[^abc]` or `\PL`, which the subject never holds, so it never matches
//! and each call scans the whole subject. A named family that ends in `c`
//! gets a subject with no `c` exactly `k + 1` characters after an `a`. The
//! harness checks that no call finds a match.
//!
//! Every admitted shape is screened with one call, over the subject that
//! is eight tenths `a`. The slowest shapes of the screen are then timed in
//! full over both subjects: Rust's ten slowest in seven calls each after a
//! warm-up, and RE2's six slowest in five, because one RE2 call of those
//! costs seconds. A shape that
//! holds a capture group has Rust's `captures` timed beside `is_match`,
//! because the live `extract` calls it. The slowest shape in each engine is
//! last timed at smaller `k`, to show how its cost scales with the count.
//!
//! # RE2's step
//!
//! A second cost does not follow the positions. RE2 sizes its DFA from a
//! fixed memory budget (`DuckDB` binds RE2's default `max_mem`, 8 MiB),
//! and a program too large for that budget fails the DFA's init: every
//! call then runs the NFA, at hundreds of milliseconds where the DFA took
//! a few. Large classes, `\pL` above all, make a program large within few
//! positions. The step part times the families FIX6 calibrated, at counts
//! on both sides of the step, over Deseret, CJK and ASCII. Each row gives
//! what the body compiles to in Rust and in its batch-engine view, which
//! writes `\w`, `\d`, `\s`, `\b` and `\B` in RE2's ASCII meanings
//! (`trawl_core::pattern::Preparer::batch_compiled_bytes`), and where the
//! parser puts it.
//!
//! # What the ceiling covers
//!
//! The ceiling is per call, over one stored value of at most
//! `MAX_RAW_CHARS` characters. Two things fall outside it: values stored
//! before the ingest cap existed keep their length, and a computed
//! subject such as `concat(message, message)` can be longer than any one
//! stored value. A query over either stays correct and pays in proportion
//! to the longer subject.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use duckdb::{Connection, params};
use trawl_core::parser::parse;
use trawl_core::pattern::{Pattern, PatternKind, Preparer};

/// `trawl_server::ingest::envelope::MAX_RAW_CHARS`, the most characters
/// ingest stores in one string value. trawl-engine cannot depend on
/// trawl-server, so the value is repeated here.
const SUBJECT_CHARS: usize = 65_536;

/// ADR-0063's hard bound on one Rust regex call over one stored value, as
/// the 2026-11-04 ruling sets it. RE2 has none.
const CEILING: Duration = Duration::from_millis(500);

/// Timed calls per case in Rust's full timing and in the admission part,
/// after one untimed warm-up call.
const RUNS: usize = 7;

/// The share of the subject that is `a`, in tenths. Three tenths keeps
/// the set of live positions varied, which is what defeats a DFA. Eight
/// tenths keeps more positions alive at once, which is what an NFA or
/// Pike VM pays for on every byte.
const A_SHARES: [u64; 2] = [3, 8];

/// The share the screen times every shape at.
const SCREEN_SHARE: u64 = 8;

/// The largest count the edge search tries: every unit is at least one
/// position, so no shape admits more.
const MAX_K: usize = 64;

/// How many of the Rust screen's slowest shapes the gate times in full.
const TOP: usize = 10;

/// How many of the RE2 screen's slowest shapes the envelope times in
/// full, and in how many calls each. Fewer than Rust's: one RE2 call of
/// the slowest shapes costs seconds, and the part must fit in a
/// ten-minute command.
const RE2_TOP: usize = 6;
const RE2_RUNS: usize = 5;

/// Rows in one `DuckDB` vector. `DuckDB` checks for an interrupt between
/// vectors, never inside one (`docs/evidence/issue-334/cancel-overhang.txt`).
const ROWS_PER_VECTOR: f64 = 2048.0;

/// A subject alphabet: `a` and `c` are the needle's ends, `b` is filler,
/// and the subject never holds `d`.
#[derive(Clone, Copy)]
struct Alphabet {
    name: &'static str,
    a: char,
    b: char,
    c: char,
    d: char,
    /// A large class the subject never holds, with no literal for a
    /// prefilter to look for: `\PL` over letters, `\pL` over the rest.
    none: &'static str,
}

/// Deseret letters: 4-byte, with case folds.
const DESERET: Alphabet = Alphabet {
    name: "deseret",
    a: '\u{10428}',
    b: '\u{10429}',
    c: '\u{1042A}',
    d: '\u{1042B}',
    none: "\\PL",
};

/// CJK ideographs: 3-byte letters with no case folds.
const CJK: Alphabet = Alphabet {
    name: "cjk",
    a: '\u{6F22}',
    b: '\u{5B57}',
    c: '\u{6587}',
    d: '\u{8A9E}',
    none: "\\PL",
};

/// Three characters of the Specials block, 3-byte and none of them a
/// letter or a word character. Their bytes, `EF BF xx`, sort at or near
/// the end of each list of byte ranges the fan class compiles to.
const SPECIALS: Alphabet = Alphabet {
    name: "fffd",
    a: '\u{FFFD}',
    b: '\u{FFFC}',
    c: '\u{FFFB}',
    d: '\u{FFFA}',
    none: "\\pL",
};

const ASCII: Alphabet = Alphabet {
    name: "ascii",
    a: 'a',
    b: 'b',
    c: 'c',
    d: 'd',
    none: "\\PL",
};

/// The alphabets the search measures over.
const ALPHABETS: [Alphabet; 4] = [DESERET, CJK, SPECIALS, ASCII];

/// The alphabets [`re2_step`] measures over: the three FIX6 calibrated
/// RE2's step on. Its families are large letter classes, which match none
/// of the specials.
const STEP_ALPHABETS: [Alphabet; 3] = [DESERET, CJK, ASCII];

fn class3(x: Alphabet) -> String {
    format!("[{}{}{}]", esc(x.a), esc(x.b), esc(x.c))
}

/// Every character but the subject's three: a needle end the subject
/// never holds, with no literal for a prefilter to look for.
fn not3(x: Alphabet) -> String {
    format!("[^{}{}{}]", esc(x.a), esc(x.b), esc(x.c))
}

/// `inner` wrapped `depth` times in `(?:…)` and the repeat operator `op`.
fn nest(op: &str, inner: &str, depth: usize) -> String {
    (0..depth).fold(inner.to_owned(), |s, _| format!("(?:{s}){op}"))
}

/// `\B` and the 3-character class.
fn b_class3(x: Alphabet) -> String {
    format!("\\B{}", class3(x))
}

/// A character as a regex literal: escaped when it is a metacharacter.
fn esc(c: char) -> String {
    if c.is_ascii_punctuation() {
        format!("\\{c}")
    } else {
        c.to_string()
    }
}

/// `body` as the transcript prints it: a control character, which the fan
/// class holds one of, is written as its escape.
fn printable(body: &str) -> String {
    body.chars()
        .map(|c| {
            if c.is_control() {
                format!("\\x{{{:X}}}", u32::from(c))
            } else {
                c.to_string()
            }
        })
        .collect()
}

fn plus_one(k: usize) -> usize {
    k + 1
}

/// FIX7's fan class over `x`'s characters (`gen_fan.py`'s `fan`, with
/// depth). See the module documentation.
fn fan(x: Alphabet) -> String {
    let chars: String = fan_chars(x).into_iter().map(esc).collect();
    format!("[{chars}]")
}

fn fan_chars(x: Alphabet) -> BTreeSet<char> {
    let ch = |bytes: &[u32]| {
        let bytes: Vec<u8> = bytes
            .iter()
            .map(|&b| u8::try_from(b).expect("a byte"))
            .collect();
        std::str::from_utf8(&bytes)
            .expect("one UTF-8 character")
            .chars()
            .next()
            .expect("one character")
    };
    let mut chars: BTreeSet<char> = "ACEGIKMOQSUWYdfhjlnprtvxz02468".chars().collect();
    for lead in 0xC2..0xE0 {
        chars.insert(ch(&[lead, 0x80 + (lead * 5) % 64]));
    }
    for lead in (0xE1..0xED).chain([0xEE, 0xEF]) {
        chars.insert(ch(&[lead, 0x80 + (lead * 3) % 64, 0x80 + (lead * 7) % 64]));
    }
    for lead in 0xF0..0xF4 {
        let second = if lead == 0xF0 { 0x90 } else { 0x80 };
        chars.insert(ch(&[lead, second, 0x81, 0x81]));
    }
    let mut buf = [0; 4];
    let path: Vec<u32> = x.a.encode_utf8(&mut buf).bytes().map(u32::from).collect();
    match path.len() {
        4 => {
            for b2 in (0x90..0xC0).filter(|&b| b != path[1]) {
                chars.insert(ch(&[0xF0, b2, 0x80 + (b2 * 7) % 64, 0x80 + (b2 * 13) % 64]));
            }
            for b3 in (0x80..0xC0).filter(|&b| b != path[2]) {
                chars.insert(ch(&[0xF0, path[1], b3, 0x80 + (b3 * 11) % 64]));
            }
            for b4 in (0x80..0xC0).step_by(2) {
                chars.insert(ch(&[0xF0, path[1], path[2], b4]));
            }
        }
        3 => {
            for b2 in (0x80..0xC0).filter(|&b| b != path[1]) {
                chars.insert(ch(&[path[0], b2, 0x80 + (b2 * 7) % 64]));
            }
            for b3 in (0x80..0xC0).step_by(2) {
                chars.insert(ch(&[path[0], path[1], b3]));
            }
        }
        _ => {}
    }
    chars.extend([x.a, x.b, x.c]);
    chars
}

/// The fan class grown with 2-byte characters until `shape` of it is as
/// long as a body may be: the fan at the 1024-byte body cap. Every other
/// code point, so that no two of them merge into one range.
fn padded_fan(x: Alphabet, shape: impl Fn(&str) -> String) -> String {
    let mut chars = fan_chars(x);
    let class = |chars: &BTreeSet<char>| {
        let inner: String = chars.iter().copied().map(esc).collect();
        format!("[{inner}]")
    };
    for c in ('\u{100}'..='\u{7FF}').step_by(2) {
        if chars.contains(&c) {
            continue;
        }
        chars.insert(c);
        if shape(&class(&chars)).len() > 1024 {
            chars.remove(&c);
            break;
        }
    }
    class(&chars)
}

/// How a template's count is chosen.
#[derive(Clone, Copy)]
enum Knob {
    /// The largest count in `1..=MAX_K` the parser admits.
    Edge,
    /// Each listed count, as a review or a fix reported it.
    At(&'static [usize]),
}

/// One shape of needle, before its alphabet and count are chosen.
struct Template {
    name: String,
    /// The atom of a composed shape, or empty for a named one.
    atom: &'static str,
    body: Box<dyn Fn(Alphabet, usize) -> String>,
    /// How far after an `a` the needle's `c` sits, for a needle that ends
    /// in `c`.
    gap: fn(usize) -> usize,
    knob: Knob,
}

fn named(
    name: &str,
    body: fn(Alphabet, usize) -> String,
    gap: fn(usize) -> usize,
    knob: Knob,
) -> Template {
    Template {
        name: name.to_string(),
        atom: "",
        body: Box::new(body),
        gap,
        knob,
    }
}

/// An atom of a composed unit.
#[derive(Clone, Copy)]
struct Atom {
    name: &'static str,
    text: fn(Alphabet) -> String,
}

const ATOMS: [Atom; 11] = [
    Atom {
        name: "[abc]",
        text: class3,
    },
    Atom {
        name: "(?s:.)",
        text: |_| "(?s:.)".to_string(),
    },
    Atom {
        name: "[^\\n]",
        text: |_| "[^\\n]".to_string(),
    },
    Atom {
        name: "\\w",
        text: |_| "\\w".to_string(),
    },
    Atom {
        name: "\\pL",
        text: |_| "\\pL".to_string(),
    },
    Atom {
        name: "\\PN",
        text: |_| "\\PN".to_string(),
    },
    Atom {
        name: "(?i:[abc])",
        text: |x| format!("(?i:{})", class3(x)),
    },
    Atom {
        name: "(?i:\\pL)",
        text: |_| "(?i:\\pL)".to_string(),
    },
    Atom {
        name: "fan",
        text: fan,
    },
    // The two atoms that cost the most translation: `(?i)` folds every
    // range of a class that spans all of Unicode. Counted, each translates
    // once; written out `k` times, each copy translates again.
    Atom {
        name: "(?i:\\p{Any})",
        text: |_| "(?i:\\p{Any})".to_string(),
    },
    Atom {
        name: "(?i:full)",
        text: |_| "(?i:[\\x00-\\x{10FFFF}])".to_string(),
    },
];

/// What goes before the atom in a composed unit.
const PREFIXES: [&str; 7] = ["", "\\B", "(?:\\b|\\B)", "(?m:^)?", "()", "(?:)?", "(?:|)"];

/// What goes around the atom in a composed unit.
#[derive(Clone, Copy)]
struct Wrap {
    name: &'static str,
    wrap: fn(&str, Alphabet) -> String,
}

const WRAPS: [Wrap; 6] = [
    Wrap {
        name: "X",
        wrap: |atom, _| atom.to_string(),
    },
    Wrap {
        name: "(X)",
        wrap: |atom, _| format!("({atom})"),
    },
    Wrap {
        name: "X?",
        wrap: |atom, _| format!("(?:{atom})?"),
    },
    Wrap {
        name: "(?:X?)?",
        wrap: |atom, _| format!("(?:(?:{atom})?)?"),
    },
    Wrap {
        name: "(?:X|[abc])",
        wrap: |atom, x| format!("(?:{atom}|{})", class3(x)),
    },
    Wrap {
        name: "X{0,2}",
        wrap: |atom, _| format!("(?:{atom}){{0,2}}"),
    },
];

/// Every composed template: see the module documentation.
fn composed() -> Vec<Template> {
    let mut out = Vec::new();
    for atom in ATOMS {
        for prefix in PREFIXES {
            for w in WRAPS {
                out.push(Template {
                    name: format!("a (?:{prefix}{}){{k}} [^abc]", w.name),
                    atom: atom.name,
                    body: Box::new(move |x, k| {
                        let unit = format!("{prefix}{}", (w.wrap)(&(atom.text)(x), x));
                        format!("{}(?:{unit}){{{k}}}{}", x.a, not3(x))
                    }),
                    gap: plus_one,
                    knob: Knob::Edge,
                });
            }
        }
    }
    for atom in ATOMS {
        for w in WRAPS {
            out.push(Template {
                name: format!("\\B (?:{}){{k}} \\PL", w.name),
                atom: atom.name,
                body: Box::new(move |x, k| {
                    let unit = (w.wrap)(&(atom.text)(x), x);
                    format!("\\B(?:{unit}){{{k}}}{}", x.none)
                }),
                gap: plus_one,
                knob: Knob::Edge,
            });
        }
    }
    for atom in ATOMS {
        for prefix in ["", "\\B"] {
            out.push(Template {
                name: format!("a ({prefix}X written k times) [^abc]"),
                atom: atom.name,
                body: Box::new(move |x, k| {
                    let unit = format!("{prefix}{}", (atom.text)(x));
                    format!("{}{}{}", x.a, unit.repeat(k), not3(x))
                }),
                gap: plus_one,
                knob: Knob::Edge,
            });
        }
    }
    out.extend(mixes());
    out.extend(fan_mixes());
    for prefix in ["", "\\B"] {
        out.push(Template {
            name: format!("a (?:{prefix}X){{k}} [^abc], X padded to 1024 bytes"),
            atom: "fan+pad",
            body: Box::new(move |x, k| {
                let shape = |class: &str| format!("{}(?:{prefix}{class}){{{k}}}{}", x.a, not3(x));
                shape(&padded_fan(x, shape))
            }),
            gap: plus_one,
            knob: Knob::Edge,
        });
    }
    out
}

/// The large classes the mixes are made of: `\pL`, the fan, `\w` and
/// `(?i:\pL)`.
const MIX_ATOMS: [Atom; 4] = [ATOMS[4], ATOMS[8], ATOMS[3], ATOMS[7]];

/// Two-unit mixes: `j` optional big classes, then a counted run of a small
/// class. A big class brings the program near a size limit in few
/// positions, and the run spends the positions left.
fn mixes() -> Vec<Template> {
    let mut out = Vec::new();
    for atom in MIX_ATOMS {
        for prefix in ["", "\\B"] {
            for j in [4_usize, 8, 16] {
                out.push(Template {
                    name: format!("a (?:X?){{{j}}} (?:{prefix}[abc]){{k}} [^abc]"),
                    atom: atom.name,
                    body: Box::new(move |x, k| {
                        format!(
                            "{}(?:{}?){{{j}}}(?:{prefix}{}){{{k}}}{}",
                            x.a,
                            (atom.text)(x),
                            class3(x),
                            not3(x)
                        )
                    }),
                    gap: plus_one,
                    knob: Knob::Edge,
                });
            }
        }
    }
    out
}

/// The fan mix, over each of the mixes' large classes: `j` units of `\B`
/// and the class under `{0,2}`, then `k` units of the class under `{0,2}`
/// alone. A `\B` unit is three positions and a plain one two, and each is
/// two copies of the class, so the mix trades the repeat budget against
/// the batch-engine view: FIX9 found the fan at `j = 14` and `k = 10`
/// over the specials at both limits at once. `j` takes every even count
/// up to 18; at 20 no plain unit fits.
fn fan_mixes() -> Vec<Template> {
    let mut out = Vec::new();
    for atom in MIX_ATOMS {
        for j in (2..=18).step_by(2) {
            out.push(Template {
                name: format!("a (?:\\BX{{0,2}}){{{j}}} (?:X{{0,2}}){{k}} [^abc]"),
                atom: atom.name,
                body: Box::new(move |x, k| {
                    let class = (atom.text)(x);
                    format!(
                        "{}(?:\\B(?:{class}){{0,2}}){{{j}}}(?:(?:{class}){{0,2}}){{{k}}}{}",
                        x.a,
                        not3(x)
                    )
                }),
                gap: plus_one,
                knob: Knob::Edge,
            });
        }
    }
    out
}

/// Review round 3's F7 core: four branches of `x` that count 1, 2, 4 and 5
/// positions, under `{0,2}`.
fn f7_core(x: &str) -> String {
    let b3 = format!("(?:(?:{x}){{0,2}}){{0,2}}");
    format!("(?:{x}|(?:{x})?|{b3}|(?:{b3})?){{0,2}}")
}

/// `inner` between `\B` and the alphabet's class [`Alphabet::none`]: the
/// subject never holds a character of that class, so the needle never
/// matches, and neither end gives a prefilter a literal to look for.
fn between_b_and_none(x: Alphabet, inner: &str) -> String {
    format!("\\B{inner}{}", x.none)
}

/// The families of the 2026-11-03 transcripts: each priced node kind in a
/// counted repeat, and repeats nested outside and inside one.
#[expect(
    clippy::too_many_lines,
    reason = "one entry per family, kept in one list"
)]
fn families() -> Vec<Template> {
    let edge =
        |name: &str, body: fn(Alphabet, usize) -> String| named(name, body, plus_one, Knob::Edge);
    vec![
        edge("a [abc]{k} c", |x, k| {
            format!("{}{}{{{k}}}{}", x.a, class3(x), x.c)
        }),
        edge("(?s) a .{k} c", |x, k| {
            format!("(?s){}.{{{k}}}{}", x.a, x.c)
        }),
        edge("a [^\\n]{k} c", |x, k| {
            format!("{}[^\\n]{{{k}}}{}", x.a, x.c)
        }),
        edge("a \\w{k} c", |x, k| format!("{}\\w{{{k}}}{}", x.a, x.c)),
        edge("a \\pL{k} c", |x, k| format!("{}\\pL{{{k}}}{}", x.a, x.c)),
        edge("a (?:\\w|\\pN|[abc]){k} c", |x, k| {
            format!("{}(?:\\w|\\pN|{}){{{k}}}{}", x.a, class3(x), x.c)
        }),
        edge("(?i) a [abc]{k} c", |x, k| {
            format!("(?i){}{}{{{k}}}{}", x.a, class3(x), x.c)
        }),
        edge("(?i) a [\\pL\\pN]{k} c", |x, k| {
            format!("(?i){}[\\pL\\pN]{{{k}}}{}", x.a, x.c)
        }),
        named(
            "a (?:[abc]{2}){k/2} c",
            |x, k| format!("{}(?:{}{{2}}){{{}}}{}", x.a, class3(x), k / 2, x.c),
            |k| 2 * (k / 2) + 1,
            Knob::Edge,
        ),
        edge("a (?:\\B[abc]){k} c", |x, k| {
            format!("{}(?:\\B{}){{{k}}}{}", x.a, class3(x), x.c)
        }),
        edge("\\b a (?s:.){k} c \\b", |x, k| {
            format!("\\b{}(?s:.){{{k}}}{}\\b", x.a, x.c)
        }),
        edge("a (?:\\B\\B\\B\\B\\B\\B\\B\\B[abc]){k} c", |x, k| {
            format!("{}(?:{}{}){{{k}}}{}", x.a, "\\B".repeat(8), class3(x), x.c)
        }),
        edge("a (?:\\b\\b[abc]){k} c", |x, k| {
            format!("{}(?:\\b\\b{}){{{k}}}{}", x.a, class3(x), x.c)
        }),
        edge("a (?:(?:\\b|\\B)[abc]){k} c", |x, k| {
            format!("{}(?:(?:\\b|\\B){}){{{k}}}{}", x.a, class3(x), x.c)
        }),
        edge("(?m) a (?:(?:^)?[abc]){k} c", |x, k| {
            format!("(?m){}(?:(?:^)?{}){{{k}}}{}", x.a, class3(x), x.c)
        }),
        edge("(?m) a (?:(?:$)?[abc]){k} c", |x, k| {
            format!("(?m){}(?:(?:$)?{}){{{k}}}{}", x.a, class3(x), x.c)
        }),
        edge("a (?:()[abc]){k} c", |x, k| {
            format!("{}(?:(){}){{{k}}}{}", x.a, class3(x), x.c)
        }),
        edge("a (?:(([abc]))){k} c", |x, k| {
            format!("{}(?:(({}))){{{k}}}{}", x.a, class3(x), x.c)
        }),
        edge("\\B ()x31 a [abc]{k} c", |x, k| {
            format!("\\B{}{}{}{{{k}}}{}", "()".repeat(31), x.a, class3(x), x.c)
        }),
        edge("a (?:(?:|)[abc]){k} c", |x, k| {
            format!("{}(?:(?:|){}){{{k}}}{}", x.a, class3(x), x.c)
        }),
        edge("a (?:(?:)?[abc]){k} c", |x, k| {
            format!("{}(?:(?:)?{}){{{k}}}{}", x.a, class3(x), x.c)
        }),
        // Nested repeats, as deep as the budget admits outside any count:
        // `k` is the depth.
        edge("a *^k([abc]) [^abc]", |x, k| {
            format!("{}{}{}", x.a, nest("*", &class3(x), k), not3(x))
        }),
        edge("a ?^k([abc]) [^abc]", |x, k| {
            format!("{}{}{}", x.a, nest("?", &class3(x), k), not3(x))
        }),
        edge("\\B *^k([abc]) [^abc]", |x, k| {
            format!("\\B{}{}", nest("*", &class3(x), k), not3(x))
        }),
        edge("\\B ?^k([abc]) [^abc]", |x, k| {
            format!("\\B{}{}", nest("?", &class3(x), k), not3(x))
        }),
        edge("\\B *?^k([abc]) [^abc]", |x, k| {
            format!("\\B{}{}", nest("*?", &class3(x), k), not3(x))
        }),
        edge("\\B ??^k([abc]) [^abc]", |x, k| {
            format!("\\B{}{}", nest("??", &class3(x), k), not3(x))
        }),
        edge("a *^k(\\B[abc]) [^abc]", |x, k| {
            format!("{}{}{}", x.a, nest("*", &b_class3(x), k), not3(x))
        }),
        edge("a ?^k(\\B[abc]) [^abc]", |x, k| {
            format!("{}{}{}", x.a, nest("?", &b_class3(x), k), not3(x))
        }),
        // Nested repeats at a fixed depth inside a `{k}`.
        edge("\\B (?:*^1([abc])){k} [^abc]", |x, k| {
            format!("\\B(?:{}){{{k}}}{}", nest("*", &class3(x), 1), not3(x))
        }),
        edge("\\B (?:?^1([abc])){k} [^abc]", |x, k| {
            format!("\\B(?:{}){{{k}}}{}", nest("?", &class3(x), 1), not3(x))
        }),
        edge("\\B (?:*?^1([abc])){k} [^abc]", |x, k| {
            format!("\\B(?:{}){{{k}}}{}", nest("*?", &class3(x), 1), not3(x))
        }),
        edge("\\B (?:??^1([abc])){k} [^abc]", |x, k| {
            format!("\\B(?:{}){{{k}}}{}", nest("??", &class3(x), 1), not3(x))
        }),
        edge("\\B (?:*^2([abc])){k} [^abc]", |x, k| {
            format!("\\B(?:{}){{{k}}}{}", nest("*", &class3(x), 2), not3(x))
        }),
        edge("\\B (?:*^5([abc])){k} [^abc]", |x, k| {
            format!("\\B(?:{}){{{k}}}{}", nest("*", &class3(x), 5), not3(x))
        }),
        edge("\\B (?:*^30([abc])){k} [^abc]", |x, k| {
            format!("\\B(?:{}){{{k}}}{}", nest("*", &class3(x), 30), not3(x))
        }),
        edge("\\B (?:?^30([abc])){k} [^abc]", |x, k| {
            format!("\\B(?:{}){{{k}}}{}", nest("?", &class3(x), 30), not3(x))
        }),
        edge("\\B (?:*^60([abc])){k} [^abc]", |x, k| {
            format!("\\B(?:{}){{{k}}}{}", nest("*", &class3(x), 60), not3(x))
        }),
        edge("a (?:?^1([abc])){k} [^abc]", |x, k| {
            format!("{}(?:{}){{{k}}}{}", x.a, nest("?", &class3(x), 1), not3(x))
        }),
        edge("a (?:*^1(\\B[abc])){k} [^abc]", |x, k| {
            format!(
                "{}(?:{}){{{k}}}{}",
                x.a,
                nest("*", &b_class3(x), 1),
                not3(x)
            )
        }),
        edge("a (?:?^1(\\B[abc])){k} [^abc]", |x, k| {
            format!(
                "{}(?:{}){{{k}}}{}",
                x.a,
                nest("?", &b_class3(x), 1),
                not3(x)
            )
        }),
        edge("a (?:*?^1(\\B[abc])){k} [^abc]", |x, k| {
            format!(
                "{}(?:{}){{{k}}}{}",
                x.a,
                nest("*?", &b_class3(x), 1),
                not3(x)
            )
        }),
        edge("a (?:??^1(\\B[abc])){k} [^abc]", |x, k| {
            format!(
                "{}(?:{}){{{k}}}{}",
                x.a,
                nest("??", &b_class3(x), 1),
                not3(x)
            )
        }),
        // The attack's own spelling: it ends in a letter the subject never
        // holds.
        edge("\\B (?:*^1([abc])){k} d", |x, k| {
            format!("\\B(?:{}){{{k}}}{}", nest("*", &class3(x), 1), x.d)
        }),
        // The zero-charge family: nodes that `trawl_core`'s walk counts as
        // no position, before each class. It must cost what `a [abc]{k} c`
        // costs.
        edge("a (?:(?:)(?i)(?-i)(?i:)[abc]){k} c", |x, k| {
            format!("{}(?:(?:)(?i)(?-i)(?i:){}){{{k}}}{}", x.a, class3(x), x.c)
        }),
    ]
}

/// The shapes reviews and fixes reported, each at the count it was
/// reported at. Most are refused now, and the transcript says by what.
#[expect(
    clippy::too_many_lines,
    reason = "one entry per reported shape, kept in one list"
)]
fn reported() -> Vec<Template> {
    let at = |name: &str, body: fn(Alphabet, usize) -> String, ks: &'static [usize]| {
        named(name, body, plus_one, Knob::At(ks))
    };
    vec![
        at(
            "round 3 F7 body: F7 core in 38 (?:…)? wrappers",
            |x, k| between_b_and_none(x, &nest("?", &f7_core("\\pL"), k)),
            &[38],
        ),
        at(
            "FIX4 F7 core over \\pL",
            |x, _| between_b_and_none(x, &f7_core("\\pL")),
            &[0],
        ),
        at(
            "FIX4 F7 twin",
            |x, _| between_b_and_none(x, "(?:\\pL|(?:\\pL)?|(?:(?:\\pL){0,2}){0,2}){0,2}"),
            &[0],
        ),
        at(
            "FIX4 \\B \\pL{k} \\PL",
            |x, k| between_b_and_none(x, &format!("\\pL{{{k}}}")),
            &[20],
        ),
        at(
            "FIX4 \\B (?:\\pL?){k} \\PL",
            |x, k| between_b_and_none(x, &format!("(?:\\pL?){{{k}}}")),
            &[23],
        ),
        at(
            "FIX5 a (?:\\B[abc]){k} c",
            |x, k| format!("{}(?:\\B{}){{{k}}}{}", x.a, class3(x), x.c),
            &[124],
        ),
        at(
            "FIX5 (?s) a (?:.?){k} [^abc]",
            |x, k| format!("(?s){}(?:.?){{{k}}}{}", x.a, not3(x)),
            &[124, 248],
        ),
        at(
            "FIX5 \\B (?:(?i)[\\x{10400}-\\x{1044F}]?){k} \\PL",
            |x, k| between_b_and_none(x, &format!("(?:(?i)[\\x{{10400}}-\\x{{1044F}}]?){{{k}}}")),
            &[248],
        ),
        at(
            "FIX5 \\B (?:\\p{Han}?){k} \\PL",
            |x, k| between_b_and_none(x, &format!("(?:\\p{{Han}}?){{{k}}}")),
            &[124, 248],
        ),
        at(
            "FIX5 (?s) a (?:\\pL?){8} (?:.?){23} [^abc]",
            |x, _| format!("(?s){}(?:\\pL?){{8}}(?:.?){{23}}{}", x.a, not3(x)),
            &[0],
        ),
        at(
            "FIX7 a fan{k} [^abc]",
            |x, k| format!("{}{}{{{k}}}{}", x.a, fan(x), not3(x)),
            &[16, 32, 48],
        ),
        at(
            "FIX7 a (?:fan?){k} [^abc]",
            |x, k| format!("{}(?:{}?){{{k}}}{}", x.a, fan(x), not3(x)),
            &[16, 30],
        ),
        at(
            "FIX7 \\B (?:fan?){k} \\PL",
            |x, k| between_b_and_none(x, &format!("(?:{}?){{{k}}}", fan(x))),
            &[30],
        ),
        at(
            "FIX7 a (?:fan?){k} [abc]{60-2k} [^abc]",
            |x, k| {
                format!(
                    "{}(?:{}?){{{k}}}{}{{{}}}{}",
                    x.a,
                    fan(x),
                    class3(x),
                    60 - 2 * k,
                    not3(x)
                )
            },
            &[8, 16, 24],
        ),
        at(
            "FIX8 plain a [abc]{k} [^abc]",
            |x, k| format!("{}{}{{{k}}}{}", x.a, class3(x), not3(x)),
            &[62],
        ),
        at(
            "FIX9 a (?:\\Bfan{0,2}){14} (?:fan{0,2}){k} [^abc]",
            |x, k| {
                let f = fan(x);
                format!(
                    "{}(?:\\B(?:{f}){{0,2}}){{14}}(?:(?:{f}){{0,2}}){{{k}}}{}",
                    x.a,
                    not3(x)
                )
            },
            &[10],
        ),
    ]
}

/// Every template the search measures: composed, then named.
fn templates() -> Vec<Template> {
    let mut all = composed();
    all.extend(families());
    all.extend(reported());
    all
}

/// A deterministic xorshift64 stream, so every run times the same subject.
struct Rng(u64);

impl Rng {
    fn below_ten(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % 10
    }
}

/// `SUBJECT_CHARS` pseudo-random characters of `x`, `a_tenths` tenths of
/// them `a` and the rest split between `b` and `c`, with no `c` exactly
/// `gap` characters after an `a`.
fn subject(x: Alphabet, gap: usize, a_tenths: u64) -> String {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut chars: Vec<char> = Vec::with_capacity(SUBJECT_CHARS);
    for p in 0..SUBJECT_CHARS {
        let draw = rng.below_ten();
        let mut next = if draw < a_tenths {
            x.a
        } else if draw < a_tenths + (10 - a_tenths) / 2 {
            x.c
        } else {
            x.b
        };
        if next == x.c && p >= gap && chars[p - gap] == x.a {
            next = x.b;
        }
        chars.push(next);
    }
    chars.into_iter().collect()
}

/// The search-stage query that carries `body` as one regex filter.
fn query(body: &str) -> String {
    format!("message=/{body}/")
}

/// The limit a parse refusal names, in a word.
fn limit_of(message: &str) -> &'static str {
    if message.starts_with("pattern repeats too much") {
        "pos"
    } else if message.starts_with("the query's patterns cost too much to translate") {
        "units"
    } else if message.contains("for the batch engine") {
        "512K"
    } else if message.starts_with("regex pattern too large") {
        "1MiB"
    } else if message.contains("too long") {
        "1024B"
    } else if message.starts_with("nested character classes") {
        "class"
    } else {
        "other"
    }
}

/// What the screen's limit words stand for.
const LIMIT_LEGEND: [(&str, &str); 7] = [
    ("pos", "the repeat budget, 64 positions"),
    ("units", "the translation budget, 2^24 units a query"),
    ("1MiB", "the compiled-size limit, 1 MiB of Rust program"),
    (
        "512K",
        "the batch-engine cap, 512 KiB of program from the body's view",
    ),
    ("1024B", "the body cap, 1024 bytes"),
    ("end", "none: k reached the end of its range"),
    ("class", "nested classes or a class set operator"),
];

/// The admission verdict for `body`: `Ok`, or the limit of the parser's
/// first refusal.
fn verdict(body: &str) -> Result<(), &'static str> {
    match parse(&query(body)) {
        Ok(_) => Ok(()),
        Err(errors) => Err(errors.first().map_or("other", |e| limit_of(&e.message))),
    }
}

/// One admitted shape: a template at an alphabet and a count.
struct Shape {
    template: usize,
    alphabet: Alphabet,
    k: usize,
    body: String,
    gap: usize,
    /// For an edge knob, the limit that refuses the next count up.
    next: Option<&'static str>,
}

/// A template at an alphabet and a count the parser refuses.
struct Refusal {
    template: usize,
    alphabet: Alphabet,
    /// The count refused, or `None` when no count of an edge knob is
    /// admitted.
    k: Option<usize>,
    limit: &'static str,
}

struct Search {
    templates: Vec<Template>,
    shapes: Vec<Shape>,
    refused: Vec<Refusal>,
    /// Bodies parsed to find the edges.
    parsed: usize,
}

/// The largest count of an edge template the parser admits, and the limit
/// that refuses the next; or, when none is admitted, the limit that
/// refuses a count of one. Admission is monotone in the count: positions,
/// bytes, translation units and compiled sizes all grow with it.
fn edge(
    t: &Template,
    x: Alphabet,
    parsed: &mut usize,
) -> Result<(usize, &'static str), &'static str> {
    let (mut admitted, mut refused, mut limit) = (0, MAX_K + 1, "end");
    while refused - admitted > 1 {
        let k = admitted + (refused - admitted) / 2;
        *parsed += 1;
        match verdict(&(t.body)(x, k)) {
            Ok(()) => admitted = k,
            Err(why) => {
                refused = k;
                limit = why;
            }
        }
    }
    if admitted == 0 {
        Err(limit)
    } else {
        Ok((admitted, limit))
    }
}

/// Find every template's admitted shapes over every alphabet.
fn search() -> Search {
    let templates = templates();
    let (mut shapes, mut refused, mut parsed) = (Vec::new(), Vec::new(), 0);
    for (i, t) in templates.iter().enumerate() {
        for x in ALPHABETS {
            let mut admitted: Vec<(usize, Option<&'static str>)> = Vec::new();
            match t.knob {
                Knob::Edge => match edge(t, x, &mut parsed) {
                    Ok((k, next)) => admitted.push((k, Some(next))),
                    Err(limit) => refused.push(Refusal {
                        template: i,
                        alphabet: x,
                        k: None,
                        limit,
                    }),
                },
                Knob::At(ks) => {
                    for &k in ks {
                        parsed += 1;
                        match verdict(&(t.body)(x, k)) {
                            Ok(()) => admitted.push((k, None)),
                            Err(limit) => refused.push(Refusal {
                                template: i,
                                alphabet: x,
                                k: Some(k),
                                limit,
                            }),
                        }
                    }
                }
            }
            for (k, next) in admitted {
                shapes.push(Shape {
                    template: i,
                    alphabet: x,
                    k,
                    body: (t.body)(x, k),
                    gap: (t.gap)(k),
                    next,
                });
            }
        }
    }
    Search {
        templates,
        shapes,
        refused,
        parsed,
    }
}

/// What was tried, and the limit each admitted edge shape sits at.
fn report_search(s: &Search, out: &mut String) {
    let composed = s.templates.iter().filter(|t| !t.atom.is_empty()).count();
    let named = s.templates.len() - composed;
    let _ = writeln!(
        out,
        "shapes tried: {} ({composed} composed templates and {named} named cases, each over {} alphabets: {}); {} bodies parsed to find their edges",
        s.templates.len() * ALPHABETS.len(),
        ALPHABETS.len(),
        ALPHABETS.map(|x| x.name).join(", "),
        s.parsed
    );
    let _ = writeln!(
        out,
        "admitted shapes: {}; refused: {} (an edge template with no admitted k, or a reported case at its count)",
        s.shapes.len(),
        s.refused.len()
    );
    let mut line = String::from("the limit that refuses each admitted edge shape's next k:");
    for (word, _) in LIMIT_LEGEND {
        let n = s.shapes.iter().filter(|sh| sh.next == Some(word)).count();
        let _ = write!(line, " {word} {n},");
    }
    line.pop();
    let _ = writeln!(out, "{line}");
    let mut line = String::from("refusals by limit:");
    for (word, _) in LIMIT_LEGEND {
        let n = s.refused.iter().filter(|r| r.limit == word).count();
        if n > 0 {
            let _ = write!(line, " {word} {n},");
        }
    }
    line.pop();
    let _ = writeln!(out, "{line}");
    let _ = writeln!(out, "limits:");
    for (word, meaning) in LIMIT_LEGEND {
        let _ = writeln!(out, "  {word:<6} {meaning}");
    }
}

/// A shape's screen result in one engine: the call's ms, or `None` when
/// the engine could not run it.
type Screened = Option<f64>;

/// The screen table: one row per template and count, one cell per
/// alphabet, each the count, the screen's ms and the limit at the next
/// count (`at` for a reported count).
fn report_screen(s: &Search, screen: &[Screened], engine: &str, out: &mut String) {
    let _ = writeln!(
        out,
        "\n## the screen: one {engine} call per admitted shape, a 0.{SCREEN_SHARE} (k, ms, the limit at k + 1)\n"
    );
    let mut header = format!("{:<52} {:<12}", "shape", "X");
    for x in ALPHABETS {
        let _ = write!(header, " | {:<20}", x.name);
    }
    let _ = writeln!(out, "{}", header.trim_end());
    for (i, t) in s.templates.iter().enumerate() {
        let counts: Vec<Option<usize>> = match t.knob {
            Knob::Edge => vec![None],
            Knob::At(ks) => ks.iter().copied().map(Some).collect(),
        };
        for at in counts {
            let name = at.map_or_else(|| t.name.clone(), |k| format!("{} @{k}", t.name));
            let mut row = format!("{name:<52} {:<12}", t.atom);
            for x in ALPHABETS {
                let mine =
                    |template: usize, alphabet: Alphabet| template == i && alphabet.name == x.name;
                let found = s
                    .shapes
                    .iter()
                    .position(|sh| mine(sh.template, sh.alphabet) && at.is_none_or(|k| k == sh.k));
                let cell = if let Some(n) = found {
                    let sh = &s.shapes[n];
                    let ms = screen[n].map_or_else(|| "err".to_string(), |ms| format!("{ms:.1}"));
                    format!("{:>2} {:>8} {}", sh.k, ms, sh.next.unwrap_or("at"))
                } else {
                    let limit = s
                        .refused
                        .iter()
                        .find(|r| mine(r.template, r.alphabet) && (at.is_none() || r.k == at))
                        .map_or("?", |r| r.limit);
                    format!("refused: {limit}")
                };
                let _ = write!(row, " | {cell:<20}");
            }
            let _ = writeln!(out, "{}", row.trim_end());
        }
    }
}

/// Median and maximum of `samples`, in milliseconds.
fn summary(samples: &mut [Duration]) -> (f64, f64) {
    samples.sort();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    (
        ms(samples[samples.len() / 2]),
        ms(samples[samples.len() - 1]),
    )
}

/// Time `call` `runs` times after one warm-up, checking each answer is
/// `false`.
fn time_runs(runs: usize, mut call: impl FnMut() -> bool) -> (f64, f64) {
    assert!(!call(), "the subject must not match");
    let mut samples = Vec::with_capacity(runs);
    for _ in 0..runs {
        let start = Instant::now();
        let matched = call();
        samples.push(start.elapsed());
        assert!(!matched, "the subject must not match");
    }
    summary(&mut samples)
}

/// One call of `call`, in ms, checking the answer is `false`.
fn time_once(call: impl FnOnce() -> bool) -> f64 {
    let start = Instant::now();
    let matched = call();
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    assert!(!matched, "the subject must not match");
    ms
}

/// A fresh ledger: each measurement builds what it times, and nothing
/// a previous case built.
fn ledger() -> Preparer {
    Preparer::new()
}

/// `body` built as the live lane builds it.
fn build(body: &str) -> Pattern {
    ledger()
        .build(PatternKind::Regex, body)
        .expect("the parser admitted it, so it builds")
}

/// Whether `body` holds a capture group, so that the live `extract`'s
/// `captures` call does capture work.
fn has_group(body: &str) -> bool {
    let bytes = body.as_bytes();
    bytes.iter().enumerate().any(|(i, &b)| {
        b == b'('
            && (i == 0 || bytes[i - 1] != b'\\')
            && (bytes.get(i + 1) != Some(&b'?') || body[i..].starts_with("(?P<"))
    })
}

/// The host's load average, for the record beside a timing.
fn load() -> String {
    std::fs::read_to_string("/proc/loadavg").map_or_else(
        |_| "unknown".to_string(),
        |s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" "),
    )
}

/// A `DuckDB` connection and the subject it holds, so a run of shapes over
/// the same subject stores it once.
struct Re2 {
    conn: Connection,
    holds: Option<(&'static str, usize, u64)>,
}

impl Re2 {
    fn new() -> Self {
        let conn = Connection::open_in_memory().expect("in-memory duckdb");
        conn.execute("CREATE TABLE subject (message VARCHAR)", [])
            .expect("create the subject table");
        Self { conn, holds: None }
    }

    /// Store `x`'s subject for `gap` and `a_tenths`, unless it is held.
    fn hold(&mut self, x: Alphabet, gap: usize, a_tenths: u64) {
        let key = (x.name, gap, a_tenths);
        if self.holds == Some(key) {
            return;
        }
        self.conn
            .execute("DELETE FROM subject", [])
            .expect("clear the subject table");
        self.conn
            .execute(
                "INSERT INTO subject VALUES (?)",
                params![subject(x, gap, a_tenths)],
            )
            .expect("store the subject");
        self.holds = Some(key);
    }

    /// One `regexp_matches` call over the held subject. The pattern is a
    /// bound parameter, as the batch lane binds it, so RE2 compiles it once
    /// per statement.
    fn call(&self, body: &str) -> Result<bool, duckdb::Error> {
        self.conn
            .prepare("SELECT regexp_matches(message, ?) FROM subject")?
            .query_row(params![body], |row| row.get::<_, bool>(0))
    }

    /// One call, in ms, or `None` when `DuckDB` refuses the body.
    fn once(&mut self, x: Alphabet, gap: usize, a_tenths: u64, body: &str) -> Screened {
        self.hold(x, gap, a_tenths);
        let start = Instant::now();
        let matched = self.call(body).ok()?;
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        assert!(!matched, "the subject must not match");
        Some(ms)
    }

    /// `runs` calls after one warm-up: median and max ms.
    fn runs(
        &mut self,
        x: Alphabet,
        gap: usize,
        a_tenths: u64,
        body: &str,
        runs: usize,
    ) -> (f64, f64) {
        self.hold(x, gap, a_tenths);
        time_runs(runs, || self.call(body).expect("RE2 runs the body"))
    }
}

/// The shapes the screen ranks slowest, at most `top`, slowest first.
fn slowest(screen: &[Screened], top: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..screen.len()).filter(|&n| screen[n].is_some()).collect();
    order.sort_by(|&l, &r| {
        screen[r]
            .unwrap_or(0.0)
            .total_cmp(&screen[l].unwrap_or(0.0))
    });
    order.truncate(top);
    order
}

/// A row of a full timing.
struct Timed {
    shape: usize,
    a_tenths: u64,
    /// Median and max ms of the engine's call (`is_match` in Rust).
    call: (f64, f64),
    /// Rust's `captures`, for a shape that holds a group.
    captures: Option<(f64, f64)>,
}

fn shape_label(s: &Search, sh: &Shape) -> String {
    let t = &s.templates[sh.template];
    if t.atom.is_empty() {
        format!("{} / {} / k = {}", t.name, sh.alphabet.name, sh.k)
    } else {
        format!(
            "{}, X = {} / {} / k = {}",
            t.name, t.atom, sh.alphabet.name, sh.k
        )
    }
}

/// The two timings of one shape at one share: the engine's call, and
/// Rust's `captures` where it applies.
type Timing = ((f64, f64), Option<(f64, f64)>);

/// Time each of the screen's slowest shapes in full at both shares.
fn full_timing(
    s: &Search,
    top: &[usize],
    mut time: impl FnMut(&Shape, u64) -> Timing,
    out: &mut String,
) -> Vec<Timed> {
    let _ = writeln!(
        out,
        "{:<84} {:>3} {:>9} {:>9} {:>9} {:>9}  load",
        "shape", "a", "med", "max", "capt med", "capt max"
    );
    let mut rows = Vec::new();
    for &n in top {
        for a_tenths in A_SHARES {
            let (call, captures) = time(&s.shapes[n], a_tenths);
            let (cm, cx) = captures.map_or_else(
                || ("-".to_string(), "-".to_string()),
                |(m, x)| (format!("{m:.2}"), format!("{x:.2}")),
            );
            let _ = writeln!(
                out,
                "{:<84} 0.{a_tenths} {:>9.2} {:>9.2} {cm:>9} {cx:>9}  {}",
                shape_label(s, &s.shapes[n]),
                call.0,
                call.1,
                load()
            );
            rows.push(Timed {
                shape: n,
                a_tenths,
                call,
                captures,
            });
        }
    }
    rows
}

/// The slowest timed row whose shape is an edge template, by `key`: the
/// shape [`sweep`] times at smaller counts.
fn slowest_edge<'r>(
    s: &Search,
    rows: &'r [Timed],
    key: impl Fn(&Timed) -> f64,
) -> Option<&'r Timed> {
    rows.iter()
        .filter(|r| matches!(s.templates[s.shapes[r.shape].template].knob, Knob::Edge))
        .max_by(|l, r| key(l).total_cmp(&key(r)))
}

/// How the slowest edge shape scales with its count: at a quarter, a half
/// and three quarters of its edge count, on the share it was slowest
/// over.
fn sweep(
    s: &Search,
    worst: Option<&Timed>,
    mut time: impl FnMut(Alphabet, usize, u64, &str) -> (f64, f64),
    out: &mut String,
) {
    let Some(worst) = worst else {
        return;
    };
    let sh = &s.shapes[worst.shape];
    let t = &s.templates[sh.template];
    let _ = writeln!(
        out,
        "\nthe slowest edge shape, {}, a 0.{}, at smaller k (median / max ms of 3 calls):",
        shape_label(s, sh),
        worst.a_tenths
    );
    let mut ks: Vec<usize> = [sh.k / 4, sh.k / 2, 3 * sh.k / 4]
        .into_iter()
        .filter(|&k| k >= 1)
        .collect();
    ks.dedup();
    for k in ks {
        let body = (t.body)(sh.alphabet, k);
        assert!(verdict(&body).is_ok(), "a smaller k is admitted");
        let (med, max) = time(sh.alphabet, (t.gap)(k), worst.a_tenths, &body);
        let _ = writeln!(
            out,
            "  k = {k:>3}: {med:>8.2} / {max:>8.2}  load {}",
            load()
        );
    }
    let _ = writeln!(
        out,
        "  k = {:>3}: {:>8.2} / {:>8.2}  (the full timing above)",
        sh.k, worst.call.0, worst.call.1
    );
}

/// Screen every admitted shape in Rust: `is_match`, and `captures` for a
/// shape that holds a group. The slower of the two is the shape's time.
fn rust_screen(s: &Search) -> Vec<Screened> {
    s.shapes
        .iter()
        .map(|sh| {
            let pattern = build(&sh.body);
            let text = subject(sh.alphabet, sh.gap, SCREEN_SHARE);
            let matched = time_once(|| pattern.is_match(&text).expect("never cancelled"));
            let captured = if has_group(&sh.body) {
                time_once(|| pattern.captures(&text).expect("never cancelled").is_some())
            } else {
                0.0
            };
            Some(matched.max(captured))
        })
        .collect()
}

/// Rust's full timing of one shape.
fn rust_full(sh: &Shape, a_tenths: u64) -> Timing {
    let pattern = build(&sh.body);
    let text = subject(sh.alphabet, sh.gap, a_tenths);
    let matched = time_runs(RUNS, || pattern.is_match(&text).expect("never cancelled"));
    let captured = has_group(&sh.body).then(|| {
        time_runs(RUNS, || {
            pattern.captures(&text).expect("never cancelled").is_some()
        })
    });
    (matched, captured)
}

fn header(part: &str, out: &mut String) {
    let _ = writeln!(
        out,
        "\npart {part}: subject {SUBJECT_CHARS} characters; load average at start (1, 5 and 15 minutes): {}",
        load()
    );
}

fn footer(part: &str, started: Instant, out: &mut String) {
    let _ = writeln!(
        out,
        "\npart {part} took {:.0} s; load average at end (1, 5 and 15 minutes): {}",
        started.elapsed().as_secs_f64(),
        load()
    );
}

/// The 1-minute load average, the first of [`load`]'s three.
fn load_1m() -> String {
    load()
        .split_whitespace()
        .next()
        .unwrap_or("unknown")
        .to_string()
}

/// The Rust gate: the search, timed in Rust's live lane, against the hard
/// ceiling. It fails when any call, in the screen or in the full timing,
/// takes the ceiling or more.
#[test]
#[ignore = "wall-clock evidence for a reader; run through scripts/evidence/issue-334/cost.sh"]
fn rust_gate() {
    let started = Instant::now();
    let load_start = load_1m();
    let mut out = String::new();
    header("rust_gate", &mut out);
    let ceiling_ms = CEILING.as_secs_f64() * 1000.0;
    let _ = writeln!(
        out,
        "the gate: every Rust call of every admitted shape, is_match and captures alike, stays under {ceiling_ms:.0} ms\n"
    );
    let s = search();
    report_search(&s, &mut out);
    let screen = rust_screen(&s);
    report_screen(&s, &screen, "Rust", &mut out);

    let top = slowest(&screen, TOP);
    let _ = writeln!(
        out,
        "\n## the Rust gate: the {} slowest shapes of the screen, {RUNS} calls each after a warm-up (ms per call)\n",
        top.len()
    );
    let rows = full_timing(&s, &top, rust_full, &mut out);
    let slowest_call = |r: &Timed| r.call.1.max(r.captures.map_or(0.0, |c| c.1));
    let worst = rows
        .iter()
        .max_by(|l, r| slowest_call(l).total_cmp(&slowest_call(r)))
        .expect("some shape is admitted");
    sweep(
        &s,
        slowest_edge(&s, &rows, slowest_call),
        |x, gap, a, body| {
            let pattern = build(body);
            let text = subject(x, gap, a);
            time_runs(3, || pattern.is_match(&text).expect("never cancelled"))
        },
        &mut out,
    );

    let screen_max = screen.iter().flatten().copied().fold(0.0, f64::max);
    let full_max = rows.iter().map(slowest_call).fold(0.0, f64::max);
    let full_median = rows
        .iter()
        .map(|r| r.call.0.max(r.captures.map_or(0.0, |c| c.0)))
        .fold(0.0, f64::max);
    let sh = &s.shapes[worst.shape];
    let _ = writeln!(
        out,
        "\nworst Rust shape: {}, a 0.{}: median {:.2} ms, max {:.2} ms per call{}",
        shape_label(&s, sh),
        worst.a_tenths,
        worst.call.0,
        worst.call.1,
        worst.captures.map_or_else(String::new, |c| format!(
            "; captures median {:.2} ms, max {:.2} ms",
            c.0, c.1
        ))
    );
    let _ = writeln!(
        out,
        "  body ({} bytes): {}",
        sh.body.len(),
        printable(&sh.body)
    );
    let pass = screen_max < ceiling_ms && full_max < ceiling_ms;
    let _ = writeln!(
        out,
        "\nRUST GATE {}: the slowest screened call took {screen_max:.2} ms; in the full timing the slowest median was {full_median:.2} ms and the slowest call {full_max:.2} ms; the ceiling is {ceiling_ms:.0} ms; 1-minute load average {load_start} at the start and {} at the end",
        if pass { "PASS" } else { "FAIL" },
        load_1m()
    );
    footer("rust_gate", started, &mut out);
    print!("{out}");
    assert!(pass, "an admitted shape's Rust call reaches the ceiling");
}

/// The RE2 envelope: the search, timed in the batch lane's
/// `regexp_matches`. It reports the slowest call and asserts nothing about
/// time.
#[test]
#[ignore = "wall-clock evidence for a reader; run through scripts/evidence/issue-334/cost.sh"]
fn re2_envelope() {
    let started = Instant::now();
    let load_start = load_1m();
    let mut out = String::new();
    header("re2_envelope", &mut out);
    let _ = writeln!(
        out,
        "the envelope: the slowest RE2 call of any admitted shape, reported with no ceiling\n"
    );
    let s = search();
    report_search(&s, &mut out);
    let mut re2 = Re2::new();
    let screen: Vec<Screened> = s
        .shapes
        .iter()
        .map(|sh| re2.once(sh.alphabet, sh.gap, SCREEN_SHARE, &sh.body))
        .collect();
    report_screen(&s, &screen, "RE2", &mut out);
    let unrun: Vec<&Shape> = s
        .shapes
        .iter()
        .zip(&screen)
        .filter_map(|(sh, t)| t.is_none().then_some(sh))
        .collect();
    let _ = writeln!(out, "\nadmitted shapes RE2 refused to run: {}", unrun.len());
    for sh in unrun {
        let _ = writeln!(out, "  {}: {}", shape_label(&s, sh), printable(&sh.body));
    }

    let top = slowest(&screen, RE2_TOP);
    let _ = writeln!(
        out,
        "\n## the RE2 envelope: the {} slowest shapes of the screen, {RE2_RUNS} calls each after a warm-up (ms per call)\n",
        top.len()
    );
    let rows = full_timing(
        &s,
        &top,
        |sh, a| (re2.runs(sh.alphabet, sh.gap, a, &sh.body, RE2_RUNS), None),
        &mut out,
    );
    let worst = rows
        .iter()
        .max_by(|l, r| l.call.0.total_cmp(&r.call.0))
        .expect("some shape is admitted");
    sweep(
        &s,
        slowest_edge(&s, &rows, |r| r.call.0),
        |x, gap, a, body| re2.runs(x, gap, a, body, 3),
        &mut out,
    );
    let sh = &s.shapes[worst.shape];
    let _ = writeln!(
        out,
        "\nRE2 ENVELOPE (no ceiling): {}, a 0.{}: median {:.2} ms, max {:.2} ms per call; 1-minute load average {load_start} at the start and {} at the end",
        shape_label(&s, sh),
        worst.a_tenths,
        worst.call.0,
        worst.call.1,
        load_1m()
    );
    let _ = writeln!(
        out,
        "  body ({} bytes): {}",
        sh.body.len(),
        printable(&sh.body)
    );
    let _ = writeln!(
        out,
        "  DuckDB checks for an interrupt between {ROWS_PER_VECTOR:.0}-row vectors, so a timed-out batch query over such values can hold each of its threads about {ROWS_PER_VECTOR:.0} x {:.2} ms = {:.0} s past its deadline (docs/evidence/issue-334/cancel-overhang.txt)",
        worst.call.0,
        ROWS_PER_VECTOR * worst.call.0 / 1000.0
    );
    footer("re2_envelope", started, &mut out);
    print!("{out}");
}

/// Time a whole admission, parse to built patterns, of `q`.
fn time_parse(q: &str) -> (f64, f64) {
    parse(q).expect("the query is admitted");
    let mut samples = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let start = Instant::now();
        let parsed = parse(q);
        samples.push(start.elapsed());
        assert!(parsed.is_ok(), "the query is admitted");
    }
    summary(&mut samples)
}

/// The query of 256 distinct copies of `body` (`MAX_QUERY_PATTERNS`), each
/// told apart by a three-digit suffix, as one search filter. Distinct,
/// because a ledger builds each text once.
fn copies(body: &str) -> String {
    let list: Vec<String> = (0..256).map(|i| format!("/{body}{i:03}/")).collect();
    format!("message={}", list.join(","))
}

/// The body whose build compiles the most: a Rust program near 1 MiB
/// beside a view near 512 KiB. `\w` is a large class in Rust and an ASCII
/// one in the view, and `\pL` is the same large class in both, so
/// `\w{a}\pL{b}` takes, for each `b` the view admits, the most `\w` the
/// program admits, and the pair that compiles the most bytes wins. Each
/// body is measured with the suffix [`copies`] adds, and returned with its
/// program's and its view's compiled bytes.
fn most_compile_body() -> (String, usize, usize) {
    let mut measure = ledger();
    let mut best: Option<(String, usize, usize)> = None;
    for b in 1..=MAX_K {
        if verdict(&format!("\\pL{{{b}}}000")).is_err() {
            break;
        }
        let Some(a) = (1..=MAX_K)
            .take_while(|a| verdict(&format!("\\w{{{a}}}\\pL{{{b}}}000")).is_ok())
            .last()
        else {
            continue;
        };
        let body = format!("\\w{{{a}}}\\pL{{{b}}}");
        let suffixed = format!("{body}000");
        let rust = measure
            .compiled_bytes(PatternKind::Regex, &suffixed)
            .expect("measures");
        let view = measure.batch_compiled_bytes(&suffixed).expect("measures");
        if best.as_ref().is_none_or(|(_, r, v)| rust + view > r + v) {
            best = Some((body, rust, view));
        }
    }
    best.expect("some \\w{a}\\pL{b} is admitted")
}

/// What admitting the costliest queries takes: parse, price and build.
#[test]
#[ignore = "wall-clock evidence for a reader; run through scripts/evidence/issue-334/cost.sh"]
fn admission() {
    let started = Instant::now();
    let mut out = String::new();
    header("admission", &mut out);
    let _ = writeln!(
        out,
        "\n## admission of the costliest queries (parse, price and build; {RUNS} parses after one)\n"
    );
    // The calibration's slowest admitted translation (ADR-0063, CP2):
    // (?i) and 14 full-range brackets, 15 844 598 units, under the
    // 2^24-unit budget. One more bracket is refused.
    let brackets = "[\\x00-\\x{10FFFF}]".repeat(14);
    let worst_units = query(&format!("(?i){brackets}"));
    let refused = query(&format!("(?i){brackets}[\\x00-\\x{{10FFFF}}]"));
    let (median, max) = time_parse(&worst_units);
    let _ = writeln!(
        out,
        "most translation units, (?i) + 14 full-range brackets (15 844 598 units): median {median:.2} ms, max {max:.2} ms; load {}",
        load()
    );
    let _ = writeln!(
        out,
        "  one more bracket (16 976 355 units) is refused: {}",
        parse(&refused).is_err()
    );

    // The most compile work one query may hold: 256 distinct patterns,
    // each compiling a program near the 1 MiB limit and a view near the
    // batch engine's 512 KiB cap.
    let (body, rust, view) = most_compile_body();
    let (median, max) = time_parse(&copies(&body));
    let _ = writeln!(
        out,
        "most compile work, 256 distinct {body}NNN patterns, each {:.1} KiB of program and {:.1} KiB of view: median {median:.2} ms, max {max:.2} ms; load {}",
        kib(rust),
        kib(view),
        load()
    );
    footer("admission", started, &mut out);
    print!("{out}");
}

#[expect(clippy::cast_precision_loss, reason = "a KiB figure for a reader")]
fn kib(bytes: usize) -> f64 {
    bytes as f64 / 1024.0
}

/// Timed calls per engine for each row of [`re2_step`], after one warm-up:
/// a row's cost is either a few milliseconds or hundreds, so three tell
/// them apart.
const STEP_RUNS: usize = 3;

/// One family [`re2_step`] measures, at each of `ks`.
struct StepFamily {
    name: &'static str,
    body: fn(Alphabet, usize) -> String,
    ks: &'static [usize],
}

/// The families FIX6 calibrated RE2's step on, each at counts on both
/// sides of its step: large classes, which RE2 compiles to many
/// instructions, in the shapes that keep its DFA busiest.
const STEP_FAMILIES: [StepFamily; 14] = [
    StepFamily {
        name: "\\B (?:\\pL?){k} \\PL",
        body: |x, k| between_b_and_none(x, &format!("(?:\\pL?){{{k}}}")),
        ks: &[12, 14, 15, 23],
    },
    StepFamily {
        name: "\\B \\pL{k} \\PL",
        body: |x, k| between_b_and_none(x, &format!("\\pL{{{k}}}")),
        ks: &[14, 16],
    },
    StepFamily {
        name: "\\B (?:(?:\\pL|\\pN|\\pS)?){k} \\PL",
        body: |x, k| between_b_and_none(x, &format!("(?:(?:\\pL|\\pN|\\pS)?){{{k}}}")),
        ks: &[8, 10, 12],
    },
    StepFamily {
        name: "\\B (?:(?:\\pL\\pL)?){k} \\PL",
        body: |x, k| between_b_and_none(x, &format!("(?:(?:\\pL\\pL)?){{{k}}}")),
        ks: &[7, 8],
    },
    StepFamily {
        name: "\\B (?:\\pL{0,3}){k} \\PL",
        body: |x, k| between_b_and_none(x, &format!("(?:\\pL{{0,3}}){{{k}}}")),
        ks: &[4, 5],
    },
    StepFamily {
        name: "\\B (?:\\pL?){k} \\PL $",
        body: |_, k| format!("\\B(?:\\pL?){{{k}}}\\PL$"),
        ks: &[14, 15],
    },
    StepFamily {
        name: "F7 twin \\B(?:\\pL|(?:\\pL)?|b3){0,2}\\PL",
        body: |_, _| "\\B(?:\\pL|(?:\\pL)?|(?:(?:\\pL){0,2}){0,2}){0,2}\\PL".to_string(),
        ks: &[0],
    },
    StepFamily {
        name: "F7 core over \\pL",
        body: |x, _| between_b_and_none(x, &f7_core("\\pL")),
        ks: &[0],
    },
    StepFamily {
        name: "a \\pL{k} c",
        body: |x, k| format!("{}\\pL{{{k}}}{}", x.a, x.c),
        ks: &[8, 12, 16],
    },
    StepFamily {
        name: "\\B (?:\\PN?){k} \\PL",
        body: |x, k| between_b_and_none(x, &format!("(?:\\PN?){{{k}}}")),
        ks: &[31, 36, 40],
    },
    StepFamily {
        name: "\\B (?:\\p{Han}?){k} \\PL",
        body: |x, k| between_b_and_none(x, &format!("(?:\\p{{Han}}?){{{k}}}")),
        ks: &[200, 248],
    },
    StepFamily {
        name: "mix a (?:\\pL?){k} [abc]{60-2k} [^abc]",
        body: |x, k| {
            format!(
                "{}(?:\\pL?){{{k}}}{}{{{}}}{}",
                x.a,
                class3(x),
                60 - 2 * k,
                not3(x)
            )
        },
        ks: &[12, 14],
    },
    StepFamily {
        name: "mix \\B (?:\\pL?){k} (?:\\B[abc]){30-k} \\PL",
        body: |x, k| {
            between_b_and_none(
                x,
                &format!("(?:\\pL?){{{k}}}(?:\\B{}){{{}}}", class3(x), 30 - k),
            )
        },
        ks: &[12, 14],
    },
    StepFamily {
        name: "\\B (?:\\w?){k} \\PL",
        body: |x, k| between_b_and_none(x, &format!("(?:\\w?){{{k}}}")),
        ks: &[20, 31],
    },
];

/// An RE2 median past this, in ms, is the NFA's: the DFA that gives up
/// on a busy cache stays near 200 ms over these subjects.
const STEP_SLOW_MS: f64 = 250.0;

/// An RE2 median under this, in ms, is the DFA's.
const STEP_FAST_MS: f64 = 50.0;

/// RE2's step against the batch-engine view (ADR-0063). RE2's DFA gives up
/// at once on a program past its init budget, and then every call runs
/// the NFA, which costs hundreds of milliseconds where the DFA cost a few.
/// Each row gives what the body compiles to in Rust, and in its view as
/// the batch engine reads it (`trawl_core::pattern::Preparer::batch_compiled_bytes`),
/// with both engines' times. A row the ledger refuses is timed in RE2
/// alone.
#[test]
#[ignore = "wall-clock evidence for a reader; run through scripts/evidence/issue-334/cost.sh"]
fn re2_step() {
    let started = Instant::now();
    let mut out = String::new();
    header("re2_step", &mut out);
    let _ = writeln!(
        out,
        "\n## RE2's step against the batch-engine view (a 0.8; median / max ms of {STEP_RUNS} calls)\n"
    );
    let _ = writeln!(
        out,
        "{:<44} {:<8} {:>4} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}  verdict",
        "family",
        "alphabet",
        "k",
        "rust KiB",
        "view KiB",
        "rust med",
        "rust max",
        "re2 med",
        "re2 max"
    );
    let mut re2 = Re2::new();
    // Per family: the largest view RE2 ran fast, and the smallest it ran
    // slow, on any alphabet.
    let mut steps: Vec<(&str, Option<f64>, Option<f64>)> = Vec::new();
    for family in &STEP_FAMILIES {
        let (mut fast, mut slow): (Option<f64>, Option<f64>) = (None, None);
        for &k in family.ks {
            for x in STEP_ALPHABETS {
                let body = (family.body)(x, k);
                let mut measure = ledger();
                let rust_bytes = measure
                    .compiled_bytes(PatternKind::Regex, &body)
                    .expect("the body compiles under the measuring limit");
                let view_bytes = measure
                    .batch_compiled_bytes(&body)
                    .expect("the view compiles under the measuring limit");
                let text = subject(x, k + 1, 8);
                let rust = ledger()
                    .build(PatternKind::Regex, &body)
                    .ok()
                    .map(|pattern| {
                        time_runs(STEP_RUNS, || {
                            pattern.is_match(&text).expect("never cancelled")
                        })
                    });
                let re2_ms = re2.runs(x, k + 1, 8, &body, STEP_RUNS);
                let (rust_med, rust_max) = rust.map_or_else(
                    || ("-".to_string(), "-".to_string()),
                    |(med, max)| (format!("{med:.2}"), format!("{max:.2}")),
                );
                let _ = writeln!(
                    out,
                    "{:<44} {:<8} {k:>4} {:>9.1} {:>9.1} {rust_med:>9} {rust_max:>9} {:>9.2} {:>9.2}  {}",
                    family.name,
                    x.name,
                    kib(rust_bytes),
                    kib(view_bytes),
                    re2_ms.0,
                    re2_ms.1,
                    verdict(&body).err().unwrap_or("admitted")
                );
                let view = kib(view_bytes);
                if re2_ms.0 > STEP_SLOW_MS {
                    slow = Some(slow.map_or(view, |s: f64| s.min(view)));
                } else if re2_ms.0 < STEP_FAST_MS {
                    fast = Some(fast.map_or(view, |f: f64| f.max(view)));
                }
            }
        }
        steps.push((family.name, fast, slow));
    }
    let _ = writeln!(
        out,
        "\nper family, the largest view RE2 ran under {STEP_FAST_MS:.0} ms and the smallest it ran over {STEP_SLOW_MS:.0} ms (KiB):"
    );
    let show = |kib: Option<f64>| kib.map_or_else(|| "-".to_string(), |k| format!("{k:.1}"));
    for (name, fast, slow) in &steps {
        let _ = writeln!(out, "  {name:<44} {:>9} {:>9}", show(*fast), show(*slow));
    }
    if let Some(lowest) = steps
        .iter()
        .filter_map(|(_, _, slow)| *slow)
        .min_by(f64::total_cmp)
    {
        let _ = writeln!(
            out,
            "the lowest view at which any family ran over {STEP_SLOW_MS:.0} ms: {lowest:.1} KiB"
        );
    }
    footer("re2_step", started, &mut out);
    print!("{out}");
}
