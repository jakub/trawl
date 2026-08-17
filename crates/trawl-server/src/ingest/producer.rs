// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Producer profiles and the derivation policy (ADR-0013 slice 2,
//! rulings 2 and 5).
//!
//! **Profiles are code, not data.** The set is closed — `http`, `syslog`,
//! `trawld` — and a profile is chosen by the server call site that owns
//! the transport, never by anything on the wire. A profile ASSERTS the
//! identity it can prove (`env` from boot-validated config, `service`
//! from the listener's own derivation or the fixed `trawld`, `host` per
//! transport) and contributes FIXED derivation sources. It bypasses no
//! universal gate: the fold, the name-length drop, the sealed-prefix
//! strip, nested stringification, the `_raw` cap and `_repairs` assembly
//! all belong to the one canonicalizer and apply to all three doors.
//!
//! **Derivation sources are data.** `_severity` and `_time` each read an
//! ordered source list — configured globally under `[ingest]`, with each
//! profile's fixed sources PREPENDED and not configurable. That is what
//! makes provenance-licensed syslog inversion a config fact rather than a
//! privileged writer: the native listener and a syslog-over-HTTP
//! forwarder reach the same dialect through the same mechanism.
//!
//! Nothing here calls `tracing`. The telemetry producer's own events pass
//! through this module on their way to the WAL, so a log line emitted
//! from inside it is an ingestion loop (ruling 4); failure paths are
//! metrics only.

use trawl_config::{DerivationSourceSpec, IngestConfig};
use trawl_core::schema::{self, MAX_FIELD_NAME_BYTES};
use trawl_core::severity::{DIALECT_TOKENS, Dialect};

use crate::ingest::envelope::{RejectReason, RepairCode};

/// Which door an event entered through — the `_producer` column's closed
/// vocabulary (ADR-0013 slice 2, ruling 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProducerKind {
    /// `POST /api/v1/ingest`.
    Http,
    /// The syslog listener (UDP/TCP).
    Syslog,
    /// Internal telemetry — trawld observing trawld.
    Trawld,
}

impl ProducerKind {
    /// Every profile, for exhaustive iteration (metric zero-init, tests).
    pub const ALL: &'static [Self] = &[Self::Http, Self::Syslog, Self::Trawld];

    /// The one spelling: the `_producer` column VALUE and the metric
    /// label alike, so a query and a dashboard never disagree about what
    /// to call a door.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Syslog => "syslog",
            Self::Trawld => "trawld",
        }
    }

    /// Dense index into [`Derivation`]'s per-kind tables.
    const fn index(self) -> usize {
        match self {
            Self::Http => 0,
            Self::Syslog => 1,
            Self::Trawld => 2,
        }
    }
}

/// The syslog listener's severity artifact: the RAW PRI severity numeral,
/// 0-7, published as an ordinary sender-visible column.
///
/// The listener does not write `_severity` (ruling 1) — it publishes what
/// it parsed and the syslog profile's fixed source reads it back with
/// `dialect = "syslog"`, so the inversion is licensed by where the config
/// lives rather than by a privileged writer.
pub const SYSLOG_SEVERITY_FIELD: &str = "syslog_severity";

/// The syslog listener's timestamp artifact, published as an ordinary
/// column and read as the syslog profile's first `_time` source.
pub const SYSLOG_TIMESTAMP_FIELD: &str = "syslog_timestamp";

/// Maximum entries in ONE configured derivation list.
///
/// Derivation runs per event on the ingest hot path and scans the list
/// until something matches, so its length is a per-event cost paid by
/// every sender. Eight is far past any honest source chain (the packaged
/// defaults are three) and small enough that the scan stays free. Fixed
/// profile sources do not consume slots — they are trawl's, not the
/// operator's.
pub const MAX_DERIVATION_SOURCES: usize = 8;

/// What a profile ASSERTS about an event before the door sees it.
///
/// An assertion is not a repair (ruling 2): the producer is the authority
/// on these values, so stamping them is ordinary identity, not a fix. A
/// payload key that collides with one loses — `field.producer_asserted` —
/// and its value stays findable in `_raw`.
#[derive(Clone, Copy, Debug)]
pub struct Asserted<'a> {
    /// Boot-validated, so `env.defaulted` can never fire for a profile
    /// producer and a per-event env failure is a server bug, not a
    /// sender's.
    pub env: &'a str,
    /// The listener's derived service, or the literal `trawld`.
    pub service: &'a str,
    /// `None` means keep the event and OMIT `host` (`host.omitted`) —
    /// absent-but-honest beats both the peer-fill lie and the drop
    /// (ruling 4). The only paths there are a hostname-less frame behind
    /// a trusted relay and a failed hostname lookup for trawld.
    pub host: Option<&'a str>,
    /// The parsed message body, where the transport separates one from
    /// its metadata (syslog's `msg.msg`); `None` when the payload IS the
    /// message.
    pub message: Option<&'a str>,
    /// Codes the PRODUCER contributed (`host.from_peer`,
    /// `service.from_profile`). Producers contribute codes; only the door
    /// assembles `_repairs`.
    pub repairs: &'a [RepairCode],
}

/// Which door an event arrived at, with whatever that door can assert.
#[derive(Clone, Copy, Debug)]
pub enum Producer<'a> {
    /// The HTTP door asserts nothing: an HTTP sender owns its own
    /// identity fields and can be REJECTED and told to resend, so the
    /// peer is evidence for a fill, not an assertion.
    Http {
        /// Peer IP as a string (from the TCP connection).
        peer_host: &'a str,
        /// Whether the peer sits inside a configured `trusted_relays`
        /// CIDR: a missing `host` is then rejected instead of repaired,
        /// because behind a relay the peer address is confidently wrong.
        peer_is_trusted_relay: bool,
    },
    /// The syslog listener.
    Syslog(Asserted<'a>),
    /// Internal telemetry.
    Trawld(Asserted<'a>),
}

impl Producer<'_> {
    /// The profile this producer is, for the `_producer` stamp.
    pub const fn kind(&self) -> ProducerKind {
        match self {
            Self::Http { .. } => ProducerKind::Http,
            Self::Syslog(_) => ProducerKind::Syslog,
            Self::Trawld(_) => ProducerKind::Trawld,
        }
    }

    /// The profile's assertions, or `None` for the HTTP door.
    pub const fn asserted(&self) -> Option<&Asserted<'_>> {
        match self {
            Self::Http { .. } => None,
            Self::Syslog(a) | Self::Trawld(a) => Some(a),
        }
    }
}

/// One resolved derivation source: a wire key and the dialect its
/// NUMERICS read in (words always go through the one token table).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The wire key to read. Already validated bare, folded and storable.
    pub field: String,
    /// Governs numerics only; irrelevant on a `_time` source, where the
    /// config refuses to spell one at all.
    pub dialect: Dialect,
}

impl Source {
    /// A source reading `OTel` numerics — the default everywhere the
    /// operator did not assert other provenance.
    fn otel(field: &str) -> Self {
        Self {
            field: field.to_owned(),
            dialect: Dialect::Otel,
        }
    }
}

/// The resolved, per-profile derivation policy: what `_severity` and
/// `_time` read, in order, at each door.
///
/// Built once at boot (boot-FATAL on a bad config — see [`Self::resolve`])
/// and threaded to every producer, so the lists cannot drift between
/// doors and a config change is forward-only by construction: there is no
/// policy history, and nothing re-reads a stored event's derivation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Derivation {
    /// Indexed by [`ProducerKind::index`].
    severity: [Vec<Source>; 3],
    time: [Vec<Source>; 3],
}

/// The fixed severity sources a profile contributes, prepended to the
/// configured list and not configurable.
fn fixed_severity_sources(kind: ProducerKind) -> Vec<Source> {
    match kind {
        // Provenance proves the dialect: the listener knows its transport,
        // so the 0-7 numeral it published inverts, and only there.
        ProducerKind::Syslog => vec![Source {
            field: SYSLOG_SEVERITY_FIELD.to_owned(),
            dialect: Dialect::Syslog,
        }],
        // Telemetry is an ordinary sender (ruling 3): its bare `level`
        // rides the configured chain like any app's. The HTTP door has no
        // transport evidence at all.
        ProducerKind::Http | ProducerKind::Trawld => Vec::new(),
    }
}

/// The fixed time sources a profile contributes.
fn fixed_time_sources(kind: ProducerKind) -> Vec<Source> {
    match kind {
        // The frame's own timestamp, published as an ordinary column. It
        // is OMITTED when the frame carried none or an unparseable one,
        // so the configured chain — and ultimately arrival time with
        // `time.from_ingest` — takes over honestly.
        ProducerKind::Syslog => vec![Source::otel(SYSLOG_TIMESTAMP_FIELD)],
        // Telemetry proposes `_time` in its payload, which the configured
        // list already reads first.
        ProducerKind::Http | ProducerKind::Trawld => Vec::new(),
    }
}

/// Fixed sources followed by the configured ones, first spelling of a
/// field winning — so a profile's fixed source can never be displaced,
/// shadowed or duplicated by config.
fn effective(fixed: Vec<Source>, configured: &[Source]) -> Vec<Source> {
    let mut out = fixed;
    for source in configured {
        if !out.iter().any(|s| s.field == source.field) {
            out.push(source.clone());
        }
    }
    out
}

impl Derivation {
    /// Resolve and VALIDATE the configured lists — boot-fatal on any
    /// error (see [`DerivationConfigError`]).
    ///
    /// Called unconditionally at boot, before and independently of
    /// `ingest.enabled`: trawld's own telemetry derives through the same
    /// policy, so a node with the HTTP door closed still needs it.
    pub fn resolve(cfg: &IngestConfig) -> Result<Self, DerivationConfigError> {
        let severity = check_list(DerivationList::SeverityFrom, &cfg.severity_from)?;
        let time = check_list(DerivationList::TimeFrom, &cfg.time_from)?;
        Ok(Self::assemble(&severity, &time))
    }

    /// The packaged defaults, built from [`trawl_config::DEFAULT_SEVERITY_FROM`]
    /// and [`trawl_config::DEFAULT_TIME_FROM`] directly.
    ///
    /// Built from the constants rather than by resolving a default
    /// `IngestConfig`, so `defaults() == resolve(&IngestConfig::default())`
    /// is a real drift assertion instead of a tautology.
    pub fn defaults() -> Self {
        let severity: Vec<Source> = trawl_config::DEFAULT_SEVERITY_FROM
            .iter()
            .map(|f| Source::otel(f))
            .collect();
        let time: Vec<Source> = trawl_config::DEFAULT_TIME_FROM
            .iter()
            .map(|f| Source::otel(f))
            .collect();
        Self::assemble(&severity, &time)
    }

    /// Fold validated configured lists into the per-profile tables.
    fn assemble(severity: &[Source], time: &[Source]) -> Self {
        let mut resolved = Self {
            severity: [Vec::new(), Vec::new(), Vec::new()],
            time: [Vec::new(), Vec::new(), Vec::new()],
        };
        for kind in ProducerKind::ALL {
            resolved.severity[kind.index()] = effective(fixed_severity_sources(*kind), severity);
            resolved.time[kind.index()] = effective(fixed_time_sources(*kind), time);
        }
        resolved
    }

    /// The `_severity` sources this profile reads, in precedence order —
    /// first MAPPABLE wins.
    pub fn severity_from(&self, kind: ProducerKind) -> &[Source] {
        &self.severity[kind.index()]
    }

    /// The `_time` sources this profile reads, in precedence order —
    /// first PRESENT wins.
    pub fn time_from(&self, kind: ProducerKind) -> &[Source] {
        &self.time[kind.index()]
    }
}

impl Default for Derivation {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Which configured list an error is about, so every message names the
/// TOML key the operator has to go fix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DerivationList {
    SeverityFrom,
    TimeFrom,
}

impl DerivationList {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SeverityFrom => "severity_from",
            Self::TimeFrom => "time_from",
        }
    }
}

impl std::fmt::Display for DerivationList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Maximum characters of a configured field name echoed into an error.
///
/// Config is operator-authored, not client input, so the bound is about
/// legibility rather than amplification — a 300-byte name is exactly the
/// case being rejected, and printing all of it buries the sentence.
const MAX_ECHO_CHARS: usize = 64;

/// A configured name as it appears in an error message: bounded, cut on a
/// char boundary, marked when cut.
fn echo(name: &str) -> String {
    if name.chars().count() > MAX_ECHO_CHARS {
        let shown: String = name.chars().take(MAX_ECHO_CHARS).collect();
        format!("{shown}...")
    } else {
        name.to_owned()
    }
}

/// Why a derivation-source list will not load. Every variant names the
/// offending entry — the operator must not have to guess which of eight
/// sources trawld disliked.
///
/// All of these are BOOT-FATAL. A derivation list that silently never
/// matches is the sharpest footgun in this design (`severity_from =
/// ["_severity"]` would derive nothing, forever, quietly), so the answer
/// is refusing to start rather than a warning nobody reads.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DerivationConfigError {
    #[error(
        "[ingest] {list} holds {count} entries, more than the {MAX_DERIVATION_SOURCES} allowed"
    )]
    TooManySources { list: DerivationList, count: usize },

    #[error("[ingest] {list} entry {index} has an empty field name")]
    EmptyField { list: DerivationList, index: usize },

    #[error(
        "[ingest] {list} entry {index} ('{name}') is {len} bytes; a field name may be at most \
         {MAX_FIELD_NAME_BYTES}"
    )]
    NameTooLong {
        list: DerivationList,
        index: usize,
        name: String,
        len: usize,
    },

    #[error(
        "[ingest] {list} entry {index} is spelled '{name}' but ingest ASCII-folds every field \
         name, so it would never match — write it as '{folded}'"
    )]
    NameNotFolded {
        list: DerivationList,
        index: usize,
        name: String,
        folded: String,
    },

    #[error("[ingest] {list} entry {index} repeats '{name}', already listed at entry {first}")]
    DuplicateField {
        list: DerivationList,
        index: usize,
        name: String,
        first: usize,
    },

    #[error(
        "[ingest] severity_from entry {index} names '{name}', which is in trawl's reserved \
         namespace — derivation reads SENDER fields, and a '_' name would never match \
         (ADR-0013); an incoming '{name}' arrives as '{bare}'"
    )]
    ReservedSeveritySource {
        index: usize,
        name: String,
        bare: String,
    },

    #[error(
        "[ingest] time_from entry {index} names '{name}'; '{time}' is the only reserved name a \
         derivation list may read"
    )]
    ReservedTimeSource {
        index: usize,
        name: String,
        time: &'static str,
    },

    #[error("[ingest] time_from is empty; it must list '{time}' at least")]
    EmptyTimeFrom { time: &'static str },

    #[error(
        "[ingest] time_from does not list '{time}'; it is the event-time PROPOSAL slot and \
         derivation must be able to read it"
    )]
    MissingTimeProposal { time: &'static str },

    #[error(
        "[ingest] {list} entry {index} ('{name}') has dialect '{dialect}'; allowed dialects are \
         {allowed}"
    )]
    UnknownDialect {
        list: DerivationList,
        index: usize,
        name: String,
        dialect: String,
        allowed: String,
    },

    #[error(
        "[ingest] time_from entry {index} ('{name}') sets a dialect; dialects govern severity \
         NUMERICS only, so they apply to severity_from alone"
    )]
    DialectOnTimeSource { index: usize, name: String },
}

/// Validate one configured list and resolve it to [`Source`]s.
///
/// The rules, in the order they are applied (ADR-0013 slice 2, ruling 5):
/// list length, then per entry — non-empty, storable length, already
/// ASCII-folded, namespace, dialect — then list-wide duplicates, then
/// `time_from`'s `_time` requirement.
fn check_list(
    list: DerivationList,
    specs: &[DerivationSourceSpec],
) -> Result<Vec<Source>, DerivationConfigError> {
    if specs.len() > MAX_DERIVATION_SOURCES {
        return Err(DerivationConfigError::TooManySources {
            list,
            count: specs.len(),
        });
    }

    let mut sources: Vec<Source> = Vec::with_capacity(specs.len());
    for (index, spec) in specs.iter().enumerate() {
        let name = spec.field();
        if name.is_empty() {
            return Err(DerivationConfigError::EmptyField { list, index });
        }
        if name.len() > MAX_FIELD_NAME_BYTES {
            return Err(DerivationConfigError::NameTooLong {
                list,
                index,
                name: echo(name),
                len: name.len(),
            });
        }
        // Ingest folds every field name at the door, so a spelling that
        // folds to something else names a column that cannot exist. This
        // is a refusal rather than a silent fold because the operator's
        // intent is genuinely ambiguous: `Level` may be a typo, and
        // guessing which is exactly what trawl does not do.
        let folded = schema::catalog_key(name);
        if folded != name {
            return Err(DerivationConfigError::NameNotFolded {
                list,
                index,
                name: echo(name),
                folded: echo(&folded),
            });
        }
        // `_time` in `time_from` is the ONE reserved name any derivation
        // list may read: it is the event-time PROPOSAL slot, so reading
        // it is the whole point. Everything else in the namespace would
        // silently never match — the sharpest footgun in this design,
        // which is why it refuses to boot rather than warn.
        let time_proposal = list == DerivationList::TimeFrom && name == schema::TIME;
        if schema::is_reserved_name(name) && !time_proposal {
            return Err(match list {
                DerivationList::SeverityFrom => DerivationConfigError::ReservedSeveritySource {
                    index,
                    name: echo(name),
                    bare: echo(name.trim_start_matches('_')),
                },
                DerivationList::TimeFrom => DerivationConfigError::ReservedTimeSource {
                    index,
                    name: echo(name),
                    time: schema::TIME,
                },
            });
        }
        finish_entry(list, index, spec, name, &mut sources)?;
    }

    // Duplicates are checked after folding is known to be a no-op, so
    // "same field twice" is exactly string equality here.
    for (index, source) in sources.iter().enumerate() {
        if let Some(first) = sources[..index]
            .iter()
            .position(|earlier| earlier.field == source.field)
        {
            return Err(DerivationConfigError::DuplicateField {
                list,
                index,
                name: echo(&source.field),
                first,
            });
        }
    }

    if list == DerivationList::TimeFrom {
        if sources.is_empty() {
            return Err(DerivationConfigError::EmptyTimeFrom { time: schema::TIME });
        }
        if !sources.iter().any(|s| s.field == schema::TIME) {
            return Err(DerivationConfigError::MissingTimeProposal { time: schema::TIME });
        }
    }

    Ok(sources)
}

/// Resolve one entry's dialect and push it. Split out so the `_time`
/// exception inside the namespace check can reach the same code.
fn finish_entry(
    list: DerivationList,
    index: usize,
    spec: &DerivationSourceSpec,
    name: &str,
    sources: &mut Vec<Source>,
) -> Result<(), DerivationConfigError> {
    let dialect = match spec.dialect() {
        None => Dialect::Otel,
        Some(_) if list == DerivationList::TimeFrom => {
            return Err(DerivationConfigError::DialectOnTimeSource {
                index,
                name: echo(name),
            });
        }
        Some(token) => {
            Dialect::from_token(token).ok_or_else(|| DerivationConfigError::UnknownDialect {
                list,
                index,
                name: echo(name),
                dialect: echo(token),
                allowed: DIALECT_TOKENS.join(", "),
            })?
        }
    };
    sources.push(Source {
        field: name.to_owned(),
        dialect,
    });
    Ok(())
}

/// Count an event a profile producer had to DROP.
///
/// Only the salvage profiles reach here (ruling 4): syslog and telemetry
/// have no one to reject to, so a drop means the server refused its own
/// assertion — a bug, not a sender's mistake. The HTTP door keeps its
/// per-event rejection and its own counter.
pub fn count_profile_reject(kind: ProducerKind, reason: RejectReason) {
    metrics::counter!(
        crate::metrics::INGEST_PROFILE_REJECT_TOTAL,
        "profile" => kind.as_str(),
        "reason" => reason.as_str(),
    )
    .increment(1);
}

/// Publish the whole closed label matrix at zero.
///
/// An ABSENT increment on a present series is what proves the salvage
/// profiles are rejection-free; an absent SERIES proves nothing, because
/// a scrape cannot tell "never happened" from "never wired up".
pub fn init_profile_reject_metrics() {
    for kind in ProducerKind::ALL {
        if *kind == ProducerKind::Http {
            continue;
        }
        for reason in RejectReason::ALL {
            metrics::counter!(
                crate::metrics::INGEST_PROFILE_REJECT_TOTAL,
                "profile" => kind.as_str(),
                "reason" => reason.as_str(),
            )
            .increment(0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bare(names: &[&str]) -> Vec<DerivationSourceSpec> {
        names
            .iter()
            .map(|n| DerivationSourceSpec::Bare((*n).to_owned()))
            .collect()
    }

    fn typed(field: &str, dialect: Option<&str>) -> DerivationSourceSpec {
        DerivationSourceSpec::Typed {
            field: field.to_owned(),
            dialect: dialect.map(ToOwned::to_owned),
        }
    }

    fn cfg(severity: Vec<DerivationSourceSpec>, time: Vec<DerivationSourceSpec>) -> IngestConfig {
        IngestConfig {
            severity_from: severity,
            time_from: time,
            ..IngestConfig::default()
        }
    }

    /// A config that is valid except for the list under test.
    fn with_severity(severity: Vec<DerivationSourceSpec>) -> IngestConfig {
        cfg(severity, bare(&["_time"]))
    }

    fn with_time(time: Vec<DerivationSourceSpec>) -> IngestConfig {
        cfg(bare(&["level"]), time)
    }

    fn fields(sources: &[Source]) -> Vec<&str> {
        sources.iter().map(|s| s.field.as_str()).collect()
    }

    // --- the profile vocabulary (ruling 6) ------------------------------

    #[test]
    fn producer_kind_spellings_are_unique_and_round_trip() {
        let spellings: Vec<&str> = ProducerKind::ALL.iter().map(|k| k.as_str()).collect();
        assert_eq!(spellings, vec!["http", "syslog", "trawld"]);
        for kind in ProducerKind::ALL {
            let matches: Vec<&ProducerKind> = ProducerKind::ALL
                .iter()
                .filter(|k| k.as_str() == kind.as_str())
                .collect();
            assert_eq!(matches.len(), 1, "{} is spelled twice", kind.as_str());
        }
        // ALL and the dense index agree — a new variant that forgets
        // either would collide or leave a hole in Derivation's tables.
        let mut seen: Vec<usize> = ProducerKind::ALL.iter().map(|k| k.index()).collect();
        seen.sort_unstable();
        assert_eq!(seen, vec![0, 1, 2]);
    }

    #[test]
    fn producer_reports_its_own_kind() {
        let asserted = Asserted {
            env: "prod",
            service: "unifi",
            message: Some("link down"),
            host: Some("gw"),
            repairs: &[],
        };
        assert_eq!(
            Producer::Http {
                peer_host: "10.0.0.1",
                peer_is_trusted_relay: false,
            }
            .kind(),
            ProducerKind::Http
        );
        assert_eq!(Producer::Syslog(asserted).kind(), ProducerKind::Syslog);
        assert_eq!(Producer::Trawld(asserted).kind(), ProducerKind::Trawld);
        // The HTTP door asserts nothing — an HTTP sender owns its own
        // identity fields and can be told to resend.
        assert!(
            Producer::Http {
                peer_host: "10.0.0.1",
                peer_is_trusted_relay: false,
            }
            .asserted()
            .is_none()
        );
        assert_eq!(
            Producer::Syslog(asserted).asserted().map(|a| a.service),
            Some("unifi")
        );
    }

    // --- the fixed source tables (ruling 1) -----------------------------

    #[test]
    fn syslog_fixed_sources_lead_and_carry_their_dialect() {
        let d = Derivation::defaults();
        let severity = d.severity_from(ProducerKind::Syslog);
        assert_eq!(
            fields(severity),
            vec!["syslog_severity", "severity", "severity_text", "level"]
        );
        // The whole point of ruling 1: provenance licenses the inversion,
        // and it does so HERE, not in a privileged writer.
        assert_eq!(severity[0].dialect, Dialect::Syslog);
        assert!(
            severity[1..].iter().all(|s| s.dialect == Dialect::Otel),
            "only the transport-proven source inverts"
        );
        assert_eq!(
            fields(d.time_from(ProducerKind::Syslog)),
            vec!["syslog_timestamp", "_time", "timestamp", "@timestamp"]
        );
        assert!(
            d.time_from(ProducerKind::Syslog)
                .iter()
                .all(|s| s.dialect == Dialect::Otel)
        );
    }

    #[test]
    fn http_and_trawld_have_no_fixed_sources() {
        let d = Derivation::defaults();
        for kind in [ProducerKind::Http, ProducerKind::Trawld] {
            assert_eq!(
                fields(d.severity_from(kind)),
                vec!["severity", "severity_text", "level"],
                "{} must read the configured chain and nothing else",
                kind.as_str()
            );
            assert_eq!(
                fields(d.time_from(kind)),
                vec!["_time", "timestamp", "@timestamp"]
            );
        }
    }

    #[test]
    fn a_configured_source_cannot_displace_or_duplicate_a_fixed_one() {
        // Naming the syslog artifact in config must not give it OTel
        // numerics on the syslog door, nor list it twice.
        let d = Derivation::resolve(&cfg(
            vec![
                DerivationSourceSpec::Bare(SYSLOG_SEVERITY_FIELD.to_owned()),
                DerivationSourceSpec::Bare("level".to_owned()),
            ],
            bare(&["_time", SYSLOG_TIMESTAMP_FIELD]),
        ))
        .unwrap();
        let severity = d.severity_from(ProducerKind::Syslog);
        assert_eq!(fields(severity), vec!["syslog_severity", "level"]);
        assert_eq!(severity[0].dialect, Dialect::Syslog, "fixed wins");
        assert_eq!(
            fields(d.time_from(ProducerKind::Syslog)),
            vec!["syslog_timestamp", "_time"]
        );
        // On a door with no fixed source it is an ordinary OTel entry —
        // which is exactly the syslog-over-HTTP forwarder's knob.
        let http = d.severity_from(ProducerKind::Http);
        assert_eq!(fields(http), vec!["syslog_severity", "level"]);
        assert_eq!(http[0].dialect, Dialect::Otel);
    }

    // --- the packaging contract (ruling 5) ------------------------------

    #[test]
    fn defaults_match_resolving_the_default_config() {
        // `defaults()` is built from the trawl-config constants and
        // `resolve` from a parsed default `IngestConfig`: if the code
        // default and the packaged default ever diverge, this fails.
        assert_eq!(
            Derivation::defaults(),
            Derivation::resolve(&IngestConfig::default()).expect("packaged defaults must resolve")
        );
        assert_eq!(Derivation::default(), Derivation::defaults());
    }

    #[test]
    fn an_empty_severity_list_derives_nothing() {
        // Legal and meaningful: an install that wants `_severity` only
        // from transport provenance, or not at all.
        let d = Derivation::resolve(&with_severity(Vec::new())).unwrap();
        assert!(d.severity_from(ProducerKind::Http).is_empty());
        assert!(d.severity_from(ProducerKind::Trawld).is_empty());
        // The syslog profile keeps its fixed source — it is trawl's, not
        // the operator's.
        assert_eq!(
            fields(d.severity_from(ProducerKind::Syslog)),
            vec!["syslog_severity"]
        );
    }

    #[test]
    fn both_spellings_resolve_and_an_absent_dialect_is_otel() {
        let d = Derivation::resolve(&with_severity(vec![
            DerivationSourceSpec::Bare("level".to_owned()),
            typed("sev", None),
            typed("otel_sev", Some("otel")),
            typed("pri", Some("syslog")),
            typed("shouty", Some("SYSLOG")),
        ]))
        .unwrap();
        let s = d.severity_from(ProducerKind::Http);
        assert_eq!(fields(s), vec!["level", "sev", "otel_sev", "pri", "shouty"]);
        assert_eq!(
            s.iter().map(|e| e.dialect).collect::<Vec<_>>(),
            vec![
                Dialect::Otel,
                Dialect::Otel,
                Dialect::Otel,
                Dialect::Syslog,
                Dialect::Syslog,
            ],
            "dialect tokens are case-insensitive; an unset one is otel"
        );
    }

    #[test]
    fn full_lists_at_the_bound_resolve() {
        let eight = ["a", "b", "c", "d", "e", "f", "g", "h"];
        assert_eq!(eight.len(), MAX_DERIVATION_SOURCES);
        Derivation::resolve(&with_severity(bare(&eight))).expect("the bound itself is allowed");
        let mut time = vec!["_time"];
        time.extend(&eight[..MAX_DERIVATION_SOURCES - 1]);
        Derivation::resolve(&with_time(bare(&time))).expect("the bound itself is allowed");
    }

    // --- the boot-fatal validation table (ruling 5) ---------------------
    //
    // One case per rule. Every message must NAME the offending entry, so
    // an operator reading a failed boot knows which of eight sources to
    // go fix — asserting only "it errored" would let a message that says
    // nothing useful pass.

    /// The rejecting half of the table: `(what, config, must_mention)`.
    #[test]
    fn every_boot_fatal_rule_rejects_and_names_the_entry() {
        let long = "k".repeat(MAX_FIELD_NAME_BYTES + 1);
        let nine = ["a", "b", "c", "d", "e", "f", "g", "h", "i"];
        let cases: Vec<(&str, IngestConfig, Vec<String>)> = vec![
            (
                "an empty field name",
                with_severity(bare(&["level", ""])),
                vec!["severity_from".into(), "entry 1".into()],
            ),
            (
                "a name too long to be a catalog key",
                with_severity(bare(&[&long])),
                vec!["severity_from".into(), "entry 0".into(), "256".into()],
            ),
            (
                "a spelling ingest's fold would change",
                with_severity(bare(&["level", "Severity"])),
                vec!["entry 1".into(), "Severity".into(), "severity".into()],
            ),
            (
                "a duplicate after folding",
                with_severity(bare(&["level", "sev", "level"])),
                vec!["entry 2".into(), "level".into(), "entry 0".into()],
            ),
            (
                "more entries than the bound",
                with_severity(bare(&nine)),
                vec!["severity_from".into(), "9".into(), "8".into()],
            ),
            (
                "a reserved severity source",
                with_severity(bare(&["_severity"])),
                vec!["_severity".into(), "reserved".into(), "severity".into()],
            ),
            (
                "a reserved time source that is not the proposal slot",
                with_time(bare(&["_time", "_ingested"])),
                vec!["entry 1".into(), "_ingested".into(), "_time".into()],
            ),
            (
                "an empty time list",
                with_time(Vec::new()),
                vec!["time_from".into(), "_time".into()],
            ),
            (
                "a time list without the proposal slot",
                with_time(bare(&["timestamp", "@timestamp"])),
                vec!["time_from".into(), "_time".into()],
            ),
            (
                "an unknown dialect",
                with_severity(vec![typed("level", Some("rfc5424"))]),
                vec![
                    "entry 0".into(),
                    "level".into(),
                    "rfc5424".into(),
                    "otel".into(),
                    "syslog".into(),
                ],
            ),
            (
                "a dialect on a time source",
                with_time(vec![
                    DerivationSourceSpec::Bare("_time".to_owned()),
                    typed("timestamp", Some("syslog")),
                ]),
                vec![
                    "time_from".into(),
                    "entry 1".into(),
                    "timestamp".into(),
                    "severity_from".into(),
                ],
            ),
        ];

        for (what, config, must_mention) in cases {
            let err = Derivation::resolve(&config)
                .expect_err(&format!("{what} must be boot-fatal"))
                .to_string();
            for needle in must_mention {
                assert!(
                    err.contains(&needle),
                    "the message for {what} must name {needle:?}; got: {err}"
                );
            }
        }
    }

    #[test]
    fn the_time_proposal_slot_is_the_one_reserved_name_admitted() {
        // The rule's positive half: `_time` is not merely tolerated, it
        // is required — and it may sit anywhere in the list.
        let d = Derivation::resolve(&with_time(bare(&["timestamp", "_time"]))).unwrap();
        assert_eq!(
            fields(d.time_from(ProducerKind::Http)),
            vec!["timestamp", "_time"]
        );
    }

    #[test]
    fn an_over_long_name_is_echoed_bounded() {
        // The rejected name is by definition huge; the message must stay
        // readable.
        let err = Derivation::resolve(&with_severity(bare(&["k".repeat(4096).as_str()])))
            .unwrap_err()
            .to_string();
        assert!(err.contains("..."), "got: {err}");
        assert!(err.len() < 400, "message must stay legible; got: {err}");
    }
}
