# An ingest preview canonicalizes a sample as the HTTP door would and stores nothing of it

status: accepted (2026-09-30), prep record for #200

An operator who connects a new source learns what trawld did to its events only after they land. The `/api/v1/ingest` response reports a rejected event's index and message. It drops the typed reason and never reports repairs. A repair surfaces later, as a `_repairs` code on stored rows or as a `/metrics` counter. By then the events are permanent, because ingest has no idempotency key (ADR-0045). The doctors do not help: a doctor never ingests (ADR-0047). This record adds an **ingest preview**. It is a route that runs a bounded sample through the HTTP producer's canonicalization and returns what each event would become, without keeping any of it.

## Decision

**The preview is one route and one CLI verb.** `POST /api/v1/ingest/preview` takes the same body as `/api/v1/ingest`: NDJSON or a JSON array, told apart by the first byte. `trawl preview-ingest [FILE|-] --peer-ip IP` sends a file or standard input and prints a table, or the response itself with `--json`. There is no browser control in 1.0. A place to paste a sample belongs on a Senders settings page, an idea deferred to 1.1 in #198. The preview covers the HTTP producer only. Syslog frames use a different parser and listener settings, so a syslog preview needs its own design.

**`server_manage` gates it.** The reply shows configuration. `env_not_allowed` lists the allowed environments, a host repair shows whether an address is a trusted relay, and the header names the derivation sources. `server_manage` is the permission to manage and view server configuration. The trial's operator key holds it and the ingest key does not (ADR-0045), so the person connecting a source can preview and the collector cannot. The preview uses the interactive rate bucket.

**trawl-web forwards it.** trawl-web answers 404 on `/api/v1/ingest` so that a browser session can never write to the corpus. The preview writes nothing, so trawl-web forwards it like any other read. A trawl-web test pins both routes: `/api/v1/ingest` stays 404, and `/api/v1/ingest/preview` forwards. Responses carry `Cache-Control: no-store`, because the sample crosses the proxy.

**The caller names the collector's address.** The HTTP producer fills `host` from the peer address and classifies the peer as a trusted relay or not. The caller of a preview is rarely the collector. A laptop or trawl-web would put its own address into `host` and get the wrong relay classification. So `peer_ip` is required. It is the address trawld would see for the collector, after any NAT. The server derives relay membership from its own configuration. The caller cannot override the relay flag, the producer, the arrival time, or any configuration. The response echoes the peer, its relay classification, and the one arrival time used for the whole sample.

**The preview answers with the real canonicalizer's own decisions.** The preview and `/api/v1/ingest` share one parse step and one per-event canonicalization step. The preview is a second consumer of that step's result. It never has its own parser or its own context builder. The response lists events in input order. Each event carries its position, the parsed input, and an outcome. For an accepted event, the response also carries:

- the full canonical event;
- its repair codes;
- where each envelope value came from: the source field for `_time` (the first present) and `_severity` (the first mappable), or a configured default, the peer, or the arrival time;
- each field that was renamed, dropped, truncated, or stringified, with its code.

The canonicalizer reports these facts as it makes each decision. No caller reconstructs them from the rules. A rejected event carries its typed reason and message, and no partial canonical event. An input line that is not valid JSON, or an array element that is not an object, gets a position and a parse reason. A sample in which every event is rejected is still a `200`.

**Real ingest gains the typed reason too.** A rejected event in the `/api/v1/ingest` response carries `index`, `reason`, and `message`, the same shape the preview uses. Both routes return the same reject messages. The preview does not have a second, vaguer set.

**The sample is bounded, and nothing is truncated.** The body limit is the interactive `max_request_body_bytes`, 128 KiB by default, and a sample holds at most 500 events. Invalid non-blank lines count toward the 500. A larger request is refused whole with `413`. The route refuses `Content-Encoding: gzip` with `415`, because compression is transport and not canonicalization. When ingest is disabled, the route is not mounted and answers `404`, like `/api/v1/ingest`. A node without the HTTP producer has nothing to preview.

**Nothing about the sample is stored, counted, or logged.** A preview takes no hot-buffer reservation, passes no publication gate, and writes no WAL. It publishes nothing to the hot buffer, the event bus, or a stream. It changes no ingest, repair, reject, or unmapped-severity counter and no `/stats` total. It adds no name to the service-label table behind the repair metric (ADR-0009). It emits no tracing event that carries sample content. The request itself leaves the same traces as any read, the three that ADR-0047 names for doctors: the key's last-used time, spent rate budget, and an `http_failure` event if trawld answers with a 5xx (ADR-0040). With debug logging on, the request line (method, path, status) is logged too. The sample's values never appear in any of these traces.

**CI proves the contract three ways.** A real-server test sends a sample with distinctive values in every input position. The sample covers every reject reason and every repair code. With self-telemetry enabled, the test shows that the WAL listing, the hot-buffer charge, the ingest counters, `/stats`, and an open stream subscriber are unchanged, and that no persisted line contains a sample value. After the preview, a real ingest of a new service still gets its own metric label. A differential test sends one body to both routes with the same peer. It compares the preview's canonical events with the WAL lines field by field. The comparison masks `_ingested`, and `_time` where the arrival time filled it. A source-scan tripwire keeps the preview code from naming the WAL writer, the pipeline, the metrics, or tracing. The scan backs up the other two tests and does not replace them.

**A preview promises canonicalization only.** A canonical event in a preview does not mean the batch will be admitted, because capacity is a fact of the moment (ADR-0043). It does not mean the collector's key works, or that the event will survive compaction without a shelved value. It does not mean the same sample gets the same repairs after the configuration changes. It does not mean the deployment is correct. The doctors check the deployment (ADR-0047), and #198's walkthrough proves that a real sender's events arrive.

## Considered options

**Gating on `ingest`**, rejected. The collector's key could preview and the trial's operator key could not. That is the wrong way round.

**Gating on `validate`**, rejected. Reader roles hold it for DSL checks, and a reader should not learn which addresses are trusted relays.

**A new `ingest_preview` permission**, rejected for now. Every existing admin role would need the permission added by hand before its first preview works. The trial's fixed key list would need an amendment. No one needs to preview without also managing the server. Add the permission when the 1.1 Senders page needs a preview-only role.

**Defaulting the peer to the caller's address**, rejected. Through trawl-web the peer would be the proxy, and from a laptop it would be the laptop. The preview would look correct and describe the wrong host.

**Accepting gzip bodies**, rejected. A captured Vector batch is larger than the 128 KiB limit after decoding, so accepting gzip buys little. The decoder also logs on overflow.

**Answering on a node with ingest disabled**, rejected. That node's configuration is not the configuration of the node that will receive the events.

**A response-size cap and a per-daemon concurrency cap**, rejected. The input limits already bound the response. Only `server_manage` keys can call the route, and the interactive rate limiter already bounds each key.

**Reject messages that hide the environment list**, rejected. Shipper keys already receive these messages from real ingest. The list is the fix the operator needs, and a second message set would drift.

**Checking parity by querying ingested events back**, rejected. Conformance at query time can shelve a value against an existing pin, so the query result is not canonicalization's output. The WAL is.

**Calling the verb `trawl preview`**, rejected. ADR-0035 already uses "preview" for a read-only net run, so the bare verb would collide.
