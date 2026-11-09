// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! What each endpoint makes of an answer, as pure functions of its
//! status, headers and body (ADR-0066).
//!
//! The transport in `api` sends a request once, reads its body once, and
//! hands both here. An ordinary endpoint lists its success statuses and
//! nothing else ([`answer`]); every other status goes to the one
//! classifier, [`http_failure`]. The other functions are the exceptions
//! ADR-0066 lists, one per endpoint, and nothing else is one. None of
//! them navigates: what a 401 does to the page is the caller's to decide.
//!
//! The investigation writes are not here. They keep their own
//! classifiers (`investigation_live::write_refusal`,
//! `send_investigation::send_refusal`), because an unknown outcome there
//! must never read as a refusal.

#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use serde::de::DeserializeOwned;
use trawl_api::{HealthResponse, RepinJobResponse, RepinResponse, RepinStatusResponse};

use crate::api_error::{ApiError, http_failure};
use crate::evidence_queue::{Paced, retry_after_ms};
use crate::repin_flow::{ConflictBody, RepinOutcome, classify_conflict};

/// What a success with no readable body decodes to.
const UNREADABLE: &str = "the answer's body could not be read";

/// An ordinary answer: a status in `ok` decodes `body` as `T`, and any
/// other status is a failure for [`http_failure`]. A success whose body
/// is missing or does not decode is [`ApiError::Decode`]: the server
/// said yes, and this client cannot read what it said.
pub fn answer<T: DeserializeOwned>(
    status: u16,
    body: Option<&str>,
    ok: &[u16],
) -> Result<T, ApiError> {
    if ok.contains(&status) {
        decode(body)
    } else {
        Err(http_failure(status, body))
    }
}

/// A success body as `T`.
fn decode<T: DeserializeOwned>(body: Option<&str>) -> Result<T, ApiError> {
    let body = body.ok_or_else(|| ApiError::Decode(UNREADABLE.to_owned()))?;
    serde_json::from_str(body).map_err(|e| ApiError::Decode(e.to_string()))
}

/// `GET /api/v1/health`. trawld answers a degraded or unavailable report
/// with 503, and trawl-web answers its own refusals on the same route
/// with 503 too (`request_limit_reached`), so a 503 is a report only when
/// its body decodes as one. Any other 503, a malformed one included, is
/// the failure it says it is.
pub fn health(status: u16, body: Option<&str>) -> Result<HealthResponse, ApiError> {
    match status {
        200 => decode(body),
        503 => body
            .and_then(|body| serde_json::from_str(body).ok())
            .ok_or_else(|| http_failure(status, body)),
        _ => Err(http_failure(status, body)),
    }
}

/// `POST /api/v1/schema/repin`. 200 is the plan of a dry run and 202 a
/// claimed job. A 409 is double-shaped, the refusal's plan or the held
/// slot's envelope, and [`classify_conflict`] tells them apart by
/// decoding; a 409 whose body could not be read at all is a decode
/// failure, never a guess.
pub fn repin(status: u16, body: Option<&str>) -> Result<RepinOutcome, ApiError> {
    match status {
        200 => decode::<RepinResponse>(body).map(|r| RepinOutcome::DryRun(r.job)),
        202 => decode::<RepinResponse>(body).map(|r| RepinOutcome::Started(r.job)),
        409 => {
            let body = body.ok_or_else(|| ApiError::Decode(UNREADABLE.to_owned()))?;
            Ok(match classify_conflict(body) {
                ConflictBody::Plan(job) => RepinOutcome::Refused(*job),
                ConflictBody::Busy(message) => RepinOutcome::Busy(message),
            })
        }
        _ => Err(http_failure(status, body)),
    }
}

/// `GET /api/v1/schema/repin/status?id=`. A 404 is not a failure: the
/// route holds no job with that id, which is `Ok(None)`.
pub fn repin_job_status(
    status: u16,
    body: Option<&str>,
) -> Result<Option<RepinJobResponse>, ApiError> {
    match status {
        200 => decode::<RepinStatusResponse>(body).map(|r| r.job),
        404 => Ok(None),
        _ => Err(http_failure(status, body)),
    }
}

/// `POST /api/auth/logout`. 200 and 204 both mean the cookie is cleared,
/// whatever body came with them, an empty one included.
pub fn logout(status: u16, body: Option<&str>) -> Result<(), ApiError> {
    match status {
        200 | 204 => Ok(()),
        _ => Err(http_failure(status, body)),
    }
}

/// A paced read (an investigation read, a result page) before its body
/// is read. A 429 is not an answer about the rows but the pause its
/// `Retry-After` asks for, in either form [`retry_after_ms`] reads, and
/// its body is never read. Anything else is [`Paced::Answered`], and the
/// caller reads the body for [`answer`].
pub fn paced(
    status: u16,
    retry_after: Option<&str>,
    now_unix_ms: f64,
    parse_http_date: impl Fn(&str) -> Option<f64>,
) -> Paced<()> {
    if status == 429 {
        Paced::Throttled {
            retry_after_ms: retry_after_ms(retry_after, now_unix_ms, parse_http_date),
        }
    } else {
        Paced::Answered(())
    }
}

/// `POST /api/v1/export`, with its body read once as bytes. 200 is the
/// file, and a 200 whose body could not be read is a decode failure. Any
/// other status is a failure for [`http_failure`], read from the same
/// bytes: the export runs the query on the budget a search does, so it is
/// refused with the same envelope.
pub fn export(status: u16, body: Option<Vec<u8>>) -> Result<Vec<u8>, ApiError> {
    match status {
        200 => body.ok_or_else(|| ApiError::Decode(UNREADABLE.to_owned())),
        _ => Err(http_failure(
            status,
            body.as_deref()
                .and_then(|bytes| std::str::from_utf8(bytes).ok()),
        )),
    }
}

#[cfg(test)]
mod response_policy_exceptions {
    use super::{answer, export, health, logout, paced, repin, repin_job_status};
    use crate::api_error::ApiError;
    use crate::evidence_queue::Paced;
    use crate::repin_flow::RepinOutcome;
    use trawl_api::{HealthResponse, HealthStatus};

    const LIMIT_REACHED: &str = r#"{"error":{"code":"request_limit_reached","message":"the server is at its request limit; try again shortly"}}"#;

    fn job_body(status: &str) -> String {
        format!(
            r#"{{"job":{{"id":7,"field":"duration","from_type":"BIGINT",
             "to_type":"VARCHAR","dry_run":false,"force":false,"status":"{status}",
             "requested_by":"ops","started_at":"2026-08-14T00:00:00Z","finished_at":null,
             "error":null,"files_total":12,"rows_carrying":900,"projected_nulls":7,
             "resurrectable":5,"affected_bytes":4096,"files_done":0,"rows_rewritten":0,
             "rows_nulled":0,"rows_resurrected":0}}}}"#
        )
    }

    fn no_date(_: &str) -> Option<f64> {
        None
    }

    #[test]
    fn health_reads_a_503_report_and_refuses_any_other_503() {
        let report =
            health(200, Some(r#"{"status":"ok","version":"1.0.0"}"#)).expect("a 200 is the report");
        assert_eq!(report.status, HealthStatus::Ok);

        let degraded: HealthResponse = health(
            503,
            Some(r#"{"status":"degraded","checks":{"corpus":"rollup_pending"}}"#),
        )
        .expect("a 503 report is a report");
        assert_eq!(degraded.status, HealthStatus::Degraded);

        let err = health(503, Some(LIMIT_REACHED)).expect_err("an envelope is a refusal");
        assert!(
            matches!(err, ApiError::Server { status: 503, .. }),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "the server is at its request limit; try again shortly"
        );

        for body in [Some("<html>503</html>"), Some(r#"{"status":"meh"}"#), None] {
            let err = health(503, body).expect_err("not a report");
            assert!(matches!(err, ApiError::Status(503)), "{body:?}: {err:?}");
        }
        assert!(matches!(
            health(401, None),
            Err(ApiError::Unauthorized { .. })
        ));
        assert!(matches!(
            health(200, Some("not json")),
            Err(ApiError::Decode(_))
        ));
    }

    #[test]
    fn repin_tells_each_answer_apart() {
        assert!(matches!(
            repin(200, Some(&job_body("succeeded"))),
            Ok(RepinOutcome::DryRun(job)) if job.id == 7
        ));
        assert!(matches!(
            repin(202, Some(&job_body("running"))),
            Ok(RepinOutcome::Started(job)) if job.id == 7
        ));
        assert!(matches!(
            repin(409, Some(&job_body("refused_needs_force"))),
            Ok(RepinOutcome::Refused(job)) if job.status == "refused_needs_force"
        ));
        let busy = r#"{"error":{"code":"bad_request","message":"a repin job is already running"}}"#;
        assert!(matches!(
            repin(409, Some(busy)),
            Ok(RepinOutcome::Busy(message)) if message == "a repin job is already running"
        ));
        // A 409 that is neither shape keeps the conflict's fallback; one
        // with no readable body is a decode failure.
        assert!(matches!(
            repin(409, Some("<html>409</html>")),
            Ok(RepinOutcome::Busy(message)) if message == crate::repin_flow::REPIN_BUSY_FALLBACK
        ));
        assert!(matches!(repin(409, None), Err(ApiError::Decode(_))));

        let refusal = r#"{"error":{"code":"bad_request","message":"not a pinned field"}}"#;
        let err = repin(400, Some(refusal)).expect_err("a refusal");
        assert!(matches!(err, ApiError::Server { status: 400, .. }));
        assert_eq!(err.to_string(), "not a pinned field");
        assert!(matches!(
            repin(401, Some(refusal)),
            Err(ApiError::Unauthorized { .. })
        ));
        assert!(matches!(repin(503, None), Err(ApiError::Status(503))));
        assert!(matches!(repin(200, None), Err(ApiError::Decode(_))));
    }

    #[test]
    fn repin_job_status_reads_a_404_as_no_job() {
        assert!(matches!(repin_job_status(404, None), Ok(None)));
        assert!(matches!(
            repin_job_status(404, Some("<html>not found</html>")),
            Ok(None)
        ));
        assert!(matches!(
            repin_job_status(200, Some(&job_body("running"))),
            Ok(Some(job)) if job.id == 7
        ));
        assert!(matches!(
            repin_job_status(200, Some(r#"{"job":null}"#)),
            Ok(None)
        ));
        assert!(matches!(
            repin_job_status(500, None),
            Err(ApiError::Status(500))
        ));
        assert!(matches!(
            repin_job_status(401, None),
            Err(ApiError::Unauthorized { .. })
        ));
    }

    #[test]
    fn logout_accepts_200_and_an_empty_204() {
        assert!(logout(200, Some("")).is_ok());
        assert!(logout(200, Some("not json at all")).is_ok());
        assert!(logout(204, Some("")).is_ok());
        assert!(logout(204, None).is_ok());
        assert!(matches!(
            logout(401, None),
            Err(ApiError::Unauthorized { .. })
        ));
        assert!(matches!(logout(500, None), Err(ApiError::Status(500))));
    }

    #[test]
    fn a_paced_429_is_throttled_without_a_body() {
        assert_eq!(
            paced(429, Some("7"), 0.0, no_date),
            Paced::Throttled {
                retry_after_ms: 7000.0
            }
        );
        // An HTTP date pauses until then.
        assert_eq!(
            paced(
                429,
                Some("Wed, 21 Oct 2026 07:28:00 GMT"),
                1000.0,
                |_| Some(5000.0)
            ),
            Paced::Throttled {
                retry_after_ms: 4000.0
            }
        );
        for status in [200, 401, 404, 500, 503] {
            assert_eq!(
                paced(status, Some("7"), 0.0, no_date),
                Paced::Answered(()),
                "{status}"
            );
        }
        // What is not throttled reads as any ordinary answer.
        let page: Result<serde_json::Value, ApiError> = answer(
            410,
            Some(r#"{"error":{"code":"result_gone","message":"gone"}}"#),
            &[200],
        );
        assert!(matches!(page, Err(ApiError::Server { status: 410, .. })));
    }

    #[test]
    fn an_export_failure_takes_the_classifier() {
        assert_eq!(
            export(200, Some(b"a,b\n1,2\n".to_vec())).ok(),
            Some(b"a,b\n1,2\n".to_vec())
        );
        assert!(matches!(export(200, None), Err(ApiError::Decode(_))));

        let message = "query could not complete with the available query resources; narrow the \
                       time range or simplify the query";
        let body =
            format!(r#"{{"error":{{"code":"query_resource_exhausted","message":"{message}"}}}}"#);
        let err = export(503, Some(body.into_bytes())).expect_err("refused");
        assert_eq!(err.resource_exhausted(), Some(message));

        let unavailable =
            r#"{"error":{"code":"service_unavailable","message":"service unavailable"}}"#;
        let err = export(503, Some(unavailable.as_bytes().to_vec())).expect_err("refused");
        assert!(matches!(err, ApiError::Server { status: 503, .. }));
        assert_eq!(err.resource_exhausted(), None);

        for body in [
            None,
            Some(b"Export unavailable".to_vec()),
            Some(vec![0xff, 0xfe]),
        ] {
            assert!(
                matches!(export(503, body.clone()), Err(ApiError::Status(503))),
                "{body:?}"
            );
        }
        assert!(matches!(
            export(401, None),
            Err(ApiError::Unauthorized { .. })
        ));
    }

    #[test]
    fn an_ordinary_answer_decodes_its_listed_successes_only() {
        let ok: Result<serde_json::Value, ApiError> = answer(201, Some("{}"), &[200, 201]);
        assert!(ok.is_ok());
        let missing: Result<serde_json::Value, ApiError> = answer(200, None, &[200]);
        assert!(matches!(missing, Err(ApiError::Decode(_))));
        let unlisted: Result<serde_json::Value, ApiError> = answer(204, Some(""), &[200]);
        assert!(matches!(unlisted, Err(ApiError::Status(204))));
        let session: Result<serde_json::Value, ApiError> = answer(401, None, &[200]);
        assert!(matches!(session, Err(ApiError::Unauthorized { .. })));
    }
}
