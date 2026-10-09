#!/usr/bin/env python3
"""Checks for run.py that need no build, database or Vector.

* The pass predicate: the real Run.collect() and verdict() over in-memory
  /metrics bodies and a written trawld log, and the --check-summary mode.
* Container cleanup: Run.cleanup() over a stubbed docker command.
* The --work rules: a refused path is never created or deleted,
  Run.remove_work deletes only the directory its own claim created, and
  a parent that another uid can write to is refused.

Usage, from the repository root:

    python3 -I docs/launch/evidence/2026-10-08-issue-293/default-load/test_run.py

Scratch directories go under .flow-scratch/ in the repository root and are
removed afterwards.
"""

import argparse
import contextlib
import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from datetime import datetime, timezone
from pathlib import Path

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("run", HERE / "run.py")
run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run)

SCRATCH_ROOT = run.ROOT / ".flow-scratch"


def metrics(regular=0, control=0, in_progress=3, allowance=(32, 4)):
    """A /metrics body. None leaves a series out; a string is written
    verbatim as the value."""
    lines = ["# TYPE trawl_http_requests_in_progress gauge"]
    if in_progress is not None:
        lines.append(f'trawl_http_requests_in_progress{{allowance="regular"}} {in_progress}')
    lines += [
        'trawl_http_requests_in_progress{allowance="control"} 1',
        "# TYPE trawl_http_requests_refused_total counter",
    ]
    for name, value in (("regular", regular), ("control", control)):
        if value is not None:
            lines.append(f'trawl_http_requests_refused_total{{allowance="{name}"}} {value}')
    for name, value in zip(("regular", "control"), allowance):
        if value is not None:
            lines.append(f'trawl_http_request_allowance{{allowance="{name}"}} {value}')
    lines.append("trawl_hot_buffer_events 10")
    return "\n".join(lines) + "\n"


def log_line(at="2026-10-08T12:00:01.000000Z", **fields):
    body = " ".join(f'{k}="{v}"' if isinstance(v, str) else f"{k}={v}" for k, v in fields.items())
    return f"{at}  INFO trawl_server: event {body}"


# One compaction and one rollup inside the measured window, 12:00:00 to
# 12:01:00 in judge().
WORK_EVENTS = (log_line(event_type="compaction_complete", compact_service="nginx"),
               log_line(event_type="rollup_complete", compact_service="nginx"))
SENDERS = ("vector-1", "vector-2", "vector-3")


def delivery(stored=100, senders=SENDERS):
    return {"complete": True, "expectedDistinct": {s: 100 for s in senders},
            "counted": {s: {"stored": stored, "distinct": stored} for s in senders}}


def query(status=200):
    return {"t": 1.0, "kind": "page", "status": status, "ms": 5.0}


class Scratch(unittest.TestCase):
    def setUp(self):
        SCRATCH_ROOT.mkdir(exist_ok=True)
        self.scratch = Path(tempfile.mkdtemp(prefix="test-run-", dir=SCRATCH_ROOT))

    def tearDown(self):
        shutil.rmtree(self.scratch)

    def args(self, **overrides):
        values = dict(work=str(self.scratch / "work"), out=str(self.scratch / "out"),
                      vector="/nonexistent/vector", senders=3, seed_cycles=30, steady_rate=268,
                      steady_seconds=180, backlog_cycles=1500, scrape_interval=0.25,
                      query_think=1.0, drain_seconds=900, keep_work=False)
        values.update(overrides)
        return argparse.Namespace(**values)


class Predicate(Scratch):
    """collect() and verdict() on inputs that a full run would produce."""

    def judge(self, samples, final, log_lines=(), collect_error=False, work_events=WORK_EVENTS,
              queries=(query(), query()), deliveries=None):
        r = run.Run(self.args(keep_work=True))
        r.t0 = 0.0
        r.wall0 = datetime(2026, 10, 8, 12, 0, 0, tzinfo=timezone.utc)
        r.delivered_at = 60.0
        r.samples = []
        for i, text in enumerate(samples):
            row = {"t": i * 0.25, "scrape_ms": 5.0, "status": 200}
            row.update(run.sample_row(text))
            r.samples.append(row)
        r.final_metrics = {"t": 70.0, "status": 200 if final is not None else 0, "attempts": 1,
                           "text": final}
        r.queries = list(queries)
        r.trawld_log = self.scratch / "trawld.log"
        if not collect_error:
            r.trawld_log.write_text("".join(line + "\n" for line in (*work_events, *log_lines)))
        r.summary.update({
            "delivery": deliveries if deliveries is not None else delivery(),
            "producersStopped": {"threadsStillRunning": [], "vectorExitCodes": {}},
            "cleanup": {"processes": True, "container": True},
        })
        with contextlib.redirect_stdout(io.StringIO()):
            code = run.conclude(r, 0)
        written = json.loads((self.scratch / "out/summary.json").read_text())
        self.assertEqual(written["status"], r.summary["status"])
        return code, r.summary

    def assertFails(self, result, fragment):
        code, summary = result
        self.assertEqual(code, 1)
        self.assertEqual(summary["status"], "failed")
        self.assertTrue(any(fragment in reason for reason in summary["failureReasons"]),
                        summary["failureReasons"])

    def test_clean_input_passes(self):
        code, summary = self.judge([metrics(), metrics()], metrics(),
                                   [log_line(event_type="http_failure", route="/api/v1/ingest",
                                             status=503, cause_kind="hot_buffer_full")])
        self.assertEqual((code, summary["status"], summary["failureReasons"]), (0, "passed", []))
        self.assertEqual(summary["metrics"]["refusedTotalAtEnd"], {"regular": 0, "control": 0})
        self.assertEqual(summary["requestLimitRefusalEvents"], 0)

    def test_missing_series_in_a_sample_fails(self):
        self.assertFails(self.judge([metrics(), metrics(control=None)], metrics()),
                         "samples without a valid control refusal counter: 1")

    def test_missing_series_in_terminal_snapshot_fails(self):
        self.assertFails(self.judge([metrics()], metrics(regular=None)),
                         "regular refusal counter in the terminal snapshot: None")

    def test_unparseable_value_fails(self):
        self.assertFails(self.judge([metrics(regular="many")], metrics()),
                         "samples without a valid regular refusal counter: 1")

    def test_nonzero_counter_in_a_sample_fails(self):
        self.assertFails(self.judge([metrics(), metrics(control=2)], metrics(control=2)),
                         "highest sampled control refusal counter: 2")

    def test_refusal_after_the_last_sample_fails(self):
        self.assertFails(self.judge([metrics(), metrics()], metrics(regular=1)),
                         "regular refusal counter in the terminal snapshot: 1")

    def test_recorded_request_limit_reached_fails(self):
        self.assertFails(self.judge([metrics()], metrics(),
                                    [log_line(event_type="http_failure", route="/api/v1/ingest",
                                              status=503, cause_kind="request_limit_reached")]),
                         "trawld recorded request_limit_reached failures: 1")

    def test_collect_error_fails(self):
        # No trawld log: collect() raises while reading it.
        result = self.judge([metrics()], metrics(), collect_error=True)
        self.assertIn("collectError", result[1])
        self.assertFails(result, "collecting the evidence failed")

    def test_missing_terminal_snapshot_fails(self):
        r_code, summary = self.judge([metrics()], None)
        self.assertEqual(r_code, 1)
        self.assertIn("no terminal /metrics snapshot after the producers stopped",
                      summary["failureReasons"])

    def test_missing_allowance_fails(self):
        self.assertFails(self.judge([metrics()], metrics(allowance=(None, 4))),
                         "regular allowance in the terminal snapshot: None, not 32")

    def test_wrong_allowance_fails(self):
        self.assertFails(self.judge([metrics()], metrics(allowance=(256, 4))),
                         "regular allowance in the terminal snapshot: 256.0, not 32")
        self.assertFails(self.judge([metrics()], metrics(allowance=(32, 8))),
                         "control allowance in the terminal snapshot: 8.0, not 4")

    def test_null_peak_fails(self):
        self.assertFails(self.judge([metrics(in_progress=None)] * 2, metrics()),
                         "no regular in-progress peak of at least 1 was sampled: None")

    def test_zero_or_unreadable_peak_fails(self):
        for value in (0, "NaN"):
            with self.subTest(value=value):
                self.assertFails(self.judge([metrics(in_progress=value)], metrics()),
                                 "no regular in-progress peak of at least 1")

    def test_no_compaction_during_ingest_fails(self):
        self.assertFails(self.judge([metrics()], metrics(), work_events=WORK_EVENTS[1:]),
                         "compaction_complete events while the senders delivered: None")

    def test_no_rollup_during_ingest_fails(self):
        self.assertFails(self.judge([metrics()], metrics(), work_events=WORK_EVENTS[:1]),
                         "rollup_complete events while the senders delivered: None")

    def test_rollup_after_delivery_fails(self):
        late = log_line(at="2026-10-08T12:05:00.000000Z", event_type="rollup_complete")
        self.assertFails(self.judge([metrics()], metrics(), work_events=(WORK_EVENTS[0], late)),
                         "rollup_complete events while the senders delivered: 0")

    def test_query_failures_fail(self):
        for status in (0, 400, 500, 503, 504):
            with self.subTest(status=status):
                self.assertFails(self.judge([metrics()], metrics(),
                                            queries=(query(), query(status))),
                                 "search queries not all answered 200")

    def test_no_queries_fails(self):
        self.assertFails(self.judge([metrics()], metrics(), queries=()), "no search query ran: 0")

    def test_missing_sender_fails(self):
        self.assertFails(self.judge([metrics()], metrics(), deliveries=delivery(senders=SENDERS[:2])),
                         "vector-3 did not deliver")

    def test_short_delivery_fails(self):
        self.assertFails(self.judge([metrics()], metrics(), deliveries=delivery(stored=99)),
                         "vector-1 did not deliver: wrote 100, stored 99 distinct")

    def test_feed_error_fails(self):
        broken = dict(delivery(), feedErrors={"vector-2": "Vector closed stdin"})
        self.assertFails(self.judge([metrics()], metrics(), deliveries=broken),
                         "a sender stopped reading its input")

    def test_check_summary_applies_the_same_check(self):
        code, summary = self.judge([metrics()], metrics())
        self.assertEqual(code, 0)
        path = self.scratch / "out/summary.json"
        with contextlib.redirect_stdout(io.StringIO()) as printed:
            self.assertEqual(run.check_summary(path), 0)
        self.assertIn("pass check: passed", printed.getvalue())
        self.assertIn(f"run.py git blob: {run.git_blob_sha(run.HERE / 'run.py')}",
                      printed.getvalue())
        broken = json.loads(path.read_text())
        broken["compactionAndRollup"].pop("rollup_complete")
        path.write_text(json.dumps(broken))
        with contextlib.redirect_stdout(io.StringIO()) as printed:
            self.assertEqual(run.check_summary(path), 1)
        self.assertIn("pass check: failed", printed.getvalue())
        self.assertIn("rollup_complete events while the senders delivered", printed.getvalue())


def docker_stub(*answers):
    """A subprocess.run stand-in that answers each docker call in turn with
    (returncode, stderr), and records the calls."""
    calls = []
    queue = list(answers)

    def fake(argv, **_):
        calls.append(argv[len(run.DOCKER):])
        code, stderr = queue.pop(0)
        return subprocess.CompletedProcess(argv, code, "", stderr)
    return fake, calls


class ContainerCleanup(Scratch):
    """Only a removal or docker's own "no such container" counts as clean."""

    def clean(self, *answers):
        """Run.cleanup() with docker answering (returncode, stderr) in turn.
        "{}" in a stderr becomes this run's container name."""
        r = run.Run(self.args())
        fake, calls = docker_stub(*[(code, err.format(r.container)) for code, err in answers])
        with mock.patch.object(run.subprocess, "run", fake):
            r.cleanup()
        self.assertEqual(calls[0], ["inspect", "--type", "container", r.container])
        return r.summary["cleanup"], calls, r

    def test_removed(self):
        cleanup, calls, _ = self.clean((0, ""), (0, ""))
        self.assertEqual((cleanup["container"], cleanup["containerState"]), (True, "removed"))
        self.assertEqual([c[0] for c in calls], ["inspect", "rm"])

    def test_no_such_container_is_clean(self):
        # Docker 29 says the first with --type container, the second without.
        for message in ("Error response from daemon: No such container: {}",
                        "error: no such object: {}"):
            with self.subTest(message=message):
                cleanup, calls, _ = self.clean((1, message))
                self.assertEqual((cleanup["container"], cleanup["containerState"]),
                                 (True, "absent"))
                self.assertEqual([c[0] for c in calls], ["inspect"])

    def test_no_such_other_container_is_not_clean(self):
        cleanup, _, _ = self.clean((1, "Error response from daemon: No such container: someone-else"))
        self.assertFalse(cleanup["container"])

    def test_daemon_error_leaves_cleanup_failed(self):
        unreachable = (1, "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. "
                          "Is the docker daemon running?")
        cleanup, calls, r = self.clean(unreachable)
        self.assertFalse(cleanup["container"])
        self.assertTrue(cleanup["containerState"].startswith("unknown: Cannot connect"))
        self.assertEqual([c[0] for c in calls], ["inspect"])
        r.summary["cleanup"]["workDir"] = "removed"
        self.assertTrue(any(reason.startswith("cleanup failed") for reason in run.verdict(r.summary)))

    def test_permission_error_leaves_cleanup_failed(self):
        denied = (1, "permission denied while trying to connect to the docker API at "
                     "unix:///var/run/docker.sock")
        cleanup, _, _ = self.clean(denied)
        self.assertFalse(cleanup["container"])

    def test_failed_removal_then_unreachable_daemon_fails(self):
        cleanup, calls, _ = self.clean((0, ""), (1, "Error response from daemon: busy"),
                                       (1, "Cannot connect to the Docker daemon"))
        self.assertFalse(cleanup["container"])
        self.assertEqual([c[0] for c in calls], ["inspect", "rm", "inspect"])

    def test_docker_missing_or_hung_fails(self):
        for error in (FileNotFoundError(2, "docker"), subprocess.TimeoutExpired("docker", 60)):
            with self.subTest(error=type(error).__name__):
                r = run.Run(self.args())

                def fake(argv, **_):
                    raise error
                with mock.patch.object(run.subprocess, "run", fake):
                    r.cleanup()
                self.assertFalse(r.summary["cleanup"]["container"])


class WorkDirectory(Scratch):
    """No refused --work is created or deleted."""

    def sentinel(self, directory):
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / "keep-me"
        path.write_text("x")
        return path

    def main(self, *argv):
        out = self.scratch / "main-out"
        saved = sys.argv
        sys.argv = ["run.py", *argv, "--vector", "/nonexistent/vector", "--out", str(out)]
        try:
            with self.assertRaises(SystemExit) as raised:
                run.main()
        finally:
            sys.argv = saved
        self.assertIn("already exists", str(raised.exception.code))
        self.assertFalse(out.exists(), "a refused --work wrote to --out")

    def test_main_leaves_an_existing_work_directory(self):
        existing = self.scratch / "previous-run"
        keep = self.sentinel(existing)
        self.main("--work", str(existing))
        self.assertTrue(keep.exists())

    def test_main_leaves_the_current_directory(self):
        here = self.scratch / "checkout"
        keep = self.sentinel(here)
        saved = os.getcwd()
        os.chdir(here)
        try:
            self.main("--work", ".")
        finally:
            os.chdir(saved)
        self.assertTrue(keep.exists())

    def test_dangerous_paths_are_refused(self):
        target = self.scratch / "target"
        keep = self.sentinel(target)
        live = self.scratch / "live-link"
        live.symlink_to(target)
        dangling = self.scratch / "dangling-link"
        dangling.symlink_to(self.scratch / "nowhere")
        through = self.scratch / "dir-link"
        through.symlink_to(target)
        for raw in ("/", str(Path.home()), str(run.ROOT), str(run.ROOT.parent), "/tmp",
                    "/tmp/ac23-work", str(run.HERE), str(run.HERE / "work"),
                    str(live), str(dangling), str(through / "work")):
            with self.subTest(raw=raw), self.assertRaises(SystemExit):
                run.check_work_path(raw)
        self.assertTrue(keep.exists())
        self.assertFalse((target / "work").exists())

    def test_claim_refuses_an_existing_directory_and_remove_leaves_it(self):
        existing = self.scratch / "existing"
        keep = self.sentinel(existing)
        r = run.Run(self.args(work=str(existing)))
        with self.assertRaises(SystemExit):
            r.claim_work()
        self.assertIsNone(r.work_owned)
        self.assertEqual(r.remove_work(), "not created by this run; left in place")
        self.assertTrue(keep.exists())

    def test_remove_deletes_the_directory_it_created(self):
        r = run.Run(self.args())
        r.claim_work()
        (r.work / "private/secret").write_text("x")
        self.assertEqual(r.remove_work(), "removed")
        self.assertFalse(r.work.exists())

    def test_remove_refuses_a_symlink_swapped_in(self):
        r = run.Run(self.args())
        r.claim_work()
        elsewhere = self.scratch / "elsewhere"
        keep = self.sentinel(elsewhere)
        os.rename(r.work, self.scratch / "moved")
        r.work.symlink_to(elsewhere)
        self.assertTrue(r.remove_work().startswith("refused"))
        self.assertTrue(keep.exists())
        self.assertTrue((self.scratch / "moved/private").exists())

    def test_remove_refuses_a_directory_swapped_in(self):
        r = run.Run(self.args())
        r.claim_work()
        os.rename(r.work, self.scratch / "moved")
        keep = self.sentinel(r.work)
        self.assertEqual(r.remove_work(), "refused: no longer the directory this run created")
        self.assertTrue(keep.exists())

    def test_group_or_other_writable_parent_is_refused(self):
        shared = self.scratch / "shared"
        shared.mkdir()
        try:
            for mode in (0o770, 0o707, 0o1777):
                with self.subTest(mode=oct(mode)):
                    shared.chmod(mode)
                    with self.assertRaises(SystemExit) as raised:
                        run.check_work_path(str(shared / "work"))
                    self.assertIn("writable by group or other", str(raised.exception.code))
                    r = run.Run(self.args(work=str(shared / "work")))
                    with self.assertRaises(SystemExit):
                        r.claim_work()
                    self.assertFalse((shared / "work").exists())
                    self.assertEqual(r.remove_work(), "not created by this run; left in place")
        finally:
            shared.chmod(0o700)

    def test_parent_owned_by_another_uid_is_refused(self):
        with mock.patch.object(run.os, "geteuid", return_value=os.geteuid() + 1):
            with self.assertRaises(SystemExit) as raised:
                run.check_work_path(str(self.scratch / "work"))
            self.assertIn("is owned by uid", str(raised.exception.code))
            r = run.Run(self.args())
            with self.assertRaises(SystemExit):
                r.claim_work()
        self.assertFalse((self.scratch / "work").exists())

    def test_missing_parent_is_created_owner_only(self):
        r = run.Run(self.args(work=str(self.scratch / "new/work")))
        r.claim_work()
        self.assertEqual(os.stat(self.scratch / "new").st_mode & 0o777, 0o700)
        self.assertEqual(r.remove_work(), "removed")

    def test_remove_refuses_once_the_parent_is_shared(self):
        r = run.Run(self.args())
        r.claim_work()
        self.scratch.chmod(0o777)
        try:
            self.assertTrue(r.remove_work().startswith("refused: --work parent"))
            self.assertTrue((r.work / "private").exists())
        finally:
            self.scratch.chmod(0o700)

    def test_out_inside_work_is_refused(self):
        r = run.Run(self.args(out=str(self.scratch / "work/out")))
        with self.assertRaises(SystemExit):
            r.claim_work()
        self.assertFalse((self.scratch / "work").exists())


class Credentials(Scratch):
    """No credential reaches a file or a command line."""

    def test_docker_run_argv_never_carries_the_password(self):
        password = "p" * 48
        argv, env = run.postgres_run("trawl-ac23-test", "ac23-run", password)
        self.assertEqual(env["POSTGRES_PASSWORD"], password)
        self.assertFalse(any(password in arg for arg in argv), argv)
        self.assertNotIn("--env-file", argv)
        # --env NAME, with no =value, for every variable docker is given.
        passed = [argv[i + 1] for i, arg in enumerate(argv) if arg == "--env"]
        self.assertEqual(sorted(passed), sorted(env))
        self.assertTrue(all("=" not in name for name in passed), passed)

    def test_start_postgres_hands_docker_the_password_through_its_environment(self):
        r = run.Run(self.args())
        r.claim_work()
        seen = []

        def fake(argv, **kwargs):
            seen.append((argv, kwargs.get("env")))
            raise RuntimeError("stop after docker run")
        with mock.patch.object(run.subprocess, "run", fake):
            with self.assertRaises(RuntimeError):
                r.start_postgres()
        argv, env = seen[0]
        password = r.secrets[0]
        self.assertFalse(any(password in arg for arg in argv), argv)
        self.assertEqual(env["POSTGRES_PASSWORD"], password)
        self.assertEqual(list((r.work / "private").iterdir()), [])
        r.remove_work()

    def test_private_file_is_owner_only_from_the_moment_it_exists(self):
        path = self.scratch / "secret"
        modes = []
        real_fdopen = os.fdopen

        def fdopen(fd, *a, **kw):
            # Before the first byte: the file exists, empty, as created.
            modes.append(os.fstat(fd).st_mode & 0o777)
            return real_fdopen(fd, *a, **kw)
        old = os.umask(0o000)
        try:
            with mock.patch.object(run.os, "fdopen", fdopen):
                run.write_private(path, "hunter2\n")
        finally:
            os.umask(old)
        self.assertEqual(modes, [0o600])
        self.assertEqual(os.stat(path).st_mode & 0o777, 0o600)
        self.assertEqual(path.read_text(), "hunter2\n")

    def test_private_file_refuses_an_existing_path(self):
        path = self.scratch / "secret"
        path.write_text("old")
        with self.assertRaises(FileExistsError):
            run.write_private(path, "new")
        self.assertEqual(path.read_text(), "old")

    def test_private_file_refuses_a_symlink(self):
        target = self.scratch / "target"
        for link_to in (target, self.scratch / "nowhere"):
            with self.subTest(exists=link_to.exists()):
                link = self.scratch / "link"
                link.symlink_to(link_to)
                if link_to == target:
                    target.write_text("keep")
                with self.assertRaises(OSError):
                    run.write_private(link, "secret")
                self.assertFalse((self.scratch / "nowhere").exists())
                if link_to == target:
                    self.assertEqual(target.read_text(), "keep")
                link.unlink()

    def test_trawld_and_vector_configs_are_written_through_write_private(self):
        r = run.Run(self.args())
        r.claim_work()
        written = []
        real = run.write_private

        def spy(path, text):
            written.append(Path(path).name)
            real(path, text)
        r.fleet_dsn = r.app_dsn = "postgres://x"
        r.ingest_keys = ["k1", "k2", "k3"]
        r.port, r.cert = 1, Path("/cert.pem")
        # trawld "exits" at once, so start_trawld stops after the config write.
        with mock.patch.object(run, "write_private", spy), \
                mock.patch.object(r, "spawn", return_value=mock.Mock(poll=lambda: 1)):
            with self.assertRaises(RuntimeError):
                r.start_trawld("trawld.log")
            r.start_vectors()
        self.assertEqual(written, ["trawld.toml", "vector-1.json", "vector-2.json", "vector-3.json"])
        for name in written:
            self.assertEqual(os.stat(r.work / "private" / name).st_mode & 0o777, 0o600)
        r.remove_work()


if __name__ == "__main__":
    unittest.main(verbosity=2)
