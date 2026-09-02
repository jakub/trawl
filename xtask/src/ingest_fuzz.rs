// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Deterministic NDJSON corpus generation for the ingest and field-catalog
//! boundary. The output is deliberately just JSON objects separated by `\n`:
//! Vector's `file` source can decode it, and an HTTP test can post the same
//! bytes directly as `application/x-ndjson`.
//!
//! Three producer profiles sit behind one canonicalizer, so `--profile`
//! selects which door's payload shape the `mutate` corpus imitates:
//! `http` emits wire events exactly as a sender posts them, while
//! `syslog` and `trawld` emit the payload maps their doors hand
//! `envelope::canonicalize` (the `syslog_*`/`sd_*` parse artifacts and
//! the tracing-visitor fields respectively). Those two are still
//! ordinary NDJSON, so they replay over HTTP the way a syslog-over-HTTP
//! forwarder's batch does. The in-process proof that each shape survives
//! its own door is `crates/trawl-server/tests/profile_fuzz.rs`, which
//! generates its own corpus deliberately: this command's job is a wire
//! artifact, that test's job is the function.

use std::io::{self, BufWriter, Write as _};
use std::process::ExitCode;

use clap::ValueEnum;
use rand::rngs::StdRng;
use rand::{Rng as _, SeedableRng as _};
use serde_json::{Map, Value, json};

/// One independently-emittable part of a pinning run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Phase {
    /// Ten accepted rows that directly pin the five canonical scalar types,
    /// defer an all-null field, and straddle the pin ladder's 90% boundary.
    Pin,
    /// Values that conform, drift in representation, or conflict with the
    /// pins installed by `pin`. Compact the pin phase before sending these.
    Conflicts,
    /// Seeded accepted events covering scalar boundaries, nested values,
    /// hostile-but-storable names, casing, repair paths, and the whole
    /// derivation surface of ADR-0013: severity source precedence, the
    /// numeric dialect domain, the time sources (read and stored), and
    /// every branch of the sealed `_`-prefix strip, including the empty
    /// remainder, the bare-name collision, and a forged `_severity`.
    ///
    /// The invariant this phase exists to break: every accepted field
    /// stays queryable under some name. Nothing is consumed, and the
    /// strip rule either renames the field or, with no remainder, drops
    /// it with the value still in `_raw`.
    #[default]
    Mutate,
    /// Valid JSON objects that trawl should reject at envelope validation.
    Rejects,
}

/// Which producer profile the `mutate` corpus is shaped for.
///
/// The profiles share one canonicalizer, so what differs is only the
/// payload a door hands it, which is exactly what a corpus can imitate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Profile {
    /// Wire events as an HTTP sender posts them: the door asserts nothing
    /// and the sender owns every identity field.
    #[default]
    Http,
    /// The payload the syslog door builds from a frame: the `_raw` wire
    /// line, the `syslog_*` parse artifacts and the unfolded `sd_*`
    /// structured-data pairs, with identity left to the profile's
    /// assertion.
    Syslog,
    /// The payload the telemetry layer builds from a tracing event:
    /// ordinary sender vocabulary (`level`, `target`, `event_type`,
    /// `message`), a `_time` proposal, and whatever field names the
    /// daemon's own call sites happened to use.
    Trawld,
}

impl Profile {
    /// The one spelling — the `--profile` token and the `_producer`
    /// column value a replay of this corpus should land under.
    const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Syslog => "syslog",
            Self::Trawld => "trawld",
        }
    }
}

#[derive(Debug)]
pub struct Config {
    pub phase: Phase,
    pub profile: Profile,
    pub seed: u64,
    pub namespace: String,
    pub env: String,
    pub mutation_events: usize,
}

#[derive(Debug)]
struct Names {
    field_prefix: String,
    service_prefix: String,
}

impl Names {
    fn new(namespace: &str, seed: u64) -> Self {
        Self {
            field_prefix: format!("fuzz_{namespace}_{seed:016x}"),
            service_prefix: format!("fuzz-{namespace}-{seed:016x}"),
        }
    }

    fn field(&self, suffix: &str) -> String {
        format!("{}_{suffix}", self.field_prefix)
    }

    fn service(&self, phase: &str) -> String {
        format!("{}-{phase}", self.service_prefix)
    }
}

pub fn run(config: &Config) -> ExitCode {
    if let Err(e) = validate_config(config) {
        eprintln!("xtask: {e}");
        return ExitCode::FAILURE;
    }

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    match emit(config, &mut out) {
        Ok(rows) => {
            if let Err(e) = out.flush() {
                if e.kind() == io::ErrorKind::BrokenPipe {
                    return ExitCode::SUCCESS;
                }
                eprintln!("xtask: flush ingest-fuzz output: {e}");
                return ExitCode::FAILURE;
            }
            eprintln!(
                "xtask: emitted {rows} {:?} NDJSON events (profile={}, seed={}, namespace={})",
                config.phase,
                config.profile.as_str(),
                config.seed,
                config.namespace
            );
            ExitCode::SUCCESS
        }
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: emit ingest-fuzz corpus: {e}");
            ExitCode::FAILURE
        }
    }
}

fn validate_config(config: &Config) -> Result<(), String> {
    let namespace_ok = !config.namespace.is_empty()
        && config.namespace.len() <= 24
        && config
            .namespace
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !namespace_ok {
        return Err("--namespace must match [a-z0-9_]{1,24}".to_owned());
    }
    let env_ok = !config.env.is_empty()
        && config.env.len() <= 32
        && config
            .env
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-'));
    if !env_ok {
        return Err("--env must match [a-z0-9_-]{1,32}".to_owned());
    }
    // The fixed contract phases describe the catalog boundary, not a
    // door: `pin`/`conflicts` are pin-ladder corpora and `rejects` is a
    // list of things only the HTTP door can refuse (a profile producer
    // asserts boot-validated identity and has nobody to reject to). A
    // silently HTTP-shaped `--profile syslog --phase rejects` would be
    // the worst of both.
    if config.profile != Profile::Http && config.phase != Phase::Mutate {
        return Err(format!(
            "--profile {} shapes the `mutate` phase only; {:?} is a \
             door-independent corpus",
            config.profile.as_str(),
            config.phase
        ));
    }
    Ok(())
}

fn emit(config: &Config, out: &mut impl io::Write) -> io::Result<usize> {
    let names = Names::new(&config.namespace, config.seed);
    let events = match config.phase {
        Phase::Pin => pin_events(config, &names),
        Phase::Conflicts => conflict_events(config, &names),
        Phase::Mutate => match config.profile {
            Profile::Http => mutation_events(config, &names),
            Profile::Syslog => syslog_payload_events(config, &names),
            Profile::Trawld => trawld_payload_events(config, &names),
        },
        Phase::Rejects => reject_events(config, &names),
    };
    let rows = events.len();
    for event in events {
        serde_json::to_writer(&mut *out, &event).map_err(io::Error::other)?;
        out.write_all(b"\n")?;
    }
    Ok(rows)
}

fn base_event(config: &Config, names: &Names, phase: &str, seq: usize) -> Map<String, Value> {
    let mut event = Map::new();
    event.insert("env".into(), json!(config.env));
    event.insert("service".into(), json!(names.service(phase)));
    event.insert("host".into(), json!("fuzz-host"));
    // A severity source, stored verbatim: derivation reads it and leaves
    // it exactly where it is (ADR-0013 §2).
    event.insert("severity".into(), json!("info"));
    event.insert(
        "message".into(),
        json!(format!("trawl ingest fuzz {phase} row {seq}")),
    );
    // Text keeps this common provenance field type-stable even when a seed
    // exceeds BIGINT; seed-derived test fields themselves remain isolated.
    event.insert(
        "trawl_fuzz_seed".into(),
        json!(format!("{:016x}", config.seed)),
    );
    event.insert("trawl_fuzz_phase".into(), json!(phase));
    event.insert("trawl_fuzz_seq".into(), json!(seq));
    event
}

fn pin_events(config: &Config, names: &Names) -> Vec<Value> {
    (0..10)
        .map(|seq| {
            let mut event = base_event(config, names, "pin", seq);
            let n = i64::try_from(seq).expect("the fixed pin phase has ten rows");
            let fraction =
                f64::from(u32::try_from(seq).expect("the fixed pin phase has ten rows")) + 0.25;
            event.insert(names.field("bigint"), json!(1_000_i64 + n));
            event.insert(names.field("double"), json!(fraction));
            event.insert(names.field("boolean"), json!(seq % 2 == 0));
            event.insert(
                names.field("timestamp"),
                json!(format!("2024-01-02T03:04:{seq:02}Z")),
            );
            event.insert(names.field("varchar"), json!(format!("word-{seq}")));
            event.insert(
                names.field("nested"),
                json!({"pod": format!("pod-{seq}"), "ready": seq % 2 == 0}),
            );
            event.insert(names.field("all_null"), Value::Null);
            event.insert(
                names.field("ladder_90_bigint"),
                if seq < 9 {
                    json!(200_i64 + n)
                } else {
                    json!("not-an-integer")
                },
            );
            event.insert(
                names.field("ladder_80_varchar"),
                if seq < 8 {
                    json!(300_i64 + n)
                } else {
                    json!("not-an-integer")
                },
            );
            event.insert(names.field("unsigned_overflow"), json!(u64::MAX));
            // The uppercase spelling is folded before it reaches the catalog.
            event.insert(names.field("CaseFolded"), json!(format!("fold-{seq}")));
            Value::Object(event)
        })
        .collect()
}

fn conflict_events(config: &Config, names: &Names) -> Vec<Value> {
    let bigint = [
        json!(404),
        json!("404"),
        json!("0404"),
        json!("1.5"),
        json!("0x10"),
        json!(i64::MAX),
        json!(u64::MAX),
        Value::Null,
    ];
    let double = [
        json!(1.5),
        json!("1e-7"),
        json!("-0.0"),
        json!("nan"),
        json!("inf"),
        json!("0x10"),
        json!(u64::MAX),
        Value::Null,
    ];
    let boolean = [
        json!(true),
        json!(false),
        json!("true"),
        json!("false"),
        json!("TRUE"),
        json!("yes"),
        json!(1),
        Value::Null,
    ];
    let timestamp = [
        json!("2024-01-02T03:04:05Z"),
        json!("2024-01-02T09:00:00+05:30"),
        json!("2024-01-02 03:04:05"),
        json!("2024-01-02"),
        json!("not-a-time"),
        json!(0),
        json!(true),
        Value::Null,
    ];
    let varchar = [
        json!("plain"),
        json!(400),
        json!(true),
        json!({"nested": 1}),
        json!([1, 2, 3]),
        json!(""),
        json!("café"),
        Value::Null,
    ];

    (0..bigint.len())
        .map(|seq| {
            let mut event = base_event(config, names, "conflicts", seq);
            event.insert(names.field("bigint"), bigint[seq].clone());
            event.insert(names.field("double"), double[seq].clone());
            event.insert(names.field("boolean"), boolean[seq].clone());
            event.insert(names.field("timestamp"), timestamp[seq].clone());
            event.insert(names.field("varchar"), varchar[seq].clone());
            let n = i64::try_from(seq).expect("the fixed conflict phase has eight rows");
            event.insert(names.field("all_null"), json!(n));
            event.insert(names.field("ladder_90_bigint"), bigint[seq].clone());
            event.insert(names.field("ladder_80_varchar"), varchar[seq].clone());
            Value::Object(event)
        })
        .collect()
}

fn mutation_events(config: &Config, names: &Names) -> Vec<Value> {
    let mut rng = StdRng::seed_from_u64(config.seed);
    let dynamic_fields = [
        names.field("dynamic_00"),
        names.field("dynamic_01"),
        format!("{}.dotted", names.field_prefix),
        format!("{}/pointer", names.field_prefix),
        format!("{}~tilde", names.field_prefix),
        format!("${}", names.field_prefix),
        format!("{}_ünicode", names.field_prefix),
    ];
    let values = mutation_values();

    (0..config.mutation_events)
        .map(|seq| {
            let mut event = base_event(config, names, "mutate", seq);
            let fields = rng.gen_range(1..=5);
            for _ in 0..fields {
                let name = &dynamic_fields[rng.gen_range(0..dynamic_fields.len())];
                let value = values[rng.gen_range(0..values.len())].clone();
                event.insert(name.clone(), value);
            }

            // Deterministic cadence guarantees coverage even for seeds whose
            // random selection misses a class.
            if seq % 5 == 0 {
                event.insert(
                    names.field("nested_mutation"),
                    json!({"outer": {"inner": seq}, "list": [null, true, seq]}),
                );
            }
            // Source precedence: `severity` → `severity_text` → `level`,
            // first mappable wins, and every one of them lands as its own
            // column whatever the derivation decides.
            if seq % 7 == 0 {
                event.remove("severity");
                event.insert("level".into(), json!(severity_value(seq)));
            }
            if seq % 3 == 0 {
                event.insert("severity_text".into(), json!(severity_value(seq + 1)));
            }
            if seq % 13 == 0 {
                event.insert("timestamp".into(), timestamp_value(seq));
            }
            if seq % 8 == 0 {
                event.insert("@timestamp".into(), timestamp_value(seq + 2));
            }
            if seq % 19 == 0 {
                event.insert(names.field("case_probe"), json!("lower-wins"));
                event.insert(names.field("CASE_PROBE"), json!("folded-loses"));
            }
            universal_gate_probes(&mut event, names, seq);
            Value::Object(event)
        })
        .collect()
}

/// The gates every door applies, probed on a deterministic cadence so no
/// seed can miss a branch: the sealed `_` prefix (ADR-0013 §5) in each of
/// its shapes (server-stamped slots, the internal provenance key, a
/// journald-shaped name, an empty remainder, the bare-name collision, a
/// forged verdict, a forged provenance stamp), plus the name-length cap
/// and a non-string `_raw`.
///
/// One function for all three profiles on purpose: these gates are the
/// canonicalizer's, not a door's, so a corpus that probed them per-door
/// could drift about what "adversarial" even means.
fn universal_gate_probes(event: &mut Map<String, Value>, names: &Names, seq: usize) {
    if seq.is_multiple_of(11) {
        event.insert("_ingested".into(), json!("client-forged"));
        event.insert("_repairs".into(), json!("client-forged"));
        event.insert("_trawl_wal_file".into(), json!("client-forged"));
        // Provenance is stamped by the door, so this must strip to a bare
        // `producer` and never reach `_producer`.
        event.insert("_producer".into(), json!("client-forged"));
    }
    if seq.is_multiple_of(4) {
        event.insert("_HOSTNAME".into(), json!("journald-box"));
        event.insert("__name__".into(), json!("prometheus-series"));
    }
    if seq.is_multiple_of(9) {
        event.insert("___".into(), json!("no bare remainder"));
        event.insert(names.field("collide"), json!("bare wins"));
        event.insert(format!("_{}", names.field("collide")), json!("loser"));
    }
    // A forged verdict: derivation-only, so it strips to bare `severity`
    // and is then read like any other source.
    if seq.is_multiple_of(6) {
        event.insert("_severity".into(), json!(severity_value(seq)));
    }
    if seq.is_multiple_of(17) {
        event.insert(
            "x".repeat(trawl_core::schema::MAX_FIELD_NAME_BYTES + 1),
            json!("dropped-but-retained-in-raw"),
        );
    }
    if seq.is_multiple_of(23) {
        event.insert("_raw".into(), json!({"forged": "object"}));
    }
}

/// The payload the syslog door hands the canonicalizer: the pre-parse
/// wire line as a `_raw` proposal, the `syslog_*` parse artifacts, and
/// the `sd_*` structured-data pairs unfolded and uncapped. The listener
/// owns none of those rules; the canonicalizer does.
///
/// The identity fields are here too, because the profile asserts them: a
/// payload key that names one is the assertion-precedence case, and
/// including them is also what makes the corpus replayable over HTTP the
/// way a syslog-over-HTTP forwarder's batch is.
fn syslog_payload_events(config: &Config, names: &Names) -> Vec<Value> {
    let mut rng = StdRng::seed_from_u64(config.seed);
    let values = mutation_values();

    (0..config.mutation_events)
        .map(|seq| {
            let mut event = base_event(config, names, "syslog", seq);
            // The syslog door never publishes a bare `severity` — and on an
            // HTTP replay it would win the severity_from chain at position 0,
            // shadowing the `syslog_severity` + dialect knob this corpus
            // exists to exercise (the forwarder recipe in configuration.md).
            event.remove("severity");
            // The wire frame, proposed as `_raw`. Every eleventh one is
            // past the door's MAX_RAW_CHARS, the oversized-datagram case
            // the canonicalizer truncates. The 65 536-char bound is
            // mirrored, not imported: xtask has no trawl-server dep.
            let frame = if seq % 11 == 0 {
                format!("<{}>1 - - - - - {}", 13 + seq % 8, "A".repeat(70_000))
            } else {
                format!(
                    "<{}>1 2026-02-15T12:00:{:02}Z host-{seq} app {seq} ID{seq} - frame {seq}",
                    13 + seq % 8,
                    seq % 60
                )
            };
            event.insert("_raw".into(), json!(frame));

            // The severity artifact: raw 0-7, sometimes out of the
            // dialect's domain, sometimes absent (a PRI-less frame).
            match seq % 10 {
                8 => {}
                9 => {
                    event.insert("syslog_severity".into(), json!(99));
                }
                n => {
                    event.insert("syslog_severity".into(), json!(n));
                }
            }
            // The timestamp artifact, omitted a third of the time so the
            // configured chain and then arrival time take over.
            if seq % 3 != 0 {
                event.insert("syslog_timestamp".into(), timestamp_value(seq));
            }
            event.insert("syslog_facility".into(), json!("local0"));
            event.insert("syslog_pid".into(), json!(format!("{seq}")));
            event.insert("syslog_msgid".into(), json!(format!("ID{seq}")));
            event.insert("syslog_source_ip".into(), json!("10.0.0.1"));

            // Structured data, exactly as the listener spells it: mixed
            // case, `@`-bearing SD-IDs, an over-long key and a
            // case-collision pair, all of which the door resolves.
            event.insert(
                "sd_exampleSDID@32473_eventID".into(),
                json!(format!("{seq}")),
            );
            event.insert("sd_examplesdid@32473_eventid".into(), json!("exact wins"));
            if seq % 7 == 0 {
                event.insert(
                    format!(
                        "sd_{}_k",
                        "L".repeat(trawl_core::schema::MAX_FIELD_NAME_BYTES)
                    ),
                    json!("dropped-but-retained-in-raw"),
                );
            }
            let value = values[rng.gen_range(0..values.len())].clone();
            event.insert("sd_origin@32473_ip".into(), value);

            universal_gate_probes(&mut event, names, seq);
            Value::Object(event)
        })
        .collect()
}

/// The payload the telemetry layer hands the canonicalizer: ordinary
/// sender vocabulary (no `trawld_` prefix), a `_time` proposal, and the
/// arbitrary field names trawld's own `tracing` call sites carry.
///
/// The identity collisions are the point: a tracing field named
/// `service` must lose to the profile's assertion with a repair rather
/// than misfile the daemon's own logs, and a >255-byte field name must
/// be dropped rather than reach the WAL and wedge compaction for
/// `service=trawld`.
fn trawld_payload_events(config: &Config, names: &Names) -> Vec<Value> {
    let mut rng = StdRng::seed_from_u64(config.seed);
    let values = mutation_values();
    // Names a `tracing` field could carry, mixed case included: field
    // names are Rust-side tokens, so `myField` is legal and the door is
    // what folds it.
    let tracing_fields = [
        "query_id".to_owned(),
        "outcome".to_owned(),
        "elapsed_ms".to_owned(),
        "myField".to_owned(),
        "myfield".to_owned(),
        "error_class".to_owned(),
        names.field("dynamic_00"),
    ];

    (0..config.mutation_events)
        .map(|seq| {
            let mut event = Map::new();
            // What the layer always writes. `service`/`env`/`host` are
            // not here — the profile asserts them — except where the
            // collision probe below plants one.
            event.insert("level".into(), json!(severity_value(seq)));
            event.insert("target".into(), json!("trawl_server::ingest::handler"));
            event.insert("event_type".into(), json!("fuzz_probe"));
            event.insert("message".into(), json!(format!("telemetry fuzz {seq}")));
            event.insert("_time".into(), timestamp_value(seq));
            event.insert(
                "trawl_fuzz_seed".into(),
                json!(format!("{:016x}", config.seed)),
            );
            event.insert("trawl_fuzz_phase".into(), json!("trawld"));
            event.insert("trawl_fuzz_seq".into(), json!(seq));

            let fields = rng.gen_range(1..=4);
            for _ in 0..fields {
                let name = &tracing_fields[rng.gen_range(0..tracing_fields.len())];
                let value = values[rng.gen_range(0..values.len())].clone();
                event.insert(name.clone(), value);
            }

            // Identity collisions: every slot the profile asserts, one
            // per cadence, so each is exercised alone and together.
            if seq % 3 == 0 {
                event.insert("service".into(), json!(names.service("trawld")));
            }
            if seq % 5 == 0 {
                event.insert("env".into(), json!("client-forged"));
            }
            if seq % 7 == 0 {
                event.insert("host".into(), json!("client-forged"));
            }

            universal_gate_probes(&mut event, names, seq);
            Value::Object(event)
        })
        .collect()
}

fn mutation_values() -> Vec<Value> {
    vec![
        Value::Null,
        json!(true),
        json!(false),
        json!(0),
        json!(-1),
        json!(i64::MIN),
        json!(i64::MAX),
        json!(u64::MAX),
        json!(-0.0),
        json!(0.000_000_1),
        json!(1.5),
        json!(""),
        json!("0"),
        json!("0404"),
        json!("1.5"),
        json!("1e3"),
        json!("0x10"),
        json!("nan"),
        json!("inf"),
        json!("true"),
        json!("TRUE"),
        json!("2024-01-02T09:00:00+05:30"),
        json!("not-a-time"),
        json!("line one\nline two\t\0"),
        json!("snowman ☃ café"),
        json!([1, "two", null, {"three": 3}]),
        json!({"pod": "api-0", "labels": {"app": "api"}}),
        json!("x".repeat(4_096)),
    ]
}

/// Severity source values across the whole derivation domain: a token, an
/// alias, an `OTel` exact short name, an in-ladder numeric (which reads as
/// `OTel`, never syslog-inverted), out-of-ladder numerics, an unmappable
/// word, and nothing at all.
fn severity_value(seq: usize) -> Value {
    match seq % 9 {
        0 => json!("ERROR"),
        1 => json!(7),
        2 => json!("0"),
        3 => json!("SPICY"),
        4 => json!(25),
        5 => json!("error2"),
        6 => json!("17"),
        7 => json!(1.5),
        _ => Value::Null,
    }
}

fn timestamp_value(seq: usize) -> Value {
    match seq % 7 {
        0 => json!("2024-01-02T03:04:05.123456Z"),
        1 => json!("2024-01-02T09:00:00+0530"),
        2 => json!("2024/01/02 03:04"),
        3 => json!("2024-01-02"),
        4 => json!("not-a-time"),
        5 => json!(1_704_164_645),
        _ => Value::Null,
    }
}

fn reject_events(config: &Config, names: &Names) -> Vec<Value> {
    let cases = [
        ("missing_service", None),
        ("service_not_string", Some(json!(42))),
        ("empty_service", Some(json!(""))),
        ("service_too_long", Some(json!("s".repeat(129)))),
        ("invalid_service_chars", Some(json!("bad/service"))),
        ("leading_dot_service", Some(json!(".hidden"))),
    ];
    let mut events: Vec<Value> = cases
        .into_iter()
        .enumerate()
        .map(|(seq, (case, service))| {
            let mut event = Map::new();
            event.insert("env".into(), json!(config.env));
            event.insert("host".into(), json!("fuzz-host"));
            event.insert("message".into(), json!(format!("expected reject: {case}")));
            event.insert(
                "trawl_fuzz_seed".into(),
                json!(format!("{:016x}", config.seed)),
            );
            event.insert("trawl_fuzz_phase".into(), json!("rejects"));
            event.insert("trawl_fuzz_seq".into(), json!(seq));
            event.insert("trawl_fuzz_expected_reject".into(), json!(case));
            if let Some(service) = service {
                event.insert("service".into(), service);
            }
            // Keep the generated namespace visible without relying on a valid
            // service field (several cases intentionally do not have one).
            event.insert("trawl_fuzz_namespace".into(), json!(names.field_prefix));
            Value::Object(event)
        })
        .collect();

    for (case, bad_env) in [
        ("env_not_string", json!({"nested": true})),
        ("invalid_env", json!("BAD/ENV")),
    ] {
        let seq = events.len();
        let mut event = base_event(config, names, "rejects", seq);
        event.insert("env".into(), bad_env);
        event.insert("trawl_fuzz_expected_reject".into(), json!(case));
        events.push(Value::Object(event));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(phase: Phase) -> Config {
        Config {
            phase,
            profile: Profile::Http,
            seed: 42,
            namespace: "test".to_owned(),
            env: "prod".to_owned(),
            mutation_events: 32,
        }
    }

    fn profiled(profile: Profile) -> Config {
        Config {
            profile,
            ..config(Phase::Mutate)
        }
    }

    fn render(config: &Config) -> Vec<u8> {
        let mut out = Vec::new();
        emit(config, &mut out).unwrap();
        out
    }

    fn objects(bytes: &[u8]) -> Vec<Map<String, Value>> {
        bytes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                serde_json::from_slice::<Value>(line)
                    .unwrap()
                    .as_object()
                    .unwrap()
                    .clone()
            })
            .collect()
    }

    #[test]
    fn output_is_deterministic_and_newline_delimited() {
        let config = config(Phase::Mutate);
        let first = render(&config);
        let second = render(&config);
        assert_eq!(first, second);
        assert!(first.ends_with(b"\n"));
        assert_eq!(objects(&first).len(), config.mutation_events);
    }

    #[test]
    fn seed_changes_names_and_mutations() {
        let first = config(Phase::Mutate);
        let mut second = config(Phase::Mutate);
        second.seed += 1;
        assert_ne!(render(&first), render(&second));
    }

    #[test]
    fn pin_phase_straddles_the_ladder_threshold_exactly() {
        let config = config(Phase::Pin);
        let names = Names::new(&config.namespace, config.seed);
        let events = objects(&render(&config));
        assert_eq!(events.len(), 10);
        let ninety = names.field("ladder_90_bigint");
        let eighty = names.field("ladder_80_varchar");
        assert_eq!(events.iter().filter(|e| e[&ninety].is_number()).count(), 9);
        assert_eq!(events.iter().filter(|e| e[&eighty].is_number()).count(), 8);
        assert!(events.iter().all(|e| e[&names.field("all_null")].is_null()));
    }

    #[test]
    fn every_phase_is_valid_json_objects_only() {
        for phase in [Phase::Pin, Phase::Conflicts, Phase::Mutate, Phase::Rejects] {
            let config = config(phase);
            assert!(!objects(&render(&config)).is_empty());
        }
    }

    #[test]
    fn every_profile_corpus_is_deterministic_and_distinct() {
        let mut rendered = Vec::new();
        for profile in [Profile::Http, Profile::Syslog, Profile::Trawld] {
            let config = profiled(profile);
            let first = render(&config);
            assert_eq!(
                first,
                render(&config),
                "{} is not deterministic",
                profile.as_str()
            );
            assert_eq!(objects(&first).len(), config.mutation_events);
            rendered.push(first);
        }
        assert_ne!(rendered[0], rendered[1], "http and syslog corpora coincide");
        assert_ne!(
            rendered[1], rendered[2],
            "syslog and trawld corpora coincide"
        );
    }

    /// Every profile's corpus probes the gates the one canonicalizer
    /// applies to all of them. `crates/trawl-server/tests/profile_fuzz.rs`
    /// asserts what those probes produce.
    #[test]
    fn every_profile_corpus_probes_the_universal_gates() {
        for profile in [Profile::Http, Profile::Syslog, Profile::Trawld] {
            let events = objects(&render(&profiled(profile)));
            let has = |key: &str| events.iter().any(|e| e.contains_key(key));
            for key in ["_severity", "_producer", "_HOSTNAME", "___", "_raw"] {
                assert!(has(key), "{} corpus never probes {key}", profile.as_str());
            }
            assert!(
                events.iter().any(|e| e
                    .keys()
                    .any(|k| k.len() > trawl_core::schema::MAX_FIELD_NAME_BYTES)),
                "{} corpus never probes the name-length cap",
                profile.as_str()
            );
        }
    }

    #[test]
    fn the_syslog_corpus_carries_the_parse_artifacts_the_listener_publishes() {
        let events = objects(&render(&profiled(Profile::Syslog)));
        let has = |key: &str| events.iter().any(|e| e.contains_key(key));
        for key in [
            "syslog_severity",
            "syslog_timestamp",
            "syslog_facility",
            "syslog_pid",
            "syslog_msgid",
            "syslog_source_ip",
            "sd_exampleSDID@32473_eventID",
        ] {
            assert!(has(key), "the syslog corpus must publish {key}");
        }
        // An absent PRI and an absent frame timestamp are both real
        // shapes: absence is what makes the door fall through honestly.
        assert!(
            events.iter().any(|e| !e.contains_key("syslog_severity")),
            "a PRI-less frame must appear"
        );
        assert!(
            events.iter().any(|e| !e.contains_key("syslog_timestamp")),
            "a timestamp-less frame must appear"
        );
        // The oversized frame, past the door's MAX_RAW_CHARS.
        assert!(
            events.iter().any(|e| e["_raw"]
                .as_str()
                .is_some_and(|s| s.chars().count() > 65_536)),
            "an oversized datagram must appear"
        );
    }

    #[test]
    fn the_trawld_corpus_collides_with_every_asserted_slot() {
        let events = objects(&render(&profiled(Profile::Trawld)));
        // `message` is deliberately absent here: the trawld profile does
        // not assert it (the payload IS the message), so a payload one is
        // ordinary data, not a collision.
        for slot in ["service", "env", "host"] {
            assert!(
                events.iter().any(|e| e.contains_key(slot)),
                "the trawld corpus must collide with the asserted {slot}"
            );
        }
        // Ordinary sender vocabulary, no `trawld_` prefix.
        assert!(
            events
                .iter()
                .all(|e| e.keys().all(|k| !k.starts_with("trawld_"))),
            "telemetry fields are ordinary vocabulary, never prefixed"
        );
    }

    #[test]
    fn a_profile_only_shapes_the_mutate_phase() {
        for phase in [Phase::Pin, Phase::Conflicts, Phase::Rejects] {
            let config = Config {
                profile: Profile::Syslog,
                ..config(phase)
            };
            let err = validate_config(&config).expect_err("a profiled contract phase must refuse");
            assert!(
                err.contains("syslog"),
                "the message must name the profile: {err}"
            );
        }
        assert!(validate_config(&profiled(Profile::Syslog)).is_ok());
    }

    #[test]
    fn namespace_is_bounded_and_shell_friendly() {
        let mut config = config(Phase::Pin);
        assert!(validate_config(&config).is_ok());
        config.namespace = "Has-Dashes".to_owned();
        assert!(validate_config(&config).is_err());
        config.namespace = "x".repeat(25);
        assert!(validate_config(&config).is_err());
    }
}
