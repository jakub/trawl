# Query consistency across compaction publication

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
retirement. Recovery takes the same guard. An unfinished rollup marker
refuses corpus reads until recovery finishes, including after a restart.
Recovery finishes pending rollups before new WAL compaction, even when daily
rollup is disabled. Otherwise recovery could delete hourly rows added after
its daily file was written. Query-only processes also scan for unfinished
markers before admitting reads.

Producers take a shared guard before writing WAL and keep it through hot
insertion in the same blocking task. Otherwise a stalled producer could add
a hot copy after compaction had already published and drained its batch.
This guard admits ingestion while rollup recovery is pending. It prevents
the file swap while a producer is between durable and visible publication.

This adds waiting when a long query delays publication. It avoids rerunning
DuckDB queries and repeating export side effects after a concurrent write.
Health checks and reads of saved report files do not acquire this guard.
Repin retains its existing corpus guard and pool exclusion. Lock order is
pool permit then publication read for queries, and corpus read then
publication write for compaction. Publication holders never acquire pool
permits or the corpus guard.

The guarantee covers compaction and rollup in one daemon. It does not
deduplicate accepted payloads, client retries, or WAL replay after a crash.
Two identical accepted events remain two events. Another process modifying
the archive does not share this lock. Retention can still expire files,
and corrupt files retain their existing error and quarantine policy.
