# Launch review and correction evidence

This report records the resumed cross-family review of Trawl PR #186 and its
Coastwatch consumer, PR #312. It separates source review from executed checks.
The earlier reports retain their original source identities.

## Reviewed scope

Claude Opus, high effort, reviewed the immutable Trawl range
`723ac5d70834cda4d923652a9797ef0037c43e54..b77146ab52e357c53c05795456e218f8b03d41de`.
The [scope manifest](review-scope.json) assigns all 314 changed paths to three disjoint scopes: 82 runtime
paths, 49 distribution paths, and 183 documentation, first-use, and evidence
paths. Follow-up reads closed the reported coverage gaps. The first-use scope
contains 68 source or prose files and 115 evidence files; evidence manifests and
hashes were checked, but historical raw captures were not all reinterpreted.

Coastwatch received a separate review of its launch branch and subsequent fixes.
Every fix was reviewed against immutable Git objects. Integration uses
content-preserving cherry-picks. The review records retain each author's SHA;
the [integration map](integration-map.json) matches those changes by stable Git patch ID onto the PR branches.

The [normalized review records](reviews.json) list job IDs, model and effort, findings,
coverage, skipped work, failed commands, and execution limits. A succeeded job
with findings is not a clean verdict. Earlier findings remain in the record
after their fixes are accepted.

Final runtime closure `64c228b1`, Coastwatch closure `109c9e8d`, and
packaging cleanup closure `c4905141` all report no remaining findings in their
scopes. The preceding distribution batch `d5f72af5` confirms its five original
findings closed and records the cleanup note resolved by `c4905141`.
The Trawl source checkpoint is `4a46bd4b`; Coastwatch is `79be6c06`.
The accompanying report and ledger update are a separate documentation checkpoint.

## Corrections

| Area | Result |
| --- | --- |
| Storage startup | A fresh root can retry an interrupted initial EPOCH publication. Unknown or mixed contents still refuse startup without mutation. File logging starts after backend and storage admission, rejects aliases of storage markers, and identifies invalid configured paths. |
| Distribution | Both Linux DuckDB modules have public Breakpad symbols. The crash test installs the actual runtime, CLI, and server packages. Ordinary native Cargo commands verify the pinned runtime and refuse unchecked selection. Explicit runtime inputs remain unchanged. |
| Historical links | Reader-facing links use published main commit `723ac5d7`. The archived changelog and historical script bytes are unchanged. |
| Coastwatch fixtures | Fleet schema tests create unique databases from template0, preserve the original assertion when cleanup fails, and remove only databases created by that invocation. |
| Coastwatch authentication | Every cookie-authenticated request validates a supplied Origin, including GET and HEAD. Missing Origin keeps the shared policy's allowance. Startup and environment-override instructions describe the current Fleet schema. |
| Contributor test setup | Coastwatch's push hook accepts explicit disposable application and Fleet database targets after generating test credentials. A config-generation failure stops the hook before tests. |

## Executed verification

The root independently ran 39 integrated Trawl startup and configuration checks
on an owned PostgreSQL 18 cluster. All passed. These checks include initial
startup, restart, interrupted publication, deferred logging, marker aliases,
schema refusal, and preservation of database and filesystem state on refusal.
The final diagnostic-only delta passed 26 root checks. Two actual CLI probes
failed their diagnostic assertions before the last correction and passed afterward,
with input file snapshots unchanged.

The root ran the final integrated release-helper suite with its opt-in Docker
checks: all 60 tests passed, with no skips. This includes real stopped-container
ownership and anonymous-volume checks and a killed snapshot-writer regression.
The root also ran an ordinary Cargo CLI
query against the committed Parquet fixture and observed `{"rows":3}`.
The docs check passed 38 pages, 2,909 local links, 26 TOML blocks, and 18 DSL stages.

The distribution writer built all five release executables, packaged and
installed all three Debian packages, and captured a real minidump with 33
threads and 33 memory regions. The harness restarted the daemon, exercised the
documented disable command, and confirmed that the web-proxy user could not
read dumps directly or through `/proc/<pid>/root`. A separate Debian 12 install
with networking disabled passed runtime, CLI, dependency, file-capability, and
package-removal checks. Its independent probe reported `secure=1 duckdb=v1.5.5`.

The final harness packages from a disposable copy of current tracked source
files while mounting the original source read-only. Actual repackaging produced
three packages without changing either original Cargo manifest or its mtime;
the subsequent systemd run passed. The copy records source `308a316e` plus
tracked changes and a snapshot digest. Killing a fixture writer at the manifest
write boundary leaves the original manifest unchanged.

The final cleanup correction puts the source copy in the packager container's
`/tmp`, leaving packages and metadata in the persistent target directory.
Actual offline packaging passed again. A separate owned container ran the
production snapshot function, confirmed its live copy, and was then killed by
its captured ID. Docker removed that `--rm` container; no source-copy directory
appeared in persistent target storage and original manifests remained unchanged.
This last packaging run records `0f01318e` plus the harness change.

That binary build preceded the commits and reports `11c055e1*`. It verifies
the executed source and packaging behavior, but it is not a clean final-head
artifact. Native CI must verify the pushed revision. The harness ran only at
the existing host ptrace scope 1; scope 2 and the capability-removed negative
control were not run. Host ptrace policy remained unchanged.

The pinned dump_syms produced 75,148 public symbols for amd64 DuckDB and 56,418
for ARM64. The root independently inspected both symbol files and ELF notes:

| Architecture | GNU build ID | Breakpad module ID |
| --- | --- | --- |
| amd64 | `5220e854b6ad936b1ff7030bd4793283a398d44b` | `54E82052ADB66B931FF7030BD47932830` |
| ARM64 | `3a951e961b038a36dc2338f5195eb42387793f21` | `961E953A031B368ADC2338F5195EB4230` |

These public symbols do not supply upstream C++ file and line information.

The root independently proved overlapping Coastwatch schema-test invocations:
eight unique database create/drop pairs, five overlap samples, no leftover
fixture databases, and an unchanged parent Fleet sentinel. The application URL
was deliberately unreachable during those Fleet tests. The later author run
passed ten tests across two overlapping invocations with a harmless object in
template1, preserved the original assertion during deliberate cleanup failure,
and left no fixture databases. Root's integrated final fixture and router run
passed all 11 tests. All 64 application migrations remain byte-identical.

One Coastwatch closure record notes that the writer's isolated branch lacks
root commit `1bc1693`. The delivery branch retains that reviewed hook correction.
Root verified that `1bc1693` is an ancestor of the delivery head and that its
`lefthook.yml` and README changes remain byte-identical. The writer's final
two-file correction also matches the delivery content. This is a distinction
between the isolated writer branch and integration, not an unresolved hook defect.

## Earlier remote checks

At pushed Trawl head `b77146ab`, all 23 jobs in
[Trawl CI](https://github.com/jakub/trawl/actions/runs/34816299125) passed,
including 355 browser tests and seven mutation jobs. All nine jobs in
[native distribution CI](https://github.com/jakub/trawl/actions/runs/34816300002)
passed, including installed Linux amd64 and ARM64 artifacts and both installed
macOS CLI architectures. These are earlier-head results.

At Coastwatch head `4b4afbaf`, all six jobs in
[consumer CI](https://github.com/jakub/coastwatch/actions/runs/34816006065)
passed, including 2,754 tests and the Wasm build on the larger runner. The test
job passed on its second attempt after a fixture connection timeout. These
results do not establish the status of the later correction commits.

## Limits and recovered failures

Claude's reviews are source evidence. Runtime evidence comes from the named
author and root checks. A distribution review initially timed out at its
900-second default and returned no verdict; a verified continuation completed
the scope. A few review reads were denied by containment or network policy.
Smaller Git-object reads recovered the relevant source coverage, and the
remaining external-source limits appear in the review records.

Root's first Coastwatch concurrency attempt passed the tests but did not
observe overlap because Cargo serialized the builds. A rerun using prebuilt
nextest metadata established overlap. Copied build caches initially lacked
the empty web asset directories normally created by the projects' build scripts;
restoring those generated directories allowed the native API checks and
Trawl's normal Clippy hook to compile.
Those checks are not browser or Wasm execution. The local sqlx executable was
absent, so Fleet preparation used the actual `fleet-admin migrate` command.

Test infrastructure uses owned disposable databases and a captive SMTP sink.
Container cleanup includes anonymous data volumes. Build caches and committed
evidence can remain after the run. Existing application state is outside this
verification.

One distribution coverage continuation ran eight Cargo-helper tests and four
distribution tests despite its source-only brief. That deviation appears in
its record; those results are not counted as independent root verification.
Recorded database URLs in the systemd log replace disposable fixture passwords
with `<fixture-password>`; the fixtures use credentials defined by the harness.

## Evidence files

| Claim | Capture |
| --- | --- |
| Integrated startup and diagnostic checks | [39 startup tests](trawl-startup-39.log), [26 diagnostic tests](trawl-diagnostics-26.log), [before/after CLI probes](trawl-diagnostic-context.json) |
| Native Cargo and release helpers | [ordinary query](ordinary-cargo-query.log), [60 helper tests](release-helpers-60.log), [outside-cwd refusal](outside-cargo-rejected.log), [outside-cwd success](outside-cargo-accepted.log) |
| Explicit runtime integrity | [read-only input success](explicit-readonly-cargo.log), [mismatch refusal](explicit-mismatch-cargo.log) |
| Actual packages and harness | [offline Debian install](installed-debian.log), [source-copy packaging](snapshot-package-retry.log), [container-local packaging](container-tmp-package.log), [SIGKILL cleanup](container-tmp-kill-result.json), [systemd run](harness-snapshot-final.log), [owned cleanup](preflight-cleanup.log) |
| Coastwatch consumer | [11 integrated checks](coastwatch-integrated-11.log), [overlapping invocations](coastwatch-concurrency.log), [cleanup result](coastwatch-concurrency.json) |
| Documentation | [development docs check](docs-check.log) |

The adjacent SHA256SUMS covers the captured files. Review records preserve prior
findings as history; the report and closure records identify the final dispositions.
Current pushed-head checks and external review status are available on
[Trawl PR #186](https://github.com/jakub/trawl/pull/186) and
[Coastwatch PR #312](https://github.com/jakub/coastwatch/pull/312).
