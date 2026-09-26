// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The trial's sample data: a seeded generator and the rule that decides
//! whether `up` may post it.
//!
//! [`generate`] is a pure function of a seed and an anchor time. The
//! number of events per service and per level is a constant, so a test
//! or a tutorial can state exact answers; the seed only moves timestamps
//! and picks hosts, messages, and durations. The PRNG is an in-crate
//! `SplitMix64`, because `rand`'s `StdRng` does not promise the same stream
//! across versions.
//!
//! Events carry the sender fields trawld derives from by default:
//! `timestamp` becomes `_time` and `level` becomes `_severity`
//! (`trawl_config::DEFAULT_TIME_FROM`, `DEFAULT_SEVERITY_FROM`).
//!
//! Time layout, for an anchor `A` truncated to whole milliseconds:
//!
//! - The bulk lies in `[A-24h, A-10m)`.
//! - A fixed recent slice lies in `[A-10m, A)`: every generated service,
//!   web warn and error events, and error events in several services, so
//!   each quick-start example returns rows under the browser's default
//!   15-minute range.
//! - The three `service=tutorial` events that the query tutorial uses sit
//!   at exactly `A`, the newest sample timestamp.
//!
//! Ingest has no idempotency key. [`decide_samples`] therefore never posts
//! over a count it cannot explain, and trawld's own self-telemetry, which
//! is on in a trial, never enters that decision because only the sample
//! services are compared.

use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, SubsecRound as _, TimeDelta, Utc};
use serde_json::{Value, json};

use super::state::Samples;

/// The seed `up` uses.
pub const SAMPLE_SEED: u64 = 203;

/// Every generated service puts events in `[anchor - RECENT, anchor)`.
const RECENT: TimeDelta = TimeDelta::minutes(10);

/// The oldest sample is no older than `anchor - SPAN`.
const SPAN: TimeDelta = TimeDelta::hours(24);

/// Event counts by level for one service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelCounts {
    pub debug: u64,
    pub info: u64,
    pub warn: u64,
    pub error: u64,
}

impl LevelCounts {
    const fn total(self) -> u64 {
        self.debug + self.info + self.warn + self.error
    }

    /// `(level, count)` in a fixed order: the generator's draw order.
    const fn by_level(self) -> [(Level, u64); 4] {
        [
            (Level::Debug, self.debug),
            (Level::Info, self.info),
            (Level::Warn, self.warn),
            (Level::Error, self.error),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    /// Inclusive duration range, in milliseconds.
    const fn durations(self) -> (u64, u64) {
        match self {
            Self::Debug => (1, 20),
            Self::Info => (5, 400),
            Self::Warn => (300, 2_000),
            Self::Error => (500, 5_000),
        }
    }
}

/// One generated service: its fixed counts and what the seed picks from.
#[derive(Debug)]
pub struct ServicePlan {
    pub name: &'static str,
    /// All events, the recent slice included.
    pub counts: LevelCounts,
    /// The part of `counts` that lies in the last 10 minutes.
    pub recent: LevelCounts,
    hosts: &'static [&'static str],
    /// Messages for debug, info, warn, and error, in that order.
    messages: [&'static [&'static str]; 4],
}

/// The generated services. With the three tutorial events they sum to
/// exactly 2000.
pub const SERVICES: [ServicePlan; 5] = [
    ServicePlan {
        name: "web",
        counts: LevelCounts {
            debug: 70,
            info: 520,
            warn: 80,
            error: 30,
        },
        recent: LevelCounts {
            debug: 1,
            info: 6,
            warn: 2,
            error: 1,
        },
        hosts: &["web-1", "web-2", "web-3"],
        messages: [
            &["cache lookup", "template rendered"],
            &["GET /", "GET /products", "POST /cart", "GET /static/app.js"],
            &["slow response", "upstream retry"],
            &["upstream returned 502", "request timed out"],
        ],
    },
    ServicePlan {
        name: "api",
        counts: LevelCounts {
            debug: 60,
            info: 370,
            warn: 50,
            error: 20,
        },
        recent: LevelCounts {
            debug: 1,
            info: 4,
            warn: 1,
            error: 1,
        },
        hosts: &["api-1", "api-2"],
        messages: [
            &["query planned", "cache hit"],
            &["request handled", "token refreshed", "list products"],
            &["rate limit near", "slow query"],
            &["database pool exhausted", "handler panicked"],
        ],
    },
    ServicePlan {
        name: "checkout",
        counts: LevelCounts {
            debug: 20,
            info: 230,
            warn: 30,
            error: 20,
        },
        recent: LevelCounts {
            debug: 0,
            info: 3,
            warn: 1,
            error: 1,
        },
        hosts: &["checkout-1"],
        messages: [
            &["basket priced"],
            &["order placed", "payment authorized", "receipt sent"],
            &["payment retry", "inventory low"],
            &["payment declined by provider", "order write failed"],
        ],
    },
    ServicePlan {
        name: "auth",
        counts: LevelCounts {
            debug: 25,
            info: 190,
            warn: 25,
            error: 10,
        },
        recent: LevelCounts {
            debug: 0,
            info: 3,
            warn: 1,
            error: 0,
        },
        hosts: &["auth-1", "auth-2"],
        messages: [
            &["session lookup"],
            &["login succeeded", "session renewed", "logout"],
            &["login failed", "password reset requested"],
            &["key store unreachable"],
        ],
    },
    ServicePlan {
        name: "worker",
        counts: LevelCounts {
            debug: 30,
            info: 180,
            warn: 25,
            error: 12,
        },
        recent: LevelCounts {
            debug: 0,
            info: 2,
            warn: 0,
            error: 0,
        },
        hosts: &["worker-1", "worker-2"],
        messages: [
            &["job dequeued"],
            &["job finished", "report generated", "email batch sent"],
            &["job retried", "queue depth high"],
            &["job failed after retries"],
        ],
    },
];

/// The service of the query tutorial's events.
pub const TUTORIAL_SERVICE: &str = "tutorial";

const TUTORIAL_HOST: &str = "tutorial-host";

/// `(level, message, duration)` of the three events that
/// `use/query-tutorial.md` queries, as `getting-started/first-query.md`
/// used to send them.
const TUTORIAL_EVENTS: [(&str, &str, u64); 3] = [
    ("info", "server started", 12),
    ("error", "connection refused", 1500),
    ("warn", "upstream timeout", 700),
];

/// The four examples of the browser's Search quick start, verbatim.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "tests hold the recent slice against them")
)]
pub const QUICK_START_QUERIES: [&str; 4] = [
    "* | head 20",
    "* | stats count() by service",
    "_severity>=error | stats count() as errors by service | sort -errors | head 10",
    "service=web _severity>=warn | timechart span=5m count()",
];

/// The service the documented query counts.
const DOCUMENTED_SERVICE: &str = "checkout";

/// The query the trial tutorial runs. It has no time clause: `trawl query`
/// sends none of its own, so it counts every stored sample and gives the
/// same row for the trial's whole life. Naming one sample service keeps
/// trawld's self-telemetry out of it.
pub const DOCUMENTED_QUERY: &str =
    "service=checkout _severity>=error | stats count() as errors by service";

/// The one row [`DOCUMENTED_QUERY`] returns once the samples are complete.
pub fn documented_row() -> Value {
    let plan = SERVICES
        .iter()
        .find(|plan| plan.name == DOCUMENTED_SERVICE)
        .expect("the documented service is a generated service");
    json!({ "service": DOCUMENTED_SERVICE, "errors": plan.counts.error })
}

/// A generated sample set.
#[derive(Debug, Clone, PartialEq)]
pub struct SampleSet {
    /// One JSON object per event, oldest first.
    pub events: Vec<Value>,
    /// The oldest event's time.
    pub first: DateTime<Utc>,
    /// The newest event's time: the anchor, truncated to milliseconds.
    pub last: DateTime<Utc>,
}

/// Events per sample service. It does not depend on the seed.
pub fn expected_counts() -> BTreeMap<&'static str, u64> {
    let mut counts: BTreeMap<&'static str, u64> = SERVICES
        .iter()
        .map(|plan| (plan.name, plan.counts.total()))
        .collect();
    counts.insert(TUTORIAL_SERVICE, TUTORIAL_EVENTS.len() as u64);
    counts
}

/// Generate the sample set for `seed`, ending at `anchor`.
///
/// The same `(seed, anchor)` gives the same events, byte for byte. The
/// anchor is truncated to whole milliseconds, the precision of every
/// sample timestamp, so an anchor read back from `state.json` generates
/// the same set.
pub fn generate(seed: u64, anchor: DateTime<Utc>) -> SampleSet {
    let anchor = anchor.trunc_subsecs(3);
    let mut rng = SplitMix64(seed);
    let recent_ms = ms(RECENT);
    let span_ms = ms(SPAN);
    let mut timed: Vec<(DateTime<Utc>, Value)> = Vec::with_capacity(2000);

    for plan in &SERVICES {
        for ((level, total), (_, recent)) in plan
            .counts
            .by_level()
            .into_iter()
            .zip(plan.recent.by_level())
        {
            let messages = plan.messages[level as usize];
            let (low, high) = level.durations();
            for i in 0..total {
                // Recent: [A-10m, A-1ms]. Bulk: [A-24h, A-10m-1ms].
                let back = if i < recent {
                    1 + rng.below(recent_ms)
                } else {
                    recent_ms + 1 + rng.below(span_ms - recent_ms)
                };
                let host = pick(&mut rng, plan.hosts);
                let message = pick(&mut rng, messages);
                let duration = low + rng.below(high - low + 1);
                let time = anchor - TimeDelta::milliseconds(back.cast_signed());
                timed.push((
                    time,
                    event(plan.name, host, level.as_str(), message, duration, time),
                ));
            }
        }
    }
    for (level, message, duration) in TUTORIAL_EVENTS {
        timed.push((
            anchor,
            event(
                TUTORIAL_SERVICE,
                TUTORIAL_HOST,
                level,
                message,
                duration,
                anchor,
            ),
        ));
    }

    // Stable: equal times keep their generation order.
    timed.sort_by_key(|(time, _)| *time);
    let first = timed.first().map_or(anchor, |(time, _)| *time);
    SampleSet {
        events: timed.into_iter().map(|(_, event)| event).collect(),
        first,
        last: anchor,
    }
}

fn event(
    service: &str,
    host: &str,
    level: &str,
    message: &str,
    duration: u64,
    time: DateTime<Utc>,
) -> Value {
    json!({
        "service": service,
        "host": host,
        "level": level,
        "message": message,
        "duration": duration,
        "timestamp": time.to_rfc3339_opts(SecondsFormat::Millis, true),
    })
}

fn ms(delta: TimeDelta) -> u64 {
    delta
        .num_milliseconds()
        .try_into()
        .expect("a positive constant")
}

fn pick<'a>(rng: &mut SplitMix64, from: &[&'a str]) -> &'a str {
    from[usize::try_from(rng.below(from.len() as u64)).expect("a slice index fits usize")]
}

/// `SplitMix64` (Steele, Lea, and Flood), as in `java.util.SplittableRandom`.
#[derive(Debug)]
struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..n`, by multiply-shift. `n` is never 0 here.
    fn below(&mut self, n: u64) -> u64 {
        let wide = u128::from(self.next()) * u128::from(n);
        u64::try_from(wide >> 64).expect("the high half of a u128 fits u64")
    }
}

/// What `up` does about sample data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleAction {
    /// Record `Intent`, then post the samples once.
    Post,
    /// A post with an unverified result landed exactly: record `Complete`.
    MarkComplete,
    /// Nothing to post. When the state was `NotRequested` and samples were
    /// not requested, `up` records `Skipped`.
    Skip,
    /// The sample services hold events `up` cannot account for. Posting
    /// could duplicate them, so `up` stops with recovery text.
    Refuse {
        /// Observed events per sample service, zeros included.
        observed: BTreeMap<String, u64>,
    },
}

/// Decide what `up` does about samples, from the recorded state, whether
/// this `up` asked for samples, and the observed per-service counts.
///
/// `observed` may carry services other than the sample services, such as
/// trawld's self-telemetry; those are ignored, and a missing sample
/// service counts as zero.
///
/// - `Complete`: skip.
/// - `Intent`: exactly the recorded expected counts means the earlier post
///   landed, so mark it complete. Anything else refuses, and never posts
///   again; with `--no-sample-data` it is left as it is.
/// - `NotRequested` or `Skipped`: post when samples are requested and
///   every sample service is empty, refuse when one is not, and skip when
///   samples are not requested.
pub fn decide_samples(
    state: &Samples,
    requested: bool,
    observed: &BTreeMap<String, u64>,
) -> SampleAction {
    let observed = sample_counts(observed);
    match state {
        Samples::Complete { .. } => SampleAction::Skip,
        Samples::Intent { expected, .. } => {
            let exact = observed
                .iter()
                .all(|(service, count)| expected.get(service).copied().unwrap_or(0) == *count)
                && expected
                    .iter()
                    .all(|(service, count)| observed.get(service).copied().unwrap_or(0) == *count);
            if exact {
                SampleAction::MarkComplete
            } else if requested {
                SampleAction::Refuse { observed }
            } else {
                SampleAction::Skip
            }
        }
        Samples::NotRequested | Samples::Skipped => {
            if !requested {
                SampleAction::Skip
            } else if observed.values().all(|count| *count == 0) {
                SampleAction::Post
            } else {
                SampleAction::Refuse { observed }
            }
        }
    }
}

/// The observed count of every sample service, zero when absent.
fn sample_counts(observed: &BTreeMap<String, u64>) -> BTreeMap<String, u64> {
    expected_counts()
        .into_keys()
        .map(|service| {
            (
                service.to_owned(),
                observed.get(service).copied().unwrap_or(0),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn anchor() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-25T12:00:00.123456Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn time_of(event: &Value) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(event["timestamp"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc)
    }

    /// `_severity` as trawld derives it from `level`.
    fn severity(event: &Value) -> u8 {
        trawl_core::severity::number_for_token(event["level"].as_str().unwrap())
            .expect("every sample level is a severity token")
    }

    fn service(event: &Value) -> &str {
        event["service"].as_str().unwrap()
    }

    fn count_by<'a>(
        events: &'a [Value],
        key: impl Fn(&'a Value) -> String,
    ) -> BTreeMap<String, u64> {
        let mut counts = BTreeMap::new();
        for event in events {
            *counts.entry(key(event)).or_insert(0) += 1;
        }
        counts
    }

    fn ndjson(events: &[Value]) -> Vec<u8> {
        let mut body = Vec::new();
        for event in events {
            serde_json::to_writer(&mut body, event).unwrap();
            body.push(b'\n');
        }
        body
    }

    /// The reference stream from the `SplitMix64` paper's C code, seed 0.
    #[test]
    fn splitmix64_matches_the_reference_stream() {
        let mut rng = SplitMix64(0);
        assert_eq!(rng.next(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next(), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(rng.next(), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn the_same_seed_and_anchor_give_identical_events() {
        let a = generate(SAMPLE_SEED, anchor());
        let b = generate(SAMPLE_SEED, anchor());
        assert_eq!(ndjson(&a.events), ndjson(&b.events));
        assert_eq!((a.first, a.last), (b.first, b.last));
    }

    #[test]
    fn a_different_seed_changes_events_but_not_counts() {
        let a = generate(SAMPLE_SEED, anchor());
        let b = generate(SAMPLE_SEED + 1, anchor());
        assert_ne!(ndjson(&a.events), ndjson(&b.events));

        let by_service_level =
            |e: &Value| format!("{}/{}", service(e), e["level"].as_str().unwrap());
        assert_eq!(
            count_by(&a.events, by_service_level),
            count_by(&b.events, by_service_level)
        );
        assert_eq!(
            count_by(&a.events, |e| service(e).to_owned()),
            count_by(&b.events, |e| service(e).to_owned())
        );
    }

    #[test]
    fn counts_sum_to_2000_and_match_expected_counts() {
        let expected = expected_counts();
        assert_eq!(expected.values().sum::<u64>(), 2000);
        assert!(expected.contains_key("web"));
        for plan in &SERVICES {
            let fits = plan
                .recent
                .by_level()
                .into_iter()
                .zip(plan.counts.by_level())
                .all(|((_, recent), (_, total))| recent <= total);
            assert!(fits, "{}: the recent slice is part of the total", plan.name);
        }

        for seed in [0, 1, SAMPLE_SEED, u64::MAX] {
            let set = generate(seed, anchor());
            assert_eq!(set.events.len(), 2000);
            let observed = count_by(&set.events, |e| service(e).to_owned());
            let expected: BTreeMap<String, u64> = expected
                .iter()
                .map(|(service, count)| ((*service).to_owned(), *count))
                .collect();
            assert_eq!(observed, expected, "seed {seed}");
        }
    }

    #[test]
    fn events_lie_in_the_day_before_the_anchor_oldest_first() {
        let set = generate(SAMPLE_SEED, anchor());
        let anchor = anchor().trunc_subsecs(3);
        assert_eq!(set.last, anchor);
        assert!(set.first >= anchor - SPAN, "{}", set.first);
        assert!(
            set.first < anchor - RECENT,
            "the bulk reaches back past the recent slice"
        );

        let times: Vec<_> = set.events.iter().map(time_of).collect();
        assert!(times.is_sorted(), "oldest first");
        assert_eq!(times.first(), Some(&set.first));
        assert_eq!(times.last(), Some(&set.last));
        assert!(times.iter().all(|t| *t >= set.first && *t <= set.last));

        // A stored anchor reproduces the set.
        let stored = set.last.to_rfc3339_opts(SecondsFormat::Millis, true);
        let reread = DateTime::parse_from_rfc3339(&stored)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(generate(SAMPLE_SEED, reread), set);
    }

    #[test]
    fn events_carry_the_fields_ingest_derives_from() {
        let set = generate(SAMPLE_SEED, anchor());
        for event in &set.events {
            let fields: BTreeSet<&str> = event
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            assert_eq!(
                fields,
                BTreeSet::from([
                    "duration",
                    "host",
                    "level",
                    "message",
                    "service",
                    "timestamp"
                ]),
            );
            assert!(event["duration"].is_u64());
            severity(event);
        }
        assert!(trawl_config::DEFAULT_TIME_FROM.contains(&"timestamp"));
        assert!(trawl_config::DEFAULT_SEVERITY_FROM.contains(&"level"));
    }

    /// The recent slice holds exactly its constants for every seed, and
    /// holds what each quick-start example needs under a 15-minute range.
    #[test]
    fn the_recent_slice_answers_every_quick_start_example() {
        for seed in [0, SAMPLE_SEED, u64::MAX] {
            let set = generate(seed, anchor());
            let cutoff = set.last - RECENT;
            let recent: Vec<&Value> = set.events.iter().filter(|e| time_of(e) >= cutoff).collect();

            let mut want = 0;
            for plan in &SERVICES {
                for (level, count) in plan.recent.by_level() {
                    let got = recent
                        .iter()
                        .filter(|e| service(e) == plan.name && e["level"] == level.as_str())
                        .count() as u64;
                    assert_eq!(got, count, "seed {seed}: {} {}", plan.name, level.as_str());
                    want += count;
                }
            }
            assert_eq!(recent.len() as u64, want + TUTORIAL_EVENTS.len() as u64);

            // `* | head 20`
            assert!(!recent.is_empty());
            // `* | stats count() by service`: every sample service.
            let services: BTreeSet<&str> = recent.iter().map(|e| service(e)).collect();
            assert_eq!(services, expected_counts().into_keys().collect());
            // `_severity>=error | stats count() as errors by service ...`
            let erroring: BTreeSet<&str> = recent
                .iter()
                .filter(|e| severity(e) >= trawl_core::severity::ERROR_BAND.0)
                .map(|e| service(e))
                .collect();
            assert!(erroring.len() >= 2, "{erroring:?}");
            // `service=web _severity>=warn | timechart ...`: warn and error.
            let web: BTreeSet<u8> = recent
                .iter()
                .filter(|e| service(e) == "web")
                .map(|e| severity(e))
                .filter(|s| *s >= trawl_core::severity::WARN_BAND.0)
                .collect();
            assert!(web.contains(&trawl_core::severity::WARN_BAND.0), "{web:?}");
            assert!(web.contains(&trawl_core::severity::ERROR_BAND.0), "{web:?}");
        }
    }

    /// The query tutorial's three events, at one timestamp.
    #[test]
    fn the_tutorial_events_match_the_query_tutorial() {
        let set = generate(SAMPLE_SEED, anchor());
        let tutorial: Vec<&Value> = set
            .events
            .iter()
            .filter(|e| service(e) == TUTORIAL_SERVICE)
            .collect();
        let stamp = set.last.to_rfc3339_opts(SecondsFormat::Millis, true);
        assert_eq!(
            tutorial,
            [
                &json!({"service": "tutorial", "host": "tutorial-host", "level": "info", "message": "server started", "duration": 12, "timestamp": stamp}),
                &json!({"service": "tutorial", "host": "tutorial-host", "level": "error", "message": "connection refused", "duration": 1500, "timestamp": stamp}),
                &json!({"service": "tutorial", "host": "tutorial-host", "level": "warn", "message": "upstream timeout", "duration": 700, "timestamp": stamp}),
            ]
        );
    }

    #[test]
    fn quick_start_queries_are_the_browsers_literals() {
        let source = include_str!("../../../trawl-web-ui/src/components/search_quick_start.rs");
        for query in QUICK_START_QUERIES {
            assert!(source.contains(&format!("\"{query}\"")), "{query}");
            trawl_core::parser::parse(query).expect("the example parses");
        }
    }

    /// The documented query has no time clause, so its row does not age,
    /// and the row is what the generated events imply.
    #[test]
    fn the_documented_row_is_what_the_counts_imply() {
        let query =
            trawl_core::parser::parse(DOCUMENTED_QUERY).expect("the documented query parses");
        assert_eq!(query.search.time_clause(), None);

        for seed in [0, SAMPLE_SEED] {
            let set = generate(seed, anchor());
            let errors = set
                .events
                .iter()
                .filter(|e| service(e) == DOCUMENTED_SERVICE)
                .filter(|e| severity(e) >= trawl_core::severity::ERROR_BAND.0)
                .count();
            assert_eq!(
                documented_row(),
                json!({ "service": DOCUMENTED_SERVICE, "errors": errors })
            );
        }
        assert_eq!(
            documented_row(),
            json!({"service": "checkout", "errors": 20})
        );
    }

    #[test]
    fn the_posted_body_is_well_under_the_ingest_limit() {
        let body = ndjson(&generate(SAMPLE_SEED, anchor()).events);
        let limit = trawl_config::DEFAULT_INGEST_MAX_BODY_BYTES;
        assert!(
            body.len() < limit / 16,
            "{} bytes against {limit}",
            body.len()
        );
    }

    #[test]
    fn the_first_events_for_a_fixed_seed_and_anchor() {
        let set = generate(SAMPLE_SEED, anchor());
        insta::assert_snapshot!(serde_json::to_string_pretty(&set.events[..5]).unwrap());
    }

    type Counts = BTreeMap<String, u64>;

    /// One `decide_samples` row: name, state, `requested`, observed, want.
    type Case<'a> = (&'a str, &'a Samples, bool, Counts, SampleAction);

    fn expected_owned() -> Counts {
        expected_counts()
            .into_iter()
            .map(|(service, count)| (service.to_owned(), count))
            .collect()
    }

    fn with(base: &Counts, service: &str, count: u64) -> Counts {
        let mut counts = base.clone();
        counts.insert(service.to_owned(), count);
        counts
    }

    fn refuse(observed: &Counts) -> SampleAction {
        SampleAction::Refuse {
            observed: sample_counts(observed),
        }
    }

    fn check(cases: Vec<Case<'_>>) {
        for (name, state, requested, observed, want) in cases {
            assert_eq!(decide_samples(state, requested, &observed), want, "{name}");
        }
    }

    /// Self-telemetry, which a trial always has.
    fn telemetry() -> Counts {
        BTreeMap::from([("trawld".to_owned(), 41)])
    }

    #[test]
    fn decide_samples_before_any_post() {
        use SampleAction::{Post, Skip};
        let none = Counts::new();
        let zeros: Counts = expected_owned().into_keys().map(|s| (s, 0)).collect();
        let stray = with(&telemetry(), "checkout", 1);
        let (fresh, skipped) = (&Samples::NotRequested, &Samples::Skipped);
        check(vec![
            ("fresh, nothing", fresh, true, none.clone(), Post),
            ("fresh, zeros", fresh, true, zeros, Post),
            ("fresh, self-telemetry", fresh, true, telemetry(), Post),
            ("fresh, stray", fresh, true, stray.clone(), refuse(&stray)),
            ("fresh, not requested", fresh, false, none.clone(), Skip),
            ("skipped, requested", skipped, true, telemetry(), Post),
            (
                "skipped, stray",
                skipped,
                true,
                stray.clone(),
                refuse(&stray),
            ),
            ("skipped, not requested", skipped, false, none, Skip),
        ]);
    }

    #[test]
    fn decide_samples_after_an_unverified_post_never_posts() {
        use SampleAction::{MarkComplete, Skip};
        let expected = expected_owned();
        let intent = &Samples::Intent {
            seed: SAMPLE_SEED,
            anchor: "2026-09-25T12:00:00.123Z".into(),
            expected: expected.clone(),
        };
        let none = Counts::new();
        let beside = with(&expected, "trawld", 41);
        let partial = with(&expected, "web", 350);
        let doubled: Counts = expected.iter().map(|(s, c)| (s.clone(), c * 2)).collect();
        check(vec![
            ("exact", intent, true, expected.clone(), MarkComplete),
            ("exact beside telemetry", intent, true, beside, MarkComplete),
            (
                "exact, not requested",
                intent,
                false,
                expected,
                MarkComplete,
            ),
            ("nothing landed", intent, true, none.clone(), refuse(&none)),
            ("telemetry only", intent, true, telemetry(), refuse(&none)),
            ("partial", intent, true, partial.clone(), refuse(&partial)),
            ("doubled", intent, true, doubled.clone(), refuse(&doubled)),
            ("partial, not requested", intent, false, partial, Skip),
            ("nothing, not requested", intent, false, none, Skip),
        ]);
    }

    #[test]
    fn decide_samples_once_complete_skips() {
        let complete = &Samples::Complete {
            seed: SAMPLE_SEED,
            first: "2026-09-24T12:05:00.000Z".into(),
            last: "2026-09-25T12:00:00.123Z".into(),
            total: 2000,
        };
        let moved: Counts = expected_owned().into_keys().map(|s| (s, 7)).collect();
        check(vec![
            (
                "verified",
                complete,
                true,
                expected_owned(),
                SampleAction::Skip,
            ),
            ("counts moved", complete, true, moved, SampleAction::Skip),
            (
                "not requested",
                complete,
                false,
                Counts::new(),
                SampleAction::Skip,
            ),
        ]);
    }

    /// A refusal reports every sample service and nothing else, so its
    /// recovery text never quotes self-telemetry.
    #[test]
    fn a_refusal_reports_only_sample_services() {
        let observed = BTreeMap::from([("web".to_owned(), 3), ("trawld".to_owned(), 9)]);
        let SampleAction::Refuse { observed } =
            decide_samples(&Samples::NotRequested, true, &observed)
        else {
            panic!("a non-empty sample service refuses");
        };
        assert_eq!(
            observed.keys().map(String::as_str).collect::<Vec<_>>(),
            expected_counts().into_keys().collect::<Vec<_>>()
        );
        assert_eq!(observed["web"], 3);
        assert_eq!(observed["api"], 0);
    }
}
