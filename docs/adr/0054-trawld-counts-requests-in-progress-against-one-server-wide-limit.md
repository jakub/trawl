# trawld counts requests in progress against one server-wide limit

status: accepted (2026-10-08), prep record for #293

`[server] max_concurrent_requests` (default 256) was meant to cap the HTTP requests trawld works on at once. It did not. `with_edge_layers` added tower's `ConcurrencyLimitLayer` through `Router::layer`, and axum applies a layer once per route and once per method on that route. Each application built its own semaphore. That gave about 40 independent budgets of 256, so the real ceiling was near 10,000. The layer also never refused anything. It waits for a permit inside `poll_ready`, and axum's `Route` always reports ready, so a request over the limit waited with no deadline. The configuration reference, `config/trawld.reference.toml` and the Debian example all said "past it trawld answers 503", which was false. A value of 0 was accepted and made every request wait forever.

Memory had no bound either. Since ADR-0049's 2026-10-07 amendment, one ingest request may hold `[ingest] max_body_bytes` (16 MiB by default) on the wire, plus its decoded body when it is gzip. A bearer token whose prefix matches no key still runs a dummy argon2id check so that response timing does not reveal which prefixes exist. Each production check uses 128 MiB, and nothing limited how many ran at once. A caller could start that work, hang up, and start it again, because the blocking task kept running after the request was gone.

## Decision

**One count, shared by the whole listener.** `max_concurrent_requests` is the number of requests in progress on trawld's HTTPS listener. One count covers every route and every method. A request is one HTTP/1.1 request or one HTTP/2 stream. Requests that match no route (the default 404) and requests with a method the route does not serve (405) count too, because axum runs the edge layers for them and some of them reach authentication. A CORS preflight that the CORS layer answers is not counted, because it runs no handler. Syslog and trawl-web are separate services and are not counted.

**Where the count is taken.** The count is taken inside the request-id, trace, failure-observer, security-header and CORS layers, so a refusal carries a request id, `nosniff`, HSTS and CORS headers. It is taken outside the panic catcher and outside every layer of the nested routers: authentication, grant, rate limit, body limit and the body extractor. So the count also limits how many authentication checks and body reads run at once. Hyper reads the request head before trawld's service runs, so a slow request head holds no count. An unauthenticated request holds the count only while its bearer token is checked, because a bad token is refused before any body is read. A caller with a valid key can hold the count for as long as it takes to send its body. trawld has no deadline for reading a body (see non-goals).

**Full means refused at once.** trawld takes the count with an atomic try-acquire. When none is free, trawld refuses the request at once, without waiting. The waits that sit behind the count have deadlines of their own: the executor queue answers 503 `CAPACITY_NOT_STARTED` when its deadline passes. A second, unbounded wait in front of them is what this decision removes.

**The refusal.** The refusal is a 503 with the error envelope:

- The code is `request_limit_reached`.
- The message names the setting and not its value: `trawld is at its HTTP request limit ([server] max_concurrent_requests); the request was not processed; retry later with backoff`. The refusal reaches callers who have not authenticated, and the value would tell one exactly how many slow uploads fill the server. The setting's name is public documentation, and it tells an operator who reads the error in Vector's log which setting to look at. This is the human's ruling.
- There is no `Retry-After`. No one can predict when a request will finish: a slow upload, a query that waits up to `timeout_secs` and an ingest batch that waits on the publication gate all hold the count. A fixed hint would also make many senders retry at the same moment. Senders back off on their own. Vector's default backoff is 1 to 30 seconds.
- The response carries `Cache-Control: no-store`, including on the ingest preview route. The preview's existing no-store layer sits inside the count, so a refusal never passes through it.
- trawld does not drain the unread body, for the same reason as the 413 in ADR-0049. A client that is still uploading can lose the 503 to a connection reset.

A received `request_limit_reached` proves that the handler did not run and that nothing was ingested. A connection reset proves nothing, and a client must not read it as a 503.

**The refusal comes before every other answer.** When the count is full, a request with a bad token, an oversized body, an exhausted key rate or an unknown route still gets `request_limit_reached`. trawld has not run those checks yet. When a count is free, the other answers come back as before.

**How the refusal is recorded.** The failure observer (ADR-0040) records one event per refusal, as it does for every 5xx. The event has `error_class=service_unavailable` and the new `cause_kind=request_limit_reached`, the same split that `hot_buffer_full` uses. Its stage is `pre_admission`. No key has been counted, so the event is unmetered, logs at WARN as a pressure outcome, and is persisted under the existing process-wide cap of 60 unmetered events a minute. The refusal is made outside the nested routers, so `normalize_auth_errors` never rewrites it and it never counts as an authentication failure. Two metric series give exact numbers: a gauge of requests in progress and a counter of refusals, each labelled by allowance (`regular` or `control`), next to a gauge of each allowance's size. Metric labels never carry request text or key identity.

**A control allowance of 4.** Four further requests in progress are reserved for these routes and methods:

- `GET` and `HEAD /api/v1/health`
- `GET` and `HEAD /metrics`
- `GET` and `HEAD /api/v1/queries`
- `DELETE /api/v1/queries/{id}`

These requests never use the regular count, and regular requests never use the allowance. When all four are in use, the next control request gets the same immediate 503 with its own message, which names the control allowance and not `max_concurrent_requests`. trawld decides membership from its own route table: the matched route template and the method. The raw path, the query string and request headers never decide it. A route added later is regular unless someone adds it here. The allowance gives no permission: query listing and cancellation keep their authentication, rate limits and ownership checks. The allowance keeps probes, scrapes and cancellation reachable while regular traffic fills the count. It does not make health answer 200: health still reports its own dependencies, and an exhausted executor pool is a real 503. The size is fixed in code, like ADR-0043's telemetry share. It covers liveness and readiness probes, one scrape and one operator request. A larger allowance would give unauthenticated callers more room, because health and `/metrics` need no key.

**A request keeps its count until the work it holds ends.** A request in progress ends when its response head is produced, when its future is dropped because the client went away, or when it panics. Response bodies are not counted, so a server-sent event stream stops counting once its head is sent. `max_sse_connections` and the fixed limit of 8 dashboard streams still bound streams for their whole lifetime. There is one exception, and it is the human's ruling. Blocking work that holds the request's body keeps the request's count until that work ends. That work is ingest decode and parse, ingest WAL write and finalize, and the ingest preview report. A client that hangs up after its body was read therefore cannot start the same work again until the first copy has finished. Query and export work does not keep the count. It holds an executor permit, which outlives the request by design (ADR-0024), and making it hold the count as well would report a stalled DuckDB as an HTTP limit. Deliberately independent work, such as a manual run or a scheduled job, never holds a count.

**fleet-auth checks at most 4 tokens at once.** Every argon2id hash and every argon2id verification in fleet-auth, the dummy check included, takes one permit from a single process-wide limit of 4. A check waits for a permit. The permit lives inside the blocking task and is released when the check ends, not when the caller goes away. At the production parameters this bounds argon2 memory at 512 MiB per process, however many callers hang up. A request that waits for a permit is already counted, so the wait is bounded too. A key whose check is cached runs no argon2 and never waits. This is the human's ruling. The limit lives in fleet-auth, so trawld, trawl-web's login and Coastwatch all get it. No public signature changes.

**Range and default.** The setting takes 1 to 65,536. trawld refuses to start on 0 or on a larger value and names the setting and the range, as it does for `max_concurrent_queries`. The upper bound sits below tokio's semaphore maximum. The default is 32. At 32, the ingest bodies alone can reach 1 GiB, or 32 × (16 MiB wire + 16 MiB decoded), inside the chart's 2Gi limit with room for DuckDB. At 16, ingest batches that wait on the publication gate during a compaction fsync, and queries that wait for an executor permit, could fill the count in normal use. The executor queue's own 503 would then never be reached. 32 is a starting policy, not a measured bound. A recorded workload run must show that it refuses nothing in normal use.

**The accept loop survives accept errors.** An error from `accept()`, such as running out of file descriptors, used to end trawld's accept loop and stop it from serving. With a real limit on requests, a connection flood would hit that failure first. trawld logs the error, waits briefly and keeps accepting.

**What the docs promise.** The configuration reference states what the setting counts, the control allowance, the refusal, the range, the default and the memory arithmetic. The arithmetic is per component, not a bound on the process: the count × (wire body + decoded body) for ingest, plus 4 × 128 MiB for argon2. Parse structures, the hot buffer, DuckDB and response buffers come on top. trawl-web forwards the 503 and its headers unchanged and does not retry. The doctors (ADR-0047) read a `request_limit_reached` health answer as "the probe was refused because trawld was at its request limit". They do not read it as a foreign service.

## Non-goals

These are named so that no one mistakes the count for them:

- **A byte budget for request bodies.** `Content-Length` cannot charge a chunked or gzip body, which is why ADR-0043 rejected wire-size estimates. Charging as the body streams in would replace the body extractor. The human's ruling in ADR-0049 accepts the configured body limits, and the real count now makes their arithmetic a ceiling.
- A deadline for reading a request body.
- A timeout on TLS handshakes, and any limit on idle keep-alive connections.
- A limit in trawl-web. Its proxy buffers up to 16 MiB per request with no count, and it needs a design of its own.
- Fairness between ingest and queries. One count lets either fill it.

## Considered options

**`GlobalConcurrencyLimitLayer`**, rejected. It shares one semaphore and fixes the scope, but it still waits in `poll_ready`, so the 503 the docs promise would still never happen.

**`LoadShed` in front of a concurrency layer**, rejected. axum's `Route` reports ready, so readiness never reaches `LoadShed` through `Router::layer`.

**A bounded wait with a deadline setting**, rejected. It adds a setting, and its deadline stacks on the deadlines of the executor queue and the publication gate behind it.

**Take the count after authentication or after the body is read**, rejected. After authentication, a flood of bad tokens could run any number of argon2 checks. After the body is read, the count would bound no body memory.

**Release the count when the request ends, even if its work goes on**, rejected by the human. A caller could hang up and start the same work again, so the blocking pool, not the count, would bound decode memory.

**Hold the count through every blocking task, argon2 included**, rejected by the human. It ties query work to the HTTP limit, and it needs a change to a public fleet-auth signature to pass the count into the argon2 task. A limit inside fleet-auth bounds argon2 for every consumer.

**A new `error_class` of `server_busy`**, rejected. Every server-wide 503 shares `service_unavailable`, and `cause_kind` tells them apart. A new class would hide these refusals from queries on `service_unavailable`.

**`Retry-After: 1`**, rejected, for the reasons in the refusal above.

**Put the configured value in the message**, rejected by the human. `request_too_large` shows its value so that a caller can resize the request. Nothing similar applies here, and the value tells an attacker how many slow uploads fill the server.

**A control allowance of 8**, rejected. It doubles the extra work that callers without a key can reach while the server is full.

**Default 16 or 64**, rejected. 16 refuses normal bursts during compaction and makes the executor queue's 503 unreachable. 64 lets the ingest bodies alone reach the chart's whole 2Gi limit.

## Consequences

The setting's meaning changes, and its default drops from 256 to 32. A deployment that set a value sized for the old per-route budgets must lower it. A sender that hits the limit now gets a 503 at once where it used to hang, and Vector retries it.

The integration tests that build `HttpConfig` with `max_concurrent_requests: 256` keep compiling. A test that saturates the count must hold requests on more than one route and more than one method, or it would also pass against per-route semaphores.

A disconnected ingest request can keep the count for as long as its decode or WAL write runs. The gauge reports that time as in progress.
