# Query consistency across compaction publication

status: amended (2026-11-08): rollup marker discovery ignores the parent's name, and a day has 24 hour inputs, see the Amendment

Queries must not count an accepted event twice because compaction moves it
from the hot buffer to Parquet. Daily rollup must not expose both a merged
daily file and its hourly inputs. The previous publish-then-drain ordering
allowed these overlaps. A controlled three-event experiment returned six
rows while both copies were visible.

The daemon uses a shared `PublicationGate`. Query, export, and field-value
sampling readers acquire a pool permit, then a publication read guard before
selecting files. The blocking task owns the guard through its last read.
An async timeout does not release protection while DuckDB still runs.
Queries and exports include the publication wait in their execution timeout.

Compaction prepares the replacement file without the publication write
guard. It takes that guard for the canonical rename and hot-batch drain,
then releases it before catalog bookkeeping. Rollup holds the write guard
from marker publication through daily-file publication and hourly-file
retirement. The marker's complete hourly-file list is staged and atomically
published through the shared marker writer. Recovery takes the same guard.
An unfinished rollup marker refuses corpus reads until recovery finishes,
including after a restart.
Recovery finishes pending rollups before new WAL compaction, even when daily
rollup is disabled. Otherwise recovery could delete hourly rows added after
its daily file was written. Query-only processes also scan for unfinished
markers before admitting reads. Recovery clears a marker before discarding
an invalid replacement, so a retry cannot mistake the old daily file for
the unpublished replacement.
_Amended 2026-09-27:_ boot recovers rollup markers before it serves, and the
refusal answers 503 `corpus_recovering` with cause kind `rollup_pending`
(ADR-0041 slice 2).
_Amended 2026-11-06:_ recovery discards a replacement only after a
provable abort, and otherwise fails closed (ADR-0041, #357). The rollup
marker records the identity of the replacement, of each input hourly file
and of the prior daily file. A provable abort means every listed hourly
file carries its recorded identity, and the canonical daily file is either
absent with none recorded or carries the recorded prior identity. Recovery
then deletes the tmp and removes the marker durably. Identities tell the
prior daily file from the replacement, so the marker no longer has to go
first. In any other state recovery keeps every file and the marker, and
reads stay refused until an operator resolves it. The marker is now
written with a fatal directory fsync, not through the shared best-effort
writer.

Producers take a shared guard before writing WAL and keep it through hot
insertion in the same blocking task. Otherwise a stalled producer could add
a hot copy after compaction had already published and drained its batch.
This guard admits ingestion while rollup recovery is pending. It prevents
the file swap while a producer is between durable and visible publication.

This adds waiting when a long query delays publication. It avoids rerunning
DuckDB queries and repeating export side effects after a concurrent write.
Health checks and reads of saved report files do not acquire this guard.
Before repin scans or builds a shadow corpus, it takes corpus write, checks
publication state under a read guard, and sets its rollup pause before
releasing those guards. Pending recovery refuses the job. This prevents the
shadow copy from preserving a rollup marker while omitting the temporary
replacement that marker needs. The pause stays held through job cleanup.
Admission uses the existing cutover wait budget and observes cancellation.
Repin retains its existing corpus guard and pool exclusion for cutover.
_Amended 2026-11-06:_ the rollup pause is a count of live pause guards,
not a flag. Rollup stays suppressed while the count is above zero. A guard
adds one when repin admission takes it, under the same locks as before, and
subtracts one when it drops, including on unwind. Nothing mirrors the count
in a separate flag, because two values can disagree between writes. The
ADR-0011 repin permit already keeps a second production guard from
existing, but the count keeps the pause correct in whatever order guards
drop (#345).

Lock order is pool permit then publication read for queries, corpus read
then publication write for compaction, and corpus write then publication
read for repin admission. Publication holders never acquire pool permits
or the corpus guard.

The startup scan skips confirmed missing paths, including dangling links.
It refuses reads when an existing path cannot be inspected, since that path
could hide an unfinished rollup marker. This also stops WAL compaction and
repin admission. Ingestion can still write durable WAL, so disk usage can
grow until the filesystem problem is repaired and the daemon restarts.
Repairing a known pending rollup lets the next read proceed without restart.
_Amended 2026-09-27:_ on an ingest node, each compaction pass scans again,
and a later complete scan clears the failure without a restart. A query-only
node has no compaction, so it still needs a restart (ADR-0041 slice 2).

The guarantee covers compaction and rollup in one daemon. It does not
deduplicate accepted payloads, client retries, or WAL replay after a crash.
_Amended 2026-09-27:_ ADR-0041 closes the WAL replay exclusion. Boot loads
the surviving WAL into the hot buffer under its batch identities, and corpus
reads refuse until compaction proves that the rest is covered.
Two identical accepted events remain two events. Another process modifying
the archive does not share this lock. Retention can still expire files,
and corrupt files retain their existing error and quarantine policy.

## Amendment (2026-11-08): rollup marker discovery ignores the parent's name

Prep record for the untangle prep of 2026-11-08 (defect D13). ADR-0041's
amendment of the same date holds the full rule.

"Query-only processes also scan for unfinished markers before admitting
reads." That scan now finds a `.rollup-` marker in any directory child of a
valid env, not only in a child whose name parses as a date. A marker under a
non-canonical parent refuses reads like any unresolved marker. A rollup merges
at most 24 hourly files, `00` to `23`, and the marker reader refuses any other
hour.
