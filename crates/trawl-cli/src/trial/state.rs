// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `state.json`: what the trial has done, so a rerun of `up` resumes.
//!
//! The file holds no secret: tokens live in their own 0600 files, and the
//! key prefixes it records are never printed. It is written by atomic
//! rename at 0600, so readers that skip the lifecycle lock (`status`,
//! `key`, `-p trial`) see a whole file.
//!
//! Two readers, two contracts:
//!
//! - [`TrialState::load`] reads schema 1 exactly and refuses anything else,
//!   because `up` must not resume a trial it does not understand.
//! - [`DownView::load`] reads only `trial_id` and `engine_id` and ignores
//!   every other field, so `stop` and `down` act on a trial written by any
//!   CLI version, and only on the engine it was created on.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use super::TrialError;
use super::paths::{read_private, write_private};

/// The schema this CLI writes and resumes.
pub const SCHEMA: u32 = 1;

/// No state file trawl writes comes near this; a larger one is not ours.
const MAX_STATE_BYTES: u64 = 1024 * 1024;

/// Everything `up` has recorded about the trial.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrialState {
    /// [`SCHEMA`]; `up` refuses other values.
    pub schema: u32,
    /// 32 lowercase hex characters from the OS RNG; the value of the
    /// trial id label on every resource.
    pub trial_id: String,
    /// The Compose project name.
    pub project: String,
    /// The CLI version that created the trial.
    pub cli_version: String,
    /// RFC 3339.
    pub created_at: String,
    /// The Docker engine's `docker info` ID. A resume on another engine
    /// refuses.
    pub engine_id: String,
    pub ports: Ports,
    pub images: Images,
    pub phases: Phases,
    pub tls: Option<TlsRecord>,
    pub keys: Keys,
    pub samples: Samples,
}

/// Loopback ports, fixed when the trial is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ports {
    pub api: u16,
    pub web: u16,
}

/// The images the trial runs, recorded on the first `up`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Images {
    pub trawl: ImageRecord,
    pub postgres: ImageRecord,
    /// `--image` replaced the published trawl image.
    pub trawl_overridden: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageRecord {
    /// The reference `up` resolved, such as `postgres:18`.
    pub reference: String,
    /// `docker image inspect` `.Id`.
    pub id: String,
    /// The registry digest, when the image came from one. A locally built
    /// image has none.
    pub repo_digest: Option<String>,
}

/// Steps of `up` that are done and are skipped on resume.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(clippy::struct_excessive_bools)] // four independent milestones
pub struct Phases {
    pub database: bool,
    pub fleet_migrated: bool,
    pub tls: bool,
    pub services_verified: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsRecord {
    pub sha256_fingerprint: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keys {
    pub operator: Option<KeyRecord>,
    pub ingest: Option<KeyRecord>,
}

/// A minted key: its name and the prefix that identifies it for
/// revocation. The prefix is never printed.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyRecord {
    pub name: String,
    pub prefix: String,
}

/// Hand-written so a prefix never reaches a log line or a panic message.
impl std::fmt::Debug for KeyRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyRecord")
            .field("name", &self.name)
            .field("prefix", &"<redacted>")
            .finish()
    }
}

/// The sample data's progress. Ingest has no idempotency key, so `Intent`
/// is written before the one POST and `Complete` only after the counts are
/// verified; `up` never posts over anything else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Samples {
    /// No `up` has reached the sample step yet.
    NotRequested,
    /// `up --no-sample-data`.
    Skipped,
    /// About to post, or posted with an unverified result.
    Intent {
        seed: u64,
        /// RFC 3339; the newest sample timestamp.
        anchor: String,
        /// Expected event count per service.
        expected: BTreeMap<String, u64>,
    },
    /// Posted and verified.
    Complete {
        seed: u64,
        /// RFC 3339.
        first: String,
        /// RFC 3339.
        last: String,
        total: u64,
    },
}

impl TrialState {
    /// Replace `path` with this state, atomically, at 0600.
    pub fn save(&self, path: &Path) -> Result<(), TrialError> {
        let mut bytes = serde_json::to_vec_pretty(self).map_err(|e| TrialError::StateInvalid {
            path: path.to_owned(),
            detail: format!("cannot serialize: {}", category(&e)),
        })?;
        bytes.push(b'\n');
        write_private(path, &bytes, 0o600)
    }
}

impl TrialState {
    /// Read `path`: `Ok(None)` when there is no state file.
    ///
    /// Anything but schema [`SCHEMA`] is refused, and so is any field
    /// schema 1 does not have.
    pub fn load(path: &Path) -> Result<Option<Self>, TrialError> {
        #[derive(Deserialize)]
        struct SchemaProbe {
            schema: u32,
        }

        let Some(bytes) = read_private(path, MAX_STATE_BYTES)? else {
            return Ok(None);
        };
        let probe: SchemaProbe = parse(path, &bytes)?;
        if probe.schema != SCHEMA {
            return Err(TrialError::StateSchema {
                path: path.to_owned(),
                found: probe.schema,
                expected: SCHEMA,
            });
        }
        parse(path, &bytes).map(Some)
    }
}

/// The fields `stop` and `down` need from a state file of any schema.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DownView {
    pub trial_id: String,
    /// The engine the trial was created on. `stop` and `down` refuse
    /// another engine: there, the trial's resources are absent, and
    /// `down` would delete the state while they stay on their engine. A
    /// state that records none is acted on wherever it is run.
    #[serde(default)]
    pub engine_id: Option<String>,
}

impl DownView {
    /// Read `trial_id` and `engine_id` from `path`, ignoring every other
    /// field: `Ok(None)` when there is no state file.
    ///
    /// The id must be usable as a Docker label filter value, so it is
    /// limited to ASCII letters, digits, `-`, and `_`.
    pub fn load(path: &Path) -> Result<Option<Self>, TrialError> {
        let Some(bytes) = read_private(path, MAX_STATE_BYTES)? else {
            return Ok(None);
        };
        let view: Self = parse(path, &bytes)?;
        let usable = !view.trial_id.is_empty()
            && view
                .trial_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !usable {
            return Err(TrialError::StateInvalid {
                path: path.to_owned(),
                detail: "trial_id is not a label value trawl writes".into(),
            });
        }
        Ok(Some(view))
    }
}

/// Parse JSON, reporting a failure by position and category only. serde's
/// own message can quote the offending value, which could be a prefix.
fn parse<'de, T: Deserialize<'de>>(path: &Path, bytes: &'de [u8]) -> Result<T, TrialError> {
    serde_json::from_slice(bytes).map_err(|e| TrialError::StateInvalid {
        path: path.to_owned(),
        detail: format!(
            "{} at line {}, column {}",
            category(&e),
            e.line(),
            e.column()
        ),
    })
}

fn category(e: &serde_json::Error) -> &'static str {
    match e.classify() {
        serde_json::error::Category::Io => "read error",
        serde_json::error::Category::Syntax => "malformed JSON",
        serde_json::error::Category::Data => "unexpected or missing field",
        serde_json::error::Category::Eof => "truncated JSON",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;
    use crate::trial::paths::TrialPaths;

    /// A complete schema-1 state, with every optional part filled.
    pub(crate) fn fixture(api_port: u16) -> TrialState {
        TrialState {
            schema: SCHEMA,
            trial_id: "0123456789abcdef0123456789abcdef".into(),
            project: crate::trial::PROJECT.into(),
            cli_version: "0.9.0".into(),
            created_at: "2026-09-25T12:00:00Z".into(),
            engine_id: "ENGINE:ID".into(),
            ports: Ports {
                api: api_port,
                web: crate::trial::DEFAULT_WEB_PORT,
            },
            images: Images {
                trawl: ImageRecord {
                    reference: "ghcr.io/jakub/trawl:0.9.0".into(),
                    id: "sha256:aaaa".into(),
                    repo_digest: Some("ghcr.io/jakub/trawl@sha256:bbbb".into()),
                },
                postgres: ImageRecord {
                    reference: crate::trial::POSTGRES_IMAGE.into(),
                    id: "sha256:cccc".into(),
                    repo_digest: None,
                },
                trawl_overridden: false,
            },
            phases: Phases {
                database: true,
                fleet_migrated: true,
                tls: true,
                services_verified: false,
            },
            tls: Some(TlsRecord {
                sha256_fingerprint: "AB:CD".into(),
            }),
            keys: Keys {
                operator: Some(KeyRecord {
                    name: "trial-operator".into(),
                    prefix: "pfxsecret1".into(),
                }),
                ingest: None,
            },
            samples: Samples::Complete {
                seed: 203,
                first: "2026-09-24T12:00:00Z".into(),
                last: "2026-09-25T12:00:00Z".into(),
                total: 2000,
            },
        }
    }

    fn state_path() -> (tempfile::TempDir, std::path::PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = TrialPaths::resolve(Some(&tmp.path().join("state")), None).unwrap();
        paths.ensure_dir().unwrap();
        (tmp, paths.state_file())
    }

    #[test]
    fn state_round_trips_through_the_file() {
        let (_tmp, path) = state_path();
        assert_eq!(TrialState::load(&path).unwrap(), None, "absent is None");

        let samples = [
            Samples::NotRequested,
            Samples::Skipped,
            Samples::Intent {
                seed: 203,
                anchor: "2026-09-25T12:00:00Z".into(),
                expected: BTreeMap::from([("web".into(), 700), ("api".into(), 500)]),
            },
            fixture(1).samples,
        ];
        for samples in samples {
            let state = TrialState {
                samples,
                ..fixture(crate::trial::DEFAULT_API_PORT)
            };
            state.save(&path).unwrap();
            assert_eq!(TrialState::load(&path).unwrap().as_ref(), Some(&state));
        }

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn samples_carry_a_snake_case_state_tag() {
        let json = serde_json::to_value(Samples::NotRequested).unwrap();
        assert_eq!(json, serde_json::json!({"state": "not_requested"}));
        let json = serde_json::to_value(fixture(1).samples).unwrap();
        assert_eq!(json["state"], "complete");
    }

    #[test]
    fn up_refuses_another_schema() {
        let (_tmp, path) = state_path();
        let mut json = serde_json::to_value(fixture(1)).unwrap();
        json["schema"] = 2.into();
        write_private(&path, json.to_string().as_bytes(), 0o600).unwrap();

        let err = TrialState::load(&path).expect_err("schema 2 must be refused");
        assert!(
            matches!(
                err,
                TrialError::StateSchema {
                    found: 2,
                    expected: 1,
                    ..
                }
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("trawl trial down"), "{err}");
    }

    #[test]
    fn up_refuses_fields_schema_one_does_not_have() {
        let (_tmp, path) = state_path();
        let mut json = serde_json::to_value(fixture(1)).unwrap();
        json["surprise"] = true.into();
        write_private(&path, json.to_string().as_bytes(), 0o600).unwrap();
        assert!(matches!(
            TrialState::load(&path),
            Err(TrialError::StateInvalid { .. })
        ));
    }

    /// `down` must delete a trial whatever CLI version wrote it.
    #[test]
    fn down_reads_a_future_schema_with_extra_fields() {
        let (_tmp, path) = state_path();
        let future = serde_json::json!({
            "schema": 7,
            "trial_id": "0123456789abcdef0123456789abcdef",
            "ports": {"api": 1, "web": 2, "grpc": 3},
            "something_new": [1, 2, 3],
            "samples": {"state": "resampled", "generation": 4},
        });
        write_private(&path, future.to_string().as_bytes(), 0o600).unwrap();

        let view = DownView::load(&path).unwrap().expect("present");
        assert_eq!(view.trial_id, "0123456789abcdef0123456789abcdef");
        assert_eq!(view.engine_id, None, "a state may record no engine");
        assert!(matches!(
            TrialState::load(&path),
            Err(TrialError::StateSchema { found: 7, .. })
        ));

        let mut future = future;
        future["engine_id"] = "ENGINE:FUTURE".into();
        write_private(&path, future.to_string().as_bytes(), 0o600).unwrap();
        let view = DownView::load(&path).unwrap().expect("present");
        assert_eq!(view.engine_id.as_deref(), Some("ENGINE:FUTURE"));
    }

    /// `stop` and `down` see the engine this CLI records.
    #[test]
    fn down_reads_the_recorded_engine() {
        let (_tmp, path) = state_path();
        fixture(1).save(&path).unwrap();
        let view = DownView::load(&path).unwrap().expect("present");
        assert_eq!(
            view,
            DownView {
                trial_id: "0123456789abcdef0123456789abcdef".into(),
                engine_id: Some("ENGINE:ID".into()),
            }
        );
    }

    #[test]
    fn down_refuses_a_missing_or_unusable_trial_id() {
        let (_tmp, path) = state_path();
        assert_eq!(DownView::load(&path).unwrap(), None);
        for body in [
            r#"{"schema": 9}"#,
            r#"{"trial_id": ""}"#,
            r#"{"trial_id": "a,b=c"}"#,
        ] {
            write_private(&path, body.as_bytes(), 0o600).unwrap();
            assert!(
                matches!(DownView::load(&path), Err(TrialError::StateInvalid { .. })),
                "{body}"
            );
        }
    }

    /// A parse error reports where, never what: the value could be a
    /// prefix or a token pasted into the wrong file.
    #[test]
    fn parse_errors_do_not_echo_values() {
        let (_tmp, path) = state_path();
        let mut json = serde_json::to_value(fixture(1)).unwrap();
        json["ports"]["api"] = "flt_secretvalue".into();
        write_private(&path, json.to_string().as_bytes(), 0o600).unwrap();
        let err = TrialState::load(&path).unwrap_err();
        assert!(!err.to_string().contains("flt_secretvalue"), "{err}");

        write_private(&path, b"{\"trial_id\": flt_secretvalue}", 0o600).unwrap();
        let err = DownView::load(&path).unwrap_err();
        assert!(!err.to_string().contains("flt_secretvalue"), "{err}");
    }

    #[test]
    fn key_record_debug_redacts_the_prefix() {
        let state = fixture(1);
        let debug = format!("{state:?}");
        assert!(!debug.contains("pfxsecret1"), "{debug}");
        assert!(debug.contains("trial-operator"));
    }
}
