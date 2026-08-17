// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The bounded, in-process half of the ingest fuzzer (issue #75, AC 9):
//! all THREE producer profiles driven through the one canonicalizer, with
//! the gates that are supposed to be universal asserted per profile.
//!
//! Slice 2's whole claim is that a profile bypasses nothing — the ASCII
//! fold, the sealed `_`-prefix strip, the field-name length drop, the
//! `_raw` cap and the `_producer` stamp belong to the door and apply to
//! every one of them. That claim is exactly the kind a per-door unit test
//! proves only where someone thought to look, so this walks a hostile
//! corpus through each door and asserts the invariants as a SET:
//!
//! - no key in the output is in trawl's `_` namespace unless it is one of
//!   the envelope's own slots (a payload `_producer` must have become a
//!   bare `producer`, so the column is unforgeable);
//! - `_raw` is present, a string, and at most [`MAX_RAW_CHARS`];
//! - every key is at most [`MAX_FIELD_NAME_BYTES`] and carries no ASCII
//!   uppercase (the two rules the catalog and `DuckDB` respectively need);
//! - `_producer` equals the profile that admitted the event;
//! - what a profile ASSERTS is what the event carries;
//! - the `trawld` profile NEVER rejects (AC 3's invariant, fuzzed —
//!   there is nobody for trawld to reject to).
//!
//! Deterministic and bounded: one fixed [`SEED`], a few thousand events
//! per profile, seconds in the ordinary `cargo nextest` run. **To
//! reproduce a failure**, every assertion names its profile, its round
//! index and the seed; feed those back into [`Rng::new`] (or change
//! [`SEED`] locally to widen the search).
//!
//! The wire-corpus generator in `xtask/src/ingest_fuzz.rs` is deliberately
//! independent of this file: that one emits NDJSON to replay against a
//! LIVE server (`--profile http|syslog|trawld`), this one drives the
//! function. Neither can stand in for the other, and sharing a generator
//! across the crate boundary would mean a dev-dependency on the task
//! runner.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use serde_json::{Map, Value, json};
use trawl_core::schema::{self, MAX_FIELD_NAME_BYTES};
use trawl_server::ingest::envelope::{
    Canonical, EnvelopeContext, MAX_RAW_CHARS, RejectReason, canonicalize,
};
use trawl_server::ingest::producer::{Asserted, Derivation, Producer, ProducerKind};
use trawl_server::syslog::convert::SyslogDoor;

/// The one seed. Fixed so a failure is reproducible from the transcript
/// alone; change it locally to widen the search.
const SEED: u64 = 0x5157_1075_0000_0075;

/// Events per profile. Large enough that every cadence in
/// [`hostile_payload`] fires many times over, small enough that the whole
/// file is well under a second — this IS the bounded CI run, so it lives
/// in the default suite rather than behind `#[ignore]`.
const ROUNDS: u64 = 1_500;

const ENV: &str = "prod";

/// A deterministic LCG. No `rand` dev-dependency and no proptest: the
/// corpus is a fixed walk, so "it failed at round 812" is a complete bug
/// report.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn bits(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 11
    }

    fn pick<'a, T>(&mut self, choices: &'a [T]) -> &'a T {
        let index = usize::try_from(self.bits()).unwrap_or(0) % choices.len();
        &choices[index]
    }
}

/// Field names a hostile sender — or an honest one with unlucky
/// vocabulary — can put in front of a door. Deliberately excludes the
/// identity slots: those are planted explicitly, so a reject stays
/// attributable.
fn hostile_names() -> Vec<String> {
    [
        "_severity",
        "_producer",
        "_ingested",
        "_repairs",
        "_raw",
        "_time",
        "_trawl_wal_file",
        "_HOSTNAME",
        "__name__",
        "_",
        "___",
        "MiXeD",
        "mixed",
        "café",
        "sd_exampleSDID@32473_eventID",
        "sd_examplesdid@32473_eventid",
        "level",
        "severity",
        "severity_text",
        "syslog_severity",
        "syslog_timestamp",
        "timestamp",
        "@timestamp",
        "dur",
        "x.y",
        "a/b",
        "",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    // Past the catalog's btree-key bound: it must be dropped, not stored.
    .chain(std::iter::once("k".repeat(MAX_FIELD_NAME_BYTES + 45)))
    .collect()
}

/// Values covering every JSON shape a door has to survive, including the
/// one that overruns `_raw` on its own.
fn hostile_value(seed: u64) -> Value {
    match seed % 12 {
        0 => Value::Null,
        1 => json!(true),
        2 => json!(-1_i64),
        3 => json!(u64::MAX),
        4 => json!(1.5_f64),
        5 => json!(""),
        6 => json!("nul\u{0}and\ttab"),
        7 => json!("2026-02-15T09:00:00+05:30"),
        8 => json!("x".repeat(300)),
        9 => json!("z".repeat(70_000)),
        10 => json!({"nested": {"deep": [1, null, true]}}),
        _ => json!([1, "two", null]),
    }
}

/// Four hostile keys, drawn deterministically. Identity slots are never
/// planted here — each profile decides what it asserts.
fn hostile_payload(rng: &mut Rng, names: &[String]) -> Map<String, Value> {
    let mut payload = Map::new();
    for _ in 0..4 {
        let name = rng.pick(names).clone();
        let value = hostile_value(rng.bits());
        payload.insert(name, value);
    }
    payload
}

fn is_envelope_slot(name: &str) -> bool {
    schema::ENVELOPE_TYPES
        .iter()
        .any(|(field, _)| *field == name)
}

/// The gates every door applies, asserted on one accepted event.
///
/// `origin` is the reproduction handle — profile, round and seed — so a
/// failing assertion is a complete bug report on its own.
fn assert_universal_gates(obj: &Map<String, Value>, kind: ProducerKind, origin: &str) {
    for key in obj.keys() {
        assert!(
            !schema::is_reserved_name(key) || is_envelope_slot(key),
            "{origin}: {key:?} survived into trawl's sealed `_` namespace"
        );
        assert!(
            key.len() <= MAX_FIELD_NAME_BYTES,
            "{origin}: {} is {} bytes — too long to be a catalog key",
            key.escape_debug(),
            key.len()
        );
        assert!(
            !key.bytes().any(|b| b.is_ascii_uppercase()),
            "{origin}: {key:?} reached storage unfolded"
        );
    }

    let raw = obj
        .get(schema::RAW)
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("{origin}: _raw must be a present string"));
    assert!(
        raw.chars().count() <= MAX_RAW_CHARS,
        "{origin}: _raw is {} chars, past the {MAX_RAW_CHARS} cap",
        raw.chars().count()
    );

    assert_eq!(
        obj.get(schema::PRODUCER).and_then(Value::as_str),
        Some(kind.as_str()),
        "{origin}: _producer must name the door that admitted the event"
    );
    for slot in [schema::TIME, schema::INGESTED, schema::ENV, schema::SERVICE] {
        assert!(obj.contains_key(slot), "{origin}: {slot} must be present");
    }
}

/// What a PROFILE asserted is what the event carries — the precedence
/// ruling, checked on every accepted event rather than on one example.
fn assert_profile_assertions(canonical: &Canonical, asserted: &Asserted<'_>, origin: &str) {
    assert_eq!(canonical.env, asserted.env, "{origin}: env assertion lost");
    assert_eq!(
        canonical.service, asserted.service,
        "{origin}: service assertion lost"
    );
    assert_eq!(
        canonical.obj[schema::ENV],
        json!(asserted.env),
        "{origin}: env column disagrees with the assertion"
    );
    assert_eq!(
        canonical.obj[schema::SERVICE],
        json!(asserted.service),
        "{origin}: service column disagrees with the assertion"
    );
    match asserted.host {
        Some(host) => assert_eq!(
            canonical.obj[schema::HOST],
            json!(host),
            "{origin}: host assertion lost"
        ),
        // Absence IS the assertion: a payload key must not supply what
        // the profile denied.
        None => assert!(
            !canonical.obj.contains_key(schema::HOST),
            "{origin}: a payload key supplied a host the profile denied"
        ),
    }
    if let Some(message) = asserted.message {
        assert_eq!(
            canonical.obj[schema::MESSAGE],
            json!(message),
            "{origin}: message assertion lost"
        );
    }
}

/// Accumulate the repair codes an accepted event confessed.
fn collect_repairs(obj: &Map<String, Value>, seen: &mut BTreeSet<String>) {
    if let Some(codes) = obj.get(schema::REPAIRS).and_then(Value::as_str) {
        seen.extend(codes.split(',').map(str::to_owned));
    }
}

/// The anti-vacuity half: a corpus that never provoked a gate would pass
/// every invariant above while proving nothing, so each profile states
/// the branches it must actually have reached.
fn assert_reached(seen: &BTreeSet<String>, required: &[&str], profile: &str) {
    for code in required {
        assert!(
            seen.contains(*code),
            "the {profile} corpus never reached {code} — the invariants above \
             are vacuous for that gate; saw {seen:?}"
        );
    }
}

fn arrival() -> (chrono::DateTime<chrono::Utc>, String) {
    let instant = chrono::Utc::now();
    let text = instant.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    (instant, text)
}

/// The HTTP door: a sender owns its own identity, so this is the only
/// profile that can REJECT — and the corpus plants the bad identities
/// deliberately, so "rejected iff planted" is an exact assertion rather
/// than a tolerance.
#[test]
fn the_http_profile_survives_a_hostile_corpus() {
    let derivation = Derivation::defaults();
    let envs = vec![ENV.to_owned()];
    let names = hostile_names();
    let mut rng = Rng::new(SEED);
    let mut accepted = 0_u64;
    let mut rejected = 0_u64;
    let mut seen = BTreeSet::new();

    for round in 0..ROUNDS {
        let origin = format!("http round {round} (seed {SEED:#018x})");
        let mut payload = hostile_payload(&mut rng, &names);
        // Every sixteenth event carries an identity no server can honour.
        let hostile_identity = round % 16 == 0;
        if hostile_identity {
            payload.insert(
                schema::SERVICE.into(),
                match round % 48 {
                    0 => json!("bad/service"),
                    16 => json!(42),
                    _ => json!(""),
                },
            );
        } else {
            payload.insert(schema::SERVICE.into(), json!("fuzz-http"));
        }
        payload.insert(schema::ENV.into(), json!(ENV));
        // Half the events omit `host` so the peer fill runs.
        if round % 2 == 0 {
            payload.insert(schema::HOST.into(), json!("web01"));
        }

        let (arrival_instant, arrival_text) = arrival();
        let ctx = EnvelopeContext {
            arrival: &arrival_text,
            arrival_instant,
            envs: &envs,
            default_env: ENV,
            producer: Producer::Http {
                peer_host: "10.0.4.55",
                peer_is_trusted_relay: false,
            },
            derivation: &derivation,
        };

        match canonicalize(&payload, &ctx) {
            Ok(canonical) => {
                assert!(
                    !hostile_identity,
                    "{origin}: an unusable service was accepted"
                );
                assert_universal_gates(&canonical.obj, ProducerKind::Http, &origin);
                assert_eq!(canonical.service, "fuzz-http", "{origin}");
                collect_repairs(&canonical.obj, &mut seen);
                accepted += 1;
            }
            Err((_, reason)) => {
                assert!(hostile_identity, "{origin}: rejected as {reason}");
                assert!(
                    RejectReason::ALL.contains(&reason),
                    "{origin}: {reason} is outside the closed reason set"
                );
                rejected += 1;
            }
        }
    }

    // Vacuity guards: an all-rejecting corpus would satisfy every
    // invariant above without proving a thing.
    assert_eq!(accepted + rejected, ROUNDS);
    assert!(accepted > ROUNDS / 2, "{accepted} accepted of {ROUNDS}");
    assert!(rejected > 0, "the reject path was never exercised");
    assert_reached(
        &seen,
        &[
            "field.reserved_prefix",
            "field.name_case_folded",
            "field.name_too_long",
            "field.truncated",
            "host.from_peer",
        ],
        "http",
    );
}

/// The syslog door, both halves of it.
///
/// Frames go through [`SyslogDoor::admit`] — the real transport path,
/// parser included — because that is where hostile PRI/timestamp/hostname
/// shapes live. Payload MAPS go straight at the door under
/// `Producer::Syslog`, because a wire frame cannot spell a `_HOSTNAME`
/// key and the sealed-namespace gate has to be proven for this profile
/// too.
#[test]
fn the_syslog_profile_survives_hostile_frames_and_payloads() {
    let derivation = Arc::new(Derivation::defaults());
    let door = SyslogDoor {
        envs: vec![ENV.to_owned()].into(),
        default_env: ENV.into(),
        trusted_relays: Vec::new().into(),
        derivation: Arc::clone(&derivation),
    };
    let source_service_map: HashMap<String, String> = HashMap::new();
    let mut rng = Rng::new(SEED);
    let mut seen = BTreeSet::new();

    for round in 0..ROUNDS {
        let origin = format!("syslog frame round {round} (seed {SEED:#018x})");
        let frame = hostile_frame(&mut rng, round);
        let event = door
            .admit(
                &frame,
                "10.0.0.7".parse().expect("a literal peer address"),
                &source_service_map,
                "syslog",
                "udp",
            )
            .unwrap_or_else(|| {
                panic!("{origin}: the syslog profile refused a frame it must salvage")
            });
        assert_universal_gates(&event.map, ProducerKind::Syslog, &origin);
        assert_eq!(event.env, ENV, "{origin}: the profile asserts its env");
        assert_eq!(
            event.map[schema::SERVICE],
            json!(event.service),
            "{origin}: service column disagrees with the batch key"
        );
        collect_repairs(&event.map, &mut seen);
    }
    assert_reached(
        &seen,
        &[
            // The three defects this issue fixes, provoked by the corpus:
            // an uncapped 64 KB datagram, a frame the profile has to
            // salvage a service for, and an absent frame timestamp that
            // used to be silently substituted.
            "field.truncated",
            "service.from_profile",
            "time.from_ingest",
            "host.from_peer",
        ],
        "syslog frame",
    );

    // The payload half: the same hostile keys every other door sees.
    let names = hostile_names();
    let mut rng = Rng::new(SEED ^ 0x5359_534c_4f47_0000);
    let mut seen = BTreeSet::new();
    for round in 0..ROUNDS {
        let origin = format!("syslog payload round {round} (seed {SEED:#018x})");
        let payload = hostile_payload(&mut rng, &names);
        let host = (round % 3 != 0).then_some("appliance-01");
        let asserted = Asserted {
            env: ENV,
            service: "syslog",
            host,
            message: Some("frame body"),
            repairs: &[],
        };
        let (arrival_instant, arrival_text) = arrival();
        let ctx = EnvelopeContext {
            arrival: &arrival_text,
            arrival_instant,
            envs: &[ENV.to_owned()],
            default_env: ENV,
            producer: Producer::Syslog(asserted),
            derivation: &derivation,
        };
        let canonical = canonicalize(&payload, &ctx)
            .unwrap_or_else(|(_, reason)| panic!("{origin}: refused as {reason}"));
        assert_universal_gates(&canonical.obj, ProducerKind::Syslog, &origin);
        assert_profile_assertions(&canonical, &asserted, &origin);
        collect_repairs(&canonical.obj, &mut seen);
    }
    assert_reached(
        &seen,
        &[
            "field.reserved_prefix",
            "field.name_too_long",
            "field.truncated",
            "host.omitted",
        ],
        "syslog payload",
    );
}

/// Frames a hostile or broken appliance can put on the wire: no PRI, no
/// timestamp, no hostname, an oversized datagram, structured data with
/// mixed-case and over-long keys, and plain RFC 3164 junk.
fn hostile_frame(rng: &mut Rng, round: u64) -> String {
    let pri = rng.bits() % 200;
    let sd = format!(
        "[exampleSDID@32473 eventID=\"{round}\" {}=\"over-long\"]",
        "L".repeat(usize::try_from(rng.bits() % 300).unwrap_or(0))
    );
    match round % 8 {
        0 => format!("<{pri}>1 2026-02-15T12:00:00Z host-{round} app {round} ID{round} {sd} body"),
        // No hostname, no APP-NAME: everything the profile has to salvage.
        1 => format!("<{pri}>1 2026-02-15T12:00:00Z - - - - - body {round}"),
        // No PRI at all — `syslog_severity` must be omitted, not defaulted.
        2 => format!("plain line with no framing at all, round {round}"),
        // An APP-NAME that fails the service charset.
        3 => format!("<{pri}>1 2026-02-15T12:00:00Z host bad/app - - - body"),
        // A 64 KB datagram: the `_raw` cap the listener never applied.
        4 => format!(
            "<{pri}>1 2026-02-15T12:00:00Z host app - - - {}",
            "A".repeat(70_000)
        ),
        // RFC 3164, the shape most appliances actually emit.
        5 => format!("<{pri}>Feb 15 12:00:00 host-{round} app[{round}]: body"),
        // Empty-ish and control-bearing bodies.
        6 => format!("<{pri}>1 2026-02-15T12:00:00Z host app - - - \u{0}\t"),
        _ => format!("<{pri}>1 - host-{round} app - - - body without a timestamp"),
    }
}

/// The `trawld` profile: rejection-free BY CONSTRUCTION (AC 3). Every
/// identity it asserts is boot-validated and there is nobody to reject
/// to, so a refusal here is a server bug — and the fuzz corpus is how
/// that claim stops being an argument.
#[test]
fn the_trawld_profile_never_rejects_a_hostile_payload() {
    let derivation = Derivation::defaults();
    let envs = vec![ENV.to_owned()];
    let names = hostile_names();
    let mut rng = Rng::new(SEED);
    let mut seen = BTreeSet::new();

    for round in 0..ROUNDS {
        let origin = format!("trawld round {round} (seed {SEED:#018x})");
        let mut payload = hostile_payload(&mut rng, &names);
        // Tracing call sites do carry these names — losing them to the
        // assertion (with the value findable in `_raw`) is the ruling.
        if round % 3 == 0 {
            payload.insert(schema::SERVICE.into(), json!("nginx"));
        }
        if round % 5 == 0 {
            payload.insert(schema::ENV.into(), json!("not-an-env"));
        }
        if round % 7 == 0 {
            payload.insert(schema::HOST.into(), json!("somewhere-else"));
        }
        if round % 11 == 0 {
            payload.insert(schema::MESSAGE.into(), json!("the payload IS the message"));
        }

        let asserted = Asserted {
            env: ENV,
            service: "trawld",
            // Alternate the resolved-hostname and failed-lookup shapes.
            host: (round % 2 == 0).then_some("box"),
            message: None,
            repairs: &[],
        };
        let (arrival_instant, arrival_text) = arrival();
        let ctx = EnvelopeContext {
            arrival: &arrival_text,
            arrival_instant,
            envs: &envs,
            default_env: ENV,
            producer: Producer::Trawld(asserted),
            derivation: &derivation,
        };

        let canonical = canonicalize(&payload, &ctx).unwrap_or_else(|(_, reason)| {
            panic!("{origin}: the trawld profile rejected as {reason} — it has nobody to reject to")
        });
        assert_universal_gates(&canonical.obj, ProducerKind::Trawld, &origin);
        assert_profile_assertions(&canonical, &asserted, &origin);
        // `message` is NOT asserted by this profile (the payload is the
        // message), so a payload one stands.
        if round % 11 == 0 {
            assert_eq!(
                canonical.obj[schema::MESSAGE],
                json!("the payload IS the message"),
                "{origin}"
            );
        }
        collect_repairs(&canonical.obj, &mut seen);
    }
    assert_reached(
        &seen,
        &[
            "field.producer_asserted",
            "field.reserved_prefix",
            // The compaction wedge this issue exists to close: a tracing
            // field name past the catalog's key bound.
            "field.name_too_long",
            "host.omitted",
        ],
        "trawld",
    );
}
