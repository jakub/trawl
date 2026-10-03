# Syslog reads zone-less time in a configured zone, never the host's

status: accepted (2026-10-02), prep record for #280

An RFC 3164 frame carries wall-clock time with no zone and often no year: `Oct  2 14:00:05`. trawld reads that time in the zone of the host it runs on. syslog_loose defaults to `chrono::Local`, and nobody chose it: #126 kept it as a named residual risk. A deb install inherits the host's `/etc/localtime`. The container image has no tzdata, so it reads UTC. One sender therefore lands at different instants depending on how trawld was deployed. A sender in a different zone from trawld lands hours off on every deployment. This record makes the zone a setting.

## Decision

**Two `[syslog]` keys set the zone.** `default_timezone` applies to every sender. `[syslog.sender_timezones]` maps a peer address to a zone and overrides the default for that peer. A value is `UTC`, an IANA name such as `Europe/Warsaw`, or a fixed offset `±HH:MM`. A zero offset normalizes to `UTC`. `local`, zone abbreviations, POSIX TZ strings, and unknown names are refused. Fixed offsets stay because some devices keep a constant offset year-round, and the IANA spelling of a fixed offset (`Etc/GMT-5` for UTC+5) inverts the sign.

**An unset default means UTC.** No syslog code path reads the host zone, so the same configuration gives the same instants on every host. UTC also matches the HTTP rule that an offset-less date-time reads as UTC (ADR-0009). This is a human ruling against both design legs, which recommended refusing to boot without a default. A deb install on a non-UTC host whose senders log local time changes interpretation on upgrade. The upgrade notes tell that operator to set `default_timezone` before restarting.

**Keys are exact canonical peer addresses.** The map uses the same lookup and the same folding as `source_service_map`: an IPv4-mapped IPv6 spelling and its IPv4 form are one key. Two spellings of one peer with the same zone collapse to one entry. Two spellings with different zones refuse to start. A key that is not an IP address is refused. There are no CIDR keys, because ranges need overlap and precedence rules that no sender has asked for. The key is the transport peer. A relay's entry applies to everything the relay forwards, whatever the frame's hostname says, and `trusted_relays` does not change that. A relay that forwards senders from several zones has to send RFC 5424 with offsets.

**The zone applies only to zone-less 3164 timestamps.** Both the year-less form and the `MMM DD YYYY HH:MM:SS` form take the zone. An offset on the wire always wins, in RFC 5424 and in an RFC 3339 timestamp inside a 3164 frame. Zone words inside the frame are never read. trawld calls syslog_loose with UTC as its zone, so the wall-clock fields come back unshifted. trawld then reads them in the peer's zone. It decides that a timestamp was zone-less from the timestamp form that parsed, never from the parsed offset, because a wire `Z` and a supplied UTC look identical after parsing. Two syslog_loose defects stop mattering this way. With a zone supplied, its with-year form treats the wall clock as UTC. And a wall time in a DST gap fails the whole frame, losing PRI, host, and app.

**The year is chosen by the calendar, then the instant by the zone.** A year-less timestamp takes the previous, current, or next year by wall-clock distance from arrival, as arrival reads in the peer's zone. Only dates the calendar rejects drop out, such as Feb 29 in a non-leap year. An exact tie goes to the past. This amends #126's rule, which ranked candidate instants and dropped a candidate whose wall time fell in a DST gap. Gap dates move from year to year, so under the old rule `Mar  8 02:30` arriving on 2026-03-08 in `America/Chicago` resolves to a different year. With the year fixed:

- A wall time that occurs once becomes that instant.
- A wall time that occurs twice, in the fall-back overlap, becomes the instant nearer arrival. An exact tie goes to the earlier instant.
- A wall time that never occurs, in the spring-forward gap, gets no `syslog_timestamp`. The event takes its arrival time with `time.from_ingest` and keeps every other parsed field.

**The event records the zone it was read in.** A new artifact, `syslog_timestamp_zone`, holds the normalized zone used for a zone-less timestamp. It is written whenever a zone-less form was read in a zone, including the gap case, and never otherwise. It is an annotation, not a repair: trawld changed no sender-visible value (ADR-0013 §8). Configuration changes stay forward-only with no history (ADR-0013 ruling 5), so this artifact is how a query tells which zone an older event was read in (ADR-0013 ruling 6). Whether a reading fell in an overlap is a fact of the zone database, recomputable from `syslog_timestamp`, the zone, and `_raw`, so the event does not store it.

**IANA rules come from chrono-tz with the zone database compiled in.** It works with the chrono types trawld already uses, and a compiled-in database gives every host the same answer without tzdata in the image. A zone-rule change reaches trawld through a dependency bump. Because events record the zone name rather than an offset, events stored before a bump can be re-read.

**One validation contract covers the syslog peer settings.** Boot, `trawld --check-config`, and the doctor's `server.config` check run the same validation, even when the listener is disabled. It parses every zone, folds the keys of both `sender_timezones` and `source_service_map`, and refuses non-IP keys and conflicting folds. It also refuses a malformed `allow_cidrs` entry. Today trawld drops a malformed entry with a warning, and a list whose entries are all malformed becomes empty, which admits every peer. The `source_service_map` fold currently runs only when the listener starts, so a conflict passes `--check-config` and the doctor and then fails at boot. Errors name the setting and the reason, never the value (ADR-0047).

**A forged peer gains nothing.** A spoofed UDP source address can select another peer's zone. A forger can already write any offset into the frame, so the map grants no new capability. It authenticates nothing.

**Out of scope:** an RFC 5424 timestamp with no offset still fails the RFC 3339 parse and loses the whole frame's header. That is a salvage defect with its own design. A syslog ingest preview stays out (ADR-0049). The `UTC host` mis-parse, where a zone word becomes the hostname, is unchanged.

## Considered options

**Refusing to boot without `default_timezone` when syslog is enabled**, rejected by the human. It makes every operator decide before the first frame, and nothing is silently hours off. The ruling weighed a boot failure on upgrade against a default that agrees with the HTTP rule and that a documented upgrade step covers.

**Keeping host-local time**, rejected. Host-local time is the defect this record fixes.

**CIDR keys**, rejected. See the keys decision.

**One combined per-sender table holding service and zone**, rejected. It rewrites the shipped `source_service_map` contract in docs, the chart, and committed evidence for no behavioral gain. The new key says "sender" because the glossary does. `source_service_map` keeps its shipped name.

**A second artifact recording how the timestamp resolved** (unique, overlap, gap, invalid), rejected. Gap and overlap can be derived from stored data, unique would be on nearly every event, and invalid cannot occur while syslog_loose owns the 3164 timestamp grammar.

**jiff**, rejected: a second datetime library, and its default reads the host's zone files. **tz-rs**, rejected: it needs tzdata in the image, and rules would vary by host. **Fixed offsets only**, rejected: wrong for half of every year in a zone with DST.

**Patching syslog_loose upstream**, not needed. Calling it with UTC sidesteps both defects, and correctness does not wait on an upstream release.
