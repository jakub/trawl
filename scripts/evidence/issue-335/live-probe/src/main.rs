// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live-tail matching throughput over trawl-core's pattern corpus (issue
//! #335, ADR-0063), through the stream API a live stream runs.
//!
//! The probe builds nothing itself. Each pattern goes through
//! `trawl_core::parser::parse`, `CompiledFilter::compile` and
//! `compile_stream_plan`, the calls a stream's setup makes, and each line
//! through `CompiledFilter::matches_at` and `stream::accept_event`. So the
//! same source, built against two trees, times whatever backing each tree
//! ships: this branch's owner, the meta engine at each pattern's own
//! capacity with one cache, and main's, the regex crate at its 2 MiB
//! default with its pool. `../throughput.sh` builds it both ways.
//!
//! - `is_match`: the search stage `message=/P/`, one `matches_at` a line.
//! - `captures`: `* | extract "(?P<m>P)"`, one `accept_event` a line, which
//!   converts the event to a row and runs the extract stage's captures. A
//!   `"` in `P` is written `\x22`, because an extract string decodes
//!   nothing. A line counts when the group matched. The `control` row
//!   times `*` alone, the conversion without a regex, so the extract's own
//!   share is the difference.
//!
//! Modes:
//!
//! - `corpus` (feature `corpus`): print the corpus, one JSON object a
//!   line.
//! - `measure LABEL CORPUS RUNS`: time every cell `RUNS` times after one
//!   warm-up pass and print one TSV row a pass.
//! - `report TSV...`: the medians, per backing, with the ratio and a check
//!   that both backings matched the same lines.

use std::collections::BTreeMap;
use std::hint::black_box;
use std::io::{BufRead, BufReader};
use std::process::ExitCode;
use std::time::Instant;

use serde_json::{Map, Value};
use trawl_core::context::EvalContext;
use trawl_core::eval::EvalValue;
use trawl_core::filter::CompiledFilter;
use trawl_core::pin_scope::PinScope;
use trawl_core::schema::FieldTypes;
use trawl_core::stream::{CompiledStage, LiveOutcome, StreamPlan};

/// Lines in each line set.
const LINES: usize = 50_000;

/// The line generator's seed, so every pass times the same lines.
const SEED: u64 = 0x0335_7e57;

// --- Lines -------------------------------------------------------------------

/// `SplitMix64`, so the lines need no dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).expect("n fits")).expect("below n")
    }

    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len())]
    }
}

/// The words one line set draws from.
struct Words {
    users: &'static [&'static str],
    hosts: &'static [&'static str],
    messages: &'static [&'static str],
    paths: &'static [&'static str],
    domains: &'static [&'static str],
    agents: &'static [&'static str],
}

const ASCII: Words = Words {
    users: &[
        "alice",
        "bob",
        "carol",
        "dave",
        "eve",
        "mallory",
        "svc-backup",
        "root",
        "admin",
        "deploy",
    ],
    hosts: &["web-01", "db-02", "cache-3", "edge-a", "nas", "build-7"],
    messages: &[
        "connection refused",
        "request completed",
        "timeout waiting for upstream",
        "authentication failed for user",
        "disk usage at 91 percent",
        "retrying in 5s",
        "cache miss on key",
        "user logged out",
        "exception in worker thread",
        "permission denied",
        "health check ok",
        "scheduled job finished",
    ],
    paths: &[
        "/api/v1/items",
        "/login",
        "/static/app.js",
        "/healthz",
        "/api/v1/query?id=42",
        "/index.html",
    ],
    domains: &[
        "example.com",
        "mail.example.org",
        "corp.local",
        "lab.internal",
    ],
    agents: &[
        "curl/8.10.1",
        "Mozilla/5.0 (X11; Linux x86_64)",
        "Go-http-client/1.1",
    ],
};

const UNICODE: Words = Words {
    users: &[
        "jürgen",
        "Đorđe",
        "李雷",
        "Ольга",
        "Σοφία",
        "José",
        "مریم",
        "yūki",
        "Ægir",
        "𐐷𐐯𐑅",
    ],
    hosts: &["сервер-01", "主机-2", "höst-3", "edge-α", "nås", "構築-7"],
    messages: &[
        "接続が拒否されました",
        "запрос выполнен",
        "délai d'attente dépassé",
        "αποτυχία ταυτοποίησης χρήστη",
        "磁盘使用率 91%",
        "повтор через 5 с",
        "🔥 cache miss 🔥",
        "Benutzer abgemeldet",
        "例外: ワーカースレッド",
        "𐐀𐐁𐐂 permission refusée",
        "검사 정상",
        "επιτυχής εργασία ✓",
    ],
    paths: &[
        "/api/v1/élément",
        "/вход",
        "/静的/app.js",
        "/healthz",
        "/api/v1/query?q=naïve",
        "/índice.html",
    ],
    domains: &["例え.jp", "почта.рф", "bücher.de", "example.com"],
    agents: &[
        "curl/8.10.1",
        "Mozilla/5.0 (X11; Linux x86_64)",
        "クライアント/1.1",
    ],
};

const LEVELS: &[&str] = &["debug", "info", "warn", "error"];
const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE"];

fn ip(r: &mut Rng) -> String {
    format!(
        "{}.{}.{}.{}",
        r.below(256),
        r.below(256),
        r.below(256),
        r.below(256)
    )
}

fn hex(r: &mut Rng, digits: usize) -> String {
    (0..digits)
        .map(|_| char::from_digit(u32::try_from(r.below(16)).expect("a digit"), 16).expect("hex"))
        .collect()
}

fn uuid(r: &mut Rng) -> String {
    format!(
        "{}-{}-{}-{}-{}",
        hex(r, 8),
        hex(r, 4),
        hex(r, 4),
        hex(r, 4),
        hex(r, 12)
    )
}

fn mac(r: &mut Rng) -> String {
    (0..6).map(|_| hex(r, 2)).collect::<Vec<_>>().join(":")
}

fn clock(r: &mut Rng) -> String {
    format!("{:02}:{:02}:{:02}", r.below(24), r.below(60), r.below(60))
}

/// One log line in one of five shapes: syslog, an access log, logfmt,
/// JSON, and prose.
fn line(r: &mut Rng, w: &Words) -> String {
    let user = r.pick(w.users);
    let host = r.pick(w.hosts);
    let msg = r.pick(w.messages);
    match r.below(5) {
        0 => format!(
            "<{}>Nov  4 {} {host} sshd[{}]: {msg} user={user} from {} port {}",
            r.below(192),
            clock(r),
            r.below(65536),
            ip(r),
            r.below(65536)
        ),
        // The corpus's access-log parser reads the version as `HTTP\d\.\d`,
        // with no slash, so half the access lines write it that way and the
        // parser's captures are timed on lines it matches.
        1 => format!(
            "{} - {user} [04/Nov/2026:{} +0000] \"{} {} {}\" {} {} \"-\" \"{}\"",
            ip(r),
            clock(r),
            r.pick(METHODS),
            r.pick(w.paths),
            ["HTTP/1.1", "HTTP1.1"][r.below(2)],
            [200, 201, 301, 404, 500, 503][r.below(6)],
            r.below(100_000),
            r.pick(w.agents)
        ),
        2 => format!(
            "ts=2026-11-04T{}.{:03}Z level={} msg=\"{msg}\" user={user}@{} req={} dur={}ms",
            clock(r),
            r.below(1000),
            r.pick(LEVELS),
            r.pick(w.domains),
            uuid(r),
            r.below(5000)
        ),
        3 => format!(
            "{{\"ts\":\"2026-11-04T{}+01:00\",\"level\":\"{}\",\"msg\":\"{msg}\",\"peer\":\"{}:{}\",\"mac\":\"{}\",\"host\":\"{host}\"}}",
            clock(r),
            r.pick(LEVELS),
            ip(r),
            r.below(65536),
            mac(r)
        ),
        _ => format!(
            "{host} worker {}: {msg}; {} after {} attempts ({user})",
            r.below(64),
            r.pick(w.messages),
            r.below(10)
        ),
    }
}

/// A line set as live events: one `message` field each, as the bus
/// carries an event's JSON.
fn events(w: &Words) -> Vec<Map<String, Value>> {
    let mut r = Rng(SEED);
    (0..LINES)
        .map(|_| {
            let mut event = Map::new();
            event.insert("message".to_owned(), Value::String(line(&mut r, w)));
            event
        })
        .collect()
}

// --- Cells -------------------------------------------------------------------

/// What one stream setup prepares for a query: its filter and its stages.
struct Prepared {
    filter: CompiledFilter,
    stages: Vec<CompiledStage>,
}

/// The query's stream setup, as trawld's: parse, then compile the filter
/// and the plan against an empty pin snapshot. `Err` names the step that
/// refused it.
fn prepare(dsl: &str) -> Result<Prepared, String> {
    let ast = trawl_core::parser::parse(dsl).map_err(|_| "parse".to_owned())?;
    let pins = FieldTypes::default();
    let filter = CompiledFilter::compile(&ast.search, &pins).map_err(|e| format!("filter: {e}"))?;
    let plan = trawl_core::stream::compile_stream_plan(&ast.pipeline, &PinScope::root(&pins), None)
        .map_err(|e| format!("plan: {e}"))?;
    match plan {
        StreamPlan::PassThrough(stages) => Ok(Prepared { filter, stages }),
        StreamPlan::Aggregate { .. } => Err("an aggregate plan".to_owned()),
    }
}

#[derive(Clone, Copy)]
enum Op {
    IsMatch,
    Captures,
    Control,
}

impl Op {
    const fn name(self) -> &'static str {
        match self {
            Self::IsMatch => "is_match",
            Self::Captures => "captures",
            Self::Control => "control",
        }
    }
}

/// One pass over `events`: ns a line, and the lines it counts: matched
/// for `is_match`, with the extract's group set for `captures`, emitted
/// for `control`.
fn pass(p: &mut Prepared, op: Op, events: &[Map<String, Value>]) -> (f64, usize) {
    let ctx = EvalContext::capture();
    let mut hits = 0;
    let started = Instant::now();
    for event in events {
        let hit = match op {
            Op::IsMatch => p.filter.matches_at(black_box(event), &ctx),
            Op::Captures | Op::Control => {
                match trawl_core::stream::accept_event(
                    &p.filter,
                    &mut p.stages,
                    black_box(event),
                    &ctx,
                )
                .expect("every stage's pattern is prepared")
                {
                    LiveOutcome::Emit(row) => {
                        // An extract emits every line; the line counts when
                        // its group matched.
                        let hit = !matches!(op, Op::Captures)
                            || row.get("m").is_some_and(|v| !matches!(v, EvalValue::Null));
                        black_box(&row);
                        hit
                    }
                    LiveOutcome::Filtered | LiveOutcome::Done => false,
                }
            }
        };
        hits += usize::from(hit);
    }
    let elapsed = started.elapsed();
    #[expect(
        clippy::cast_precision_loss,
        reason = "a pass's nanoseconds and line count fit an f64's mantissa"
    )]
    let ns = elapsed.as_nanos() as f64 / events.len() as f64;
    (ns, hits)
}

/// The extract stage's text for a pattern: one named group around it,
/// with each `"` written as the regex escape `\x22`.
fn extract_dsl(pattern: &str) -> String {
    format!("* | extract \"(?P<m>{})\"", pattern.replace('"', r"\x22"))
}

// --- Modes -------------------------------------------------------------------

#[cfg(feature = "corpus")]
fn print_corpus() -> ExitCode {
    use trawl_core::pattern::charge::{LOG_REGEXES, UNICODE_REGEXES};
    for (corpus, patterns) in [("log", LOG_REGEXES), ("unicode", UNICODE_REGEXES)] {
        for pattern in patterns {
            println!(
                "{}",
                serde_json::json!({ "corpus": corpus, "pattern": pattern })
            );
        }
    }
    ExitCode::SUCCESS
}

#[cfg(not(feature = "corpus"))]
fn print_corpus() -> ExitCode {
    eprintln!("live-probe: this build has no corpus; build it with --features corpus");
    ExitCode::FAILURE
}

fn read_corpus(path: &str) -> Vec<(String, String)> {
    let file = std::fs::File::open(path).expect("the corpus file opens");
    BufReader::new(file)
        .lines()
        .map(|line| {
            let line = line.expect("a corpus line");
            let v: Value = serde_json::from_str(&line).expect("a corpus line is JSON");
            (
                v["corpus"].as_str().expect("corpus").to_owned(),
                v["pattern"].as_str().expect("pattern").to_owned(),
            )
        })
        .collect()
}

fn measure(label: &str, corpus_path: &str, runs: usize) -> ExitCode {
    let corpus = read_corpus(corpus_path);
    let sets = [("ascii", events(&ASCII)), ("unicode", events(&UNICODE))];
    let mut cells: Vec<(String, String, Op, String)> = vec![(
        "control".to_owned(),
        "*".to_owned(),
        Op::Control,
        "*".to_owned(),
    )];
    for (name, pattern) in &corpus {
        cells.push((
            name.clone(),
            pattern.clone(),
            Op::IsMatch,
            format!("message=/{pattern}/"),
        ));
        cells.push((
            name.clone(),
            pattern.clone(),
            Op::Captures,
            extract_dsl(pattern),
        ));
    }
    for (corpus_name, pattern, op, dsl) in &cells {
        let mut prepared = match prepare(dsl) {
            Ok(p) => p,
            Err(why) => {
                for (set, _) in &sets {
                    println!(
                        "{label}\t{corpus_name}\t{}\t{set}\t{}\t-\trefused\t{why}",
                        escape(pattern),
                        op.name()
                    );
                }
                continue;
            }
        };
        for (set, events) in &sets {
            let _ = pass(&mut prepared, *op, events);
            for run in 0..runs {
                let (ns, hits) = pass(&mut prepared, *op, events);
                println!(
                    "{label}\t{corpus_name}\t{}\t{set}\t{}\t{run}\t{ns:.1}\t{hits}",
                    escape(pattern),
                    op.name()
                );
            }
        }
    }
    ExitCode::SUCCESS
}

/// A pattern on one TSV field: tabs and newlines never occur in the
/// corpus, so this only guards the format.
fn escape(pattern: &str) -> String {
    pattern.replace('\t', "\\t").replace('\n', "\\n")
}

fn median(mut xs: Vec<f64>) -> f64 {
    xs.sort_by(f64::total_cmp);
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        f64::midpoint(xs[n / 2 - 1], xs[n / 2])
    }
}

/// One cell's samples, per backing label.
#[derive(Default)]
struct Cell {
    ns: BTreeMap<String, Vec<f64>>,
    hits: BTreeMap<String, Vec<String>>,
}

fn report(paths: &[String]) -> ExitCode {
    // (corpus order, pattern, set, op) -> cell; the key keeps the corpus's order.
    let mut order: Vec<(String, String)> = Vec::new();
    let mut cells: BTreeMap<(usize, String, String), Cell> = BTreeMap::new();
    let mut labels: Vec<String> = Vec::new();
    for path in paths {
        let file = std::fs::File::open(path).expect("a measurement file opens");
        for line in BufReader::new(file).lines() {
            let line = line.expect("a measurement line");
            let f: Vec<&str> = line.split('\t').collect();
            let [label, corpus, pattern, set, op, _run, ns, hits] = f[..] else {
                panic!("a measurement row has eight fields");
            };
            if !labels.iter().any(|l| l == label) {
                labels.push(label.to_owned());
            }
            let key = (corpus.to_owned(), pattern.to_owned());
            let index = order.iter().position(|k| *k == key).unwrap_or_else(|| {
                order.push(key.clone());
                order.len() - 1
            });
            let cell = cells
                .entry((index, set.to_owned(), op.to_owned()))
                .or_default();
            if ns != "refused" {
                cell.ns
                    .entry(label.to_owned())
                    .or_default()
                    .push(ns.parse().expect("ns a line"));
            }
            cell.hits
                .entry(label.to_owned())
                .or_default()
                .push(if ns == "refused" {
                    format!("refused ({hits})")
                } else {
                    hits.to_owned()
                });
        }
    }
    let [new, old] = &labels[..] else {
        eprintln!("live-probe: report wants exactly two backings, got {labels:?}");
        return ExitCode::FAILURE;
    };

    println!(
        "ns a line, the median of every timed pass; ratio = {new} / {old} (above 1.00, {new} is slower)"
    );
    println!();
    println!(
        "{:<10} {:<46} {:<8} {:>12} {:>12} {:>6}  {:>12} {:>12} {:>6} {:>5}  matched lines (of {LINES})",
        "corpus",
        "pattern",
        "lines",
        format!("match {new}"),
        format!("match {old}"),
        "ratio",
        format!("capt {new}"),
        format!("capt {old}"),
        "ratio",
        "net"
    );
    println!(
        "(net: the captures ratio with each backing's control median, the row conversion, taken off both)"
    );
    let mut parity_failures = 0;
    let mut worst: BTreeMap<&str, (f64, String)> = BTreeMap::new();
    // The control's median a line set, per backing: the row conversion an
    // extract pays beside its regex.
    let control = |label: &str, set: &str| -> Option<f64> {
        let index = order.iter().position(|(c, _)| c == "control")?;
        cells
            .get(&(index, set.to_owned(), "control".to_owned()))?
            .ns
            .get(label)
            .map(|xs| median(xs.clone()))
    };
    for (index, (corpus, pattern)) in order.iter().enumerate() {
        for set in ["ascii", "unicode"] {
            let mut row = format!("{corpus:<10} {:<46} {set:<8}", short(pattern));
            let mut matched = Vec::new();
            for op in ["is_match", "captures"] {
                let op = if corpus == "control" { "control" } else { op };
                let Some(cell) = cells.get(&(index, set.to_owned(), op.to_owned())) else {
                    row.push_str(&format!(" {:>12} {:>12} {:>6} ", "-", "-", "-"));
                    continue;
                };
                let a = cell.ns.get(new).map(|xs| median(xs.clone()));
                let b = cell.ns.get(old).map(|xs| median(xs.clone()));
                match (a, b) {
                    (Some(a), Some(b)) => {
                        let ratio = a / b;
                        row.push_str(&format!(" {a:>12.1} {b:>12.1} {ratio:>6.2} "));
                        let label = format!("{corpus} {} ({set})", short(pattern));
                        if worst.get(op).is_none_or(|(r, _)| ratio > *r) {
                            worst.insert(op, (ratio, label.clone()));
                        }
                        if op == "captures"
                            && let (Some(ca), Some(cb)) = (control(new, set), control(old, set))
                        {
                            let net = (a - ca) / (b - cb);
                            row.push_str(&format!("{net:>5.2} "));
                            if worst.get("captures net").is_none_or(|(r, _)| net > *r) {
                                worst.insert("captures net", (net, label));
                            }
                        }
                    }
                    _ => row.push_str(&format!(
                        " {:>12} {:>12} {:>6} ",
                        a.map_or("refused".to_owned(), |a| format!("{a:.1}")),
                        b.map_or("refused".to_owned(), |b| format!("{b:.1}")),
                        "-"
                    )),
                }
                let hits_new = cell.hits.get(new).cloned().unwrap_or_default();
                let hits_old = cell.hits.get(old).cloned().unwrap_or_default();
                let same = !hits_new.is_empty()
                    && hits_new.iter().chain(&hits_old).all(|h| *h == hits_new[0]);
                if same {
                    matched.push(hits_new[0].clone());
                } else {
                    parity_failures += 1;
                    matched.push(format!(
                        "DIFFER {new} {:?} {old} {:?}",
                        dedup(&hits_new),
                        dedup(&hits_old)
                    ));
                }
                if corpus == "control" {
                    break;
                }
            }
            println!("{row} {}", matched.join(" / "));
        }
    }
    println!();
    for (op, (ratio, label)) in &worst {
        println!("worst {op} ratio: {ratio:.2}, {label}");
    }
    if let (Some((m, _)), Some((c, cl)), Some((n, nl))) = (
        worst.get("is_match"),
        worst.get("captures"),
        worst.get("captures net"),
    ) {
        println!(
            "REGRESSION: {new} against {old}: is_match at most {m:.2}x in any cell; captures at most {c:.2}x for the whole extract stage ({cl}), {n:.2}x for its regex work net of the row conversion ({nl})"
        );
    }
    if parity_failures == 0 {
        println!("PARITY PASS: both backings matched or emitted the same lines in every cell");
        ExitCode::SUCCESS
    } else {
        println!("PARITY FAIL: {parity_failures} cells matched different lines");
        ExitCode::from(3)
    }
}

fn dedup(xs: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for x in xs {
        if !out.contains(x) {
            out.push(x.clone());
        }
    }
    out
}

/// A pattern cut to the table's column.
fn short(pattern: &str) -> String {
    if pattern.chars().count() <= 46 {
        pattern.to_owned()
    } else {
        let mut s: String = pattern.chars().take(45).collect();
        s.push('…');
        s
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("corpus") => print_corpus(),
        Some("measure") if args.len() == 4 => measure(
            &args[1],
            &args[2],
            args[3].parse().expect("RUNS is a count"),
        ),
        Some("report") if args.len() > 1 => report(&args[1..]),
        _ => {
            eprintln!("usage: live-probe corpus | measure LABEL CORPUS RUNS | report TSV...");
            ExitCode::FAILURE
        }
    }
}
