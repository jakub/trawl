// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Deterministic NDJSON corpus generation for the ingest and field-catalog
//! boundary. The output is deliberately just JSON objects separated by `\n`:
//! Vector's `file` source can decode it, and an HTTP test can post the same
//! bytes directly as `application/x-ndjson`.

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
    /// ADR-0013 derivation surface: severity source PRECEDENCE, the
    /// numeric dialect domain, the time sources (read AND stored), and
    /// every branch of the sealed `_`-prefix strip — including the empty
    /// remainder, the bare-name collision, and a forged `_severity`.
    ///
    /// The invariant this phase exists to break: every accepted field
    /// stays queryable under SOME name. Nothing is consumed, and the one
    /// strip rule either renames or (with no remainder) drops into
    /// `_raw`.
    #[default]
    Mutate,
    /// Valid JSON objects that trawl should reject at envelope validation.
    Rejects,
}

#[derive(Debug)]
pub struct Config {
    pub phase: Phase,
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
                "xtask: emitted {rows} {:?} NDJSON events (seed={}, namespace={})",
                config.phase, config.seed, config.namespace
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
    Ok(())
}

fn emit(config: &Config, out: &mut impl io::Write) -> io::Result<usize> {
    let names = Names::new(&config.namespace, config.seed);
    let events = match config.phase {
        Phase::Pin => pin_events(config, &names),
        Phase::Conflicts => conflict_events(config, &names),
        Phase::Mutate => mutation_events(config, &names),
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
    // A severity SOURCE, stored verbatim: derivation reads it and leaves
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
            // Source PRECEDENCE: `severity` → `severity_text` → `level`,
            // first mappable wins, and every one of them lands as its own
            // column whatever the derivation decides.
            if seq % 7 == 0 {
                event.remove("severity");
                event.insert("level".into(), json!(severity_value(seq)));
            }
            if seq % 3 == 0 {
                event.insert("severity_text".into(), json!(severity_value(seq + 1)));
            }
            // The sealed `_` prefix at the ingest door (ADR-0013 §5):
            // server-stamped slots, the internal provenance key, a
            // journald-shaped name, an empty remainder, and the
            // bare-name collision — every branch of the ONE strip rule.
            if seq % 11 == 0 {
                event.insert("_ingested".into(), json!("client-forged"));
                event.insert("_repairs".into(), json!("client-forged"));
                event.insert("_trawl_wal_file".into(), json!("client-forged"));
            }
            if seq % 4 == 0 {
                event.insert("_HOSTNAME".into(), json!("journald-box"));
                event.insert("__name__".into(), json!("prometheus-series"));
            }
            if seq % 9 == 0 {
                event.insert("___".into(), json!("no bare remainder"));
                event.insert(names.field("collide"), json!("bare wins"));
                event.insert(format!("_{}", names.field("collide")), json!("loser"));
            }
            // A forged verdict: derivation-only, so it strips to bare
            // `severity` and is then READ like any other source.
            if seq % 6 == 0 {
                event.insert("_severity".into(), json!(severity_value(seq)));
            }
            if seq % 13 == 0 {
                event.insert("timestamp".into(), timestamp_value(seq));
            }
            if seq % 8 == 0 {
                event.insert("@timestamp".into(), timestamp_value(seq + 2));
            }
            if seq % 17 == 0 {
                event.insert(
                    "x".repeat(trawl_core::schema::MAX_FIELD_NAME_BYTES + 1),
                    json!("dropped-but-retained-in-raw"),
                );
            }
            if seq % 19 == 0 {
                event.insert(names.field("case_probe"), json!("lower-wins"));
                event.insert(names.field("CASE_PROBE"), json!("folded-loses"));
            }
            if seq % 23 == 0 {
                event.insert("_raw".into(), json!({"forged": "object"}));
            }
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
            seed: 42,
            namespace: "test".to_owned(),
            env: "prod".to_owned(),
            mutation_events: 32,
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
    fn namespace_is_bounded_and_shell_friendly() {
        let mut config = config(Phase::Pin);
        assert!(validate_config(&config).is_ok());
        config.namespace = "Has-Dashes".to_owned();
        assert!(validate_config(&config).is_err());
        config.namespace = "x".repeat(25);
        assert!(validate_config(&config).is_err());
    }
}
