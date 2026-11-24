# The SPA reads every failure through one envelope decoder

status: accepted (2026-11-08), prep record for the untangle defect D14

trawld answers every failure with the `ErrorResponse` envelope: a code, a message, and optional details and conflict data. The SPA reads it in about 40 hand-written `match resp.status()` ladders in `crates/trawl-web-ui/src/api/mod.rs`. Seventeen of them end in `s => Err(ApiError::Status(s))` and drop the envelope: `login`, `me`, `logout`, `health`, `history`, `clear_history`, `schema_services`, `catalog_field`, `repin_status`, `repin_job_status`, `list_saved`, `update_saved`, `delete_saved`, `delete_schedule`, `list_runs`, `list_all_runs`, and `runs_stats`. The rest go through `server_error` (`api/mod.rs:376`), which keeps only the status and the message.

Three further faults sit on the same path:

- `saved_write_error` (`api/mod.rs:983-1003`) shows fixed copy for a saved-name refusal only when the message matches trawld's `thiserror` text byte for byte. Both refusals travel as `bad_request` (`crates/trawl-server/src/error.rs:1281-1285`, `:1317-1325`), so the text is the only signal.
- A 401 has four meanings. The Health page reads it as "no permission" (`pages/health.rs:86-87`). Investigations send the page to sign-in (`pages/investigations.rs:139-146`). Job refresh redirects only for an eligible 401 (`components/job_refresh.rs`). `telemetry_get` and `cancel_query` (`api/mod.rs:956-978`) have no 401 arm, so a 401 there becomes a generic server error.
- trawl-web answers its own refusals with a legacy `{"error": "..."}` body (`crates/trawl-web/src/error.rs:150-154`, `:267`, `:273`, and `routes/proxy.rs:54-63`), beside the envelope it already uses for request-limit and body-budget refusals.

## Decision

**One failure classifier, pure and native-tested.** It takes a status and the body, read once, and returns an `ApiError`:

- A 401 is always `Unauthorized`: the session failed, not the request. A valid envelope on a 401 is kept, but it cannot change the classification.
- Any other status with a valid envelope keeps the whole envelope: code, message, details, and conflict. The existing classifications stay as views of it: `Query` for `parse_error` and `validation_error` (ADR-0039), `Recovering`, and `ResourceExhausted`.
- An absent, unreadable, or non-envelope body gives a bare status. Raw body text is never rendered.

**One send path, with each endpoint listing only its exceptions.** Every ordinary JSON endpoint sends once, reads the body once, decodes its listed success statuses, and passes everything else to the classifier. The 17 bare-status endpoints, `server_error`, `telemetry_get`, and `cancel_query` all move onto it. The listed exceptions are:

| Endpoint | Exception |
|---|---|
| `health` | A 503 whose body decodes as `HealthResponse` is a degraded report. Any other 503, such as trawl-web's `request_limit_reached` envelope, is a failure. |
| `repin` | 200 and 202 return jobs. A 409 is told apart by body shape (`classify_conflict`). |
| `repin_job_status` | 404 is `Ok(None)`. |
| `logout` | 200 and 204 succeed with no JSON body. |
| export | Success is a binary read. Failure takes the classifier. |
| investigation reads and result pages | 429 is `Paced::Throttled` with its retry-after, and the abort signal is kept. |

**Investigation writes keep their own classifiers.** `append_keep`, `patch_status`, and `capture_attached` keep their `WriteFailure` and `SendFailure` outcomes (ADR-0059). Their `Unknown` means "never resend": a lost answer, an unreadable answer, or a 5xx after a commit. The typed `request_limit_reached` admission refusal stays a definite refusal. Nothing retries a write.

**Navigation lives in consumers, never in the decoder.** One shared handler sends an eligible consumer to sign-in with its validated return destination (`auth_return::redirect_to_login`). A consumer calls it for `Unauthorized` only after its own owner-lifetime and request-generation checks, as job refresh does today, so a stale or disposed response cannot navigate. Every protected read that can see a 401 uses this handler, the Health page included. A 401 from `login` stays a failed sign-in. Explicit logout keeps its coordination. A 403 stays a permission refusal. Consumers that matched a bare status move to the status and code accessors. `components/field_case_drawer.rs:545-547` is one: it reads a 404 as a missing field and a 503 as an unavailable store.

**Fixed copy keys on codes, never on text.** `ErrorCode` gains `InvalidName` (wire name `invalid_name`, status 400) and `DuplicateName` (`duplicate_name`, 409). trawld maps `StoreError::InvalidName` and `StoreError::DuplicateName` to them. The net-name form shows its fixed copy only for the pairs (400, `invalid_name`) and (409, `duplicate_name`), whatever the message says. Any other refusal of a saved write shows generic copy. `saved_write_error` is deleted. `ScheduleExists` and `RepinAlreadyRunning` keep `bad_request`. Every exhaustive match on `ErrorCode` gains the two codes, `kept_as` and its `EVERY_CODE` table (`crates/trawl-web-ui/src/api_error.rs:179`, `:271`) included.

**trawl-web answers only the envelope.** Every `ProxyError` and the blocked-route `not_found` handler answer `ErrorResponse`, with their fixed messages, `Cache-Control: no-store`, and their current statuses:

| Producer | Status | Code |
|---|---|---|
| `Unauthorized`, `ExpiredSession`, upstream 401 | 401 | `auth_error` |
| `OriginMismatch`, upstream 403 | 403 | `forbidden` |
| `BadRequest` | 400 | `bad_request` |
| blocked or unknown browser API path | 404 | `not_found` |
| other upstream status, `Network` | 502 | `service_unavailable` |
| `ServiceUnavailable`, `UpstreamCertificateUnavailable` | 503 | `service_unavailable` |
| `Session`, `Internal` | 500 | `internal_error` |

Only `ExpiredSession` clears the session cookie, as today. trawl-web forwards trawld's own envelopes untouched and does not assume that an arbitrary upstream body is an envelope.

**`trawl doctor --web-url` reads the envelope in the same change.** Its `web.origin` check accepts the origin on a 400 with code `bad_request` and message `bad request`, and rejects it on a 403 with code `forbidden` and message `cross-origin request rejected`. It no longer accepts the legacy body. ADR-0047's amendment of 2026-11-08 records the probe protocol.

**Trust.** Envelope messages come from trawld, which trawl-web reaches only under verification (ADR-0048) and which redacts internals (ADR-0040), or from trawl-web's fixed strings. The SPA renders them as escaped text. The fixed copy for name refusals is a wording choice keyed by code, not a security control.

## Premise checks

Checked at `b71f9e6e`. `62ee208b..b71f9e6e` changes only `crates/fleet-dev`, `fleet-dev.toml`, and `Cargo.lock`.

- `grep -c 's => Err(ApiError::Status(s))' crates/trawl-web-ui/src/api/mod.rs` prints 17.
- `trawl-server/src/error.rs:1281-1285` maps `DuplicateName` to 409 `bad_request`, and `:1317-1325` maps `InvalidName` to 400 `bad_request`.
- `crates/trawl-cli/src/doctor/web.rs:38-40` and `:116-151` accept only `{"error":"bad request"}` on a 400 and `{"error":"cross-origin request rejected"}` on a 403. `docs/src/content/docs/reference/cli.md` documents those bodies.
- `crates/trawl-web/src/error.rs:167` onward answers a 503 envelope for `RequestLimitReached` on any route, health included, so a 503 alone cannot tell a degraded health report from a refusal.
- `crates/trawl-web-ui/src/components/field_case_drawer.rs:545-547` matches `ApiError::Status(404)` and `ApiError::Status(503)`.
- `crates/trawl-web-ui/e2e/tests/readable-net-names.spec.ts:34-36` fulfils the route with `bad_request` and trawld's exact text.

## Considered and rejected

- **Redirecting only the Health page.** The Health page is where the wrong reading was found, but job refresh, investigations, and the other pages would keep their own 401 handling. The defect is the four readings, so every eligible consumer uses the one handler.
- **Navigation inside the send path.** The send path cannot know whether its caller is still mounted or still current. A stale 401 would navigate a newer page.
- **Every health 503 is a `HealthResponse`.** trawl-web answers a 503 envelope on the same route. Decoding it as a health report gives a decode error and loses the refusal.
- **Keeping only the code in `ApiError::Server`.** Details and conflict data would still be dropped, and the next consumer that needs them would add another exception.
- **One structured failure type that replaces the existing variants, with request id and retry-after on every failure.** It rewrites every consumer, the ADR-0039 query path included, for fields no screen reads. Retry-after already rides `Paced` where it is used.
- **Generic `ApiError` for investigation writes.** It would erase the difference between a refusal and an unknown outcome, and a write with an unknown outcome must never be resent.
- **Matching the saved-name text.** A harmless wording change in trawld silently drops the form's guidance.
- **The doctor accepting both body forms.** The doctor recognizes trawl-web by its exact answer. Accepting a legacy form keeps a shape no producer emits.

## Consequences

- The envelope is the only failure body that trawl-web produces. Tests that read the legacy `"error"` string move to the envelope.
- `invalid_name` and `duplicate_name` are new wire codes. API clients that matched `bad_request` for these refusals see the new codes.
- No persisted state changes.
