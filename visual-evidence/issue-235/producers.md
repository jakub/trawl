## 5xx producers on `POST /api/v1/query`

Base is `ed3a2516`, the code the homelab ran for this path. "Base log" names the producer's own cause event. At base, TraceLayer's `on_failure` (`transport/http.rs:214`) also logged a generic `http_failure` for every 5xx, as `error="Status code: N"` with no cause. "Now" is the `http_failure` that the observer (`transport/failure.rs:350`) emits on this branch.

| # | Producer at base | Status | Base log | Now: stage, class/cause | Fits the incident (500, 956 ms)? |
| --- | --- | --- | --- | --- | --- |
| 1 | fleet-auth DB, migration, schema, hash or token error: `fleet-auth/src/middleware.rs:354-373`, rewritten by `policy.rs:293` | 503 | ERROR `auth.backend`, stdout only | `pre_admission`, `service_unavailable`/`unknown` (`policy.rs:332`), with `peer_addr`; capped at 60/min | No: wrong status |
| 2 | fleet-auth unexpected `verify_key` error: `middleware.rs:386-392` (only `InvalidPrincipalKind`, from a corrupt `kind` column), rewritten by `policy.rs:311` | 500 | ERROR `fleet_auth`, stdout only | `pre_admission`, `internal`/`unknown`, recorded at `policy.rs:329` | No: it fails the same way on every replay with the same key |
| 3 | `rate_limit.rs:186,191`: `Internal` when rate state or key is missing from extensions | 500 | ERROR `internal_error` (`error.rs:458`) | `pre_admission`, `internal`, via `error.rs:542` | No: the router always installs both |
| 4 | axum `Extension<VerifiedKey>` rejection (`handlers.rs:84`) | 500 | Silent (axum logs rejections at TRACE) | `unrecorded`, `reached=handler` | No: auth always inserts the key |
| 5 | axum `Json<QueryRequest>` rejection | 4xx only | n/a | n/a | No |
| 6 | Engine `Database`, `Io`, `Cancelled` (`error.rs:311-317`) | 500 | ERROR `query_failed`, with `error_class` and `query_id` (`handlers.rs:446`) | `handler_error`, `database`/`io`/`cancelled` + typed `cause_kind`, `query_id` | **Yes.** It runs after the permit is taken, so it matches the full-length latency and the spanless `pool_acquired`. Its `query_failed` can be lost to the H1 span-loss defect (checkpoint 1) |
| 7 | Pool `Internal`: `pool.rs:1196` executor pool inconsistent (with ERROR `pool_invariant` at 1190), `pool.rs:1380` pool shut down | 500 | ERROR `query_failed` + ERROR `internal_error` | `handler_error`, `internal` | Unlikely: needs an invariant break or shutdown |
| 8 | Pool worker panic `pool.rs:1513` and join panic `pool.rs:1535` | 500 | ERROR `query_failed` + `internal_error`; the default hook prints the payload to stderr | `panicked` (`ServerError::Panicked`, `pool.rs:1504,1523`) | Unlikely: no `panicked` text in the pod log |
| 9 | Outer `CatchPanicLayer` (`http.rs:154`) | 500 | Silent in tracing; default hook on stderr | `panicked`, via `panic_response` (`http.rs:261`) | Unlikely: same reason as 8 |
| 10 | Engine `ColdDataUnread` (`error.rs:319`) | 503 | ERROR `query_failed` | `handler_error`, `cold_data_unread` | No: wrong status |
| 11 | `Timeout` (`error.rs:441`) | 504 | WARN `query_timeout` | `handler_error`, `timeout` | No: wrong status, 30 s budget |
| 12 | `CAPACITY_NOT_STARTED`: deadline before start (`handlers.rs:180`, `pool.rs:529`), publication gate not ready (`publication.rs:70`) | 503 | ERROR `query_failed`; INFO `query_not_started` | `handler_error`, `service_unavailable` | No: wrong status |
| 13 | Store `Unavailable`/`Migration` in `from saved` resolution (`error.rs:331-344`) | 503 | ERROR `storage.backend` | `handler_error`, `store` | No: wrong status, and the query has no `from saved` |
| 14 | axum `Json<QueryResponse>` serialization failure | 500 | Silent | `unrecorded`, `reached=handler` | No: `Value` serializes only primitives and arrays (`trawl-api/src/value.rs:85`), and serde_json writes non-finite floats as `null` |

Conclusion: by status and timing, row 6 is the only reachable candidate. Rows 8 and 9 remain only if the pod dropped stderr.
