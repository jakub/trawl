# Capacity reads as retention reach, not a countdown

status: accepted (2026-09-23), prep record for #202

With a deletion floor set, a trawl data disk does not fill. When free space drops below `min_free_disk_bytes`, pressure deletion removes date directories in expiry-ratio order (ADR-0018). The loss lands on retention: an environment that should hold 90 days quietly holds 40. A "days until full" countdown answers the wrong question. The free-space line also stays flat exactly while pressure deletion is destroying data. So trawl reports capacity as **retention reach**: how many days of each environment's policy the disk is projected to hold.

## Decision

**Headroom is per filesystem.** trawl measures each filesystem it writes to and labels it with a role:

- **data**: the data root. Repin staging and compaction spill are already required to share its filesystem.
- **wal**: only when `ingest.wal_dir` is on a different device.
- **spill**: the query engine's temporary directory, when it is on a different device.

Roles that share a device collapse into one row. Available bytes are never summed across devices. Each row reports:

- total and available bytes;
- for the data role only, the floor and a **deficit** (`floor − available` while below it, zero otherwise). No signed figure is shown;
- the ADR-0033 status and sample age.

Pressure deletion starts only when available bytes are strictly below the floor, so equality is not a deficit. A floor of 0 means pressure deletion is off, and the row says so. Filesystems other than data have no floor and nothing reclaims them, so they show available bytes only.

**Retention reach comes from the stored tree.** The date partitions on disk are the growth history. No new sample store is added, in memory or durable.

- **The observed days.** For each environment these are its date partitions from today−8 through today−2. Today and yesterday are excluded while rollup and late arrivals settle them.
- **Gaps inside the window.** Retention always deletes an environment's oldest date first. A missing date in the window that is newer than the environment's oldest surviving date is therefore a quiet day and counts as zero. A missing date older than the oldest surviving date is absent, not zero.
- **The budget.** Parquet bytes stored under environment date directories, plus available bytes on the data filesystem, minus the floor.
- **The projection.** Under sustained pressure, the expiry-ratio order drives every finite-retention environment toward the same fraction *f* of its `max_age_days`. Keep-forever environments are deleted last, so they keep their current bytes. Solve for *f*: `Σ daily_bytes × f × max_age_days = budget − keep_forever_bytes`.
  - If *f* ≥ 1, every environment is projected to keep its full policy.
  - Otherwise each environment keeps about `f × max_age_days` whole days.
- **The range.** The projection runs twice: once with each environment's mean observed day, and once with its largest observed day. The result is shown as a range ("about 38–52 of 90 days"), labelled with the observed days it came from.

It is a conditional projection, "if the observed days repeat". It is not a guarantee.

**Keep-forever environments get no time claim.** They show their stored bytes and mean observed daily growth. Their growth shrinks every finite environment's reach over time. A crowd-out date would need a growth model that nothing here specifies.

**Floor of 0.** If the full policy does not fit in stored bytes plus available bytes, the verdict is "the disk fills before retention is reached". No date is given.

**A projection is withheld, with a named reason, when:**

- **insufficient history**: the environment has fewer than 3 observed days;
- **retention suppressed**: a repin is holding two generations, so stored bytes are inflated;
- **measurement unavailable**: the Parquet scan or the data filesystem sample is not `complete`. A projection is never recomputed from a retained, failed sample. The raw readings stay visible and aged per ADR-0033, and the projection is withheld.

A withheld projection carries its reason and no number, on the wire as well as on screen.

**Pressure deletion is evidence, stated as facts:**

- counters of confirmed date-directory removals, split by trigger (`age`, `disk_pressure`);
- a pressure-attempt counter;
- the last sweep's outcome and its age: `completed`, `suppressed`, `failed`, or `exhausted_below_floor`;
- per environment, its oldest surviving date beside its `max_age_days`.

No bytes-freed figure is published. The existing deletion size walk skips errors and sums logical lengths, so it cannot claim space released. The counters start at process start, and the surface says so. The oldest surviving date is the fact that survives a restart. Nothing infers a cause from it, because a young install also has a short oldest date.

**Surfaces.**

- The admin dashboard API and stream carry the headroom rows, the reach projections, and the evidence.
- The Health page shows them in a "Disk and retention" section. The existing "Capacity" card stays about uptime and pools.
- The daemon's terminal monitor shows one summary line.
- `/metrics` exports, per role label only, total and available bytes, the floor, and the deletion and pressure-attempt counters. It carries no paths, device identities or environment names. `/metrics` is unauthenticated, and this matches what it already discloses about storage size. The projection is not exported: it is derived, and Prometheus gauges carry no status (ADR-0033).
- There is no verdict colouring and no "safe" state. Alert rules stay in ADR-0034's scope.

## Considered options

**A time-to-floor forecast from an in-memory free-space history**, rejected.

- Under sustained pressure every window contains a deletion. Such a forecast is ineligible exactly while data is being lost.
- It restarts from nothing after every deploy.
- When a source turns noisy, the transient net-consumption slope overstates the risk. The equilibrium footprint (daily bytes × policy) decides whether the floor is reached.
- It also cannot say which environment grew.

**Retention reach plus a short-term free-space trend**, rejected as a second model to test. The remaining gap is a burst that crosses the floor between hourly sweeps. That is documented as a limit of the sweep cadence, not forecast.

**Publishing bytes freed per deletion**, rejected: the measurement cannot prove released blocks.

**Amending ADR-0033 to put status and age on Prometheus**, rejected. It is a separate decision that belongs with alerting.

## Consequences

The Parquet scan buckets bytes by environment and date in the walk it already performs, so no second walk is added. Retention gains counters and a last-sweep record. The docs pages `operate/retention.md` and `operate/health.md` explain how to read reach and why there is no countdown. They also state that a burst can cross the floor between sweeps.
