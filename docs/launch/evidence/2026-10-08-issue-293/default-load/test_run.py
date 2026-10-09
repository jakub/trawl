#!/usr/bin/env python3
"""Checks for run.py that need no build, database or Vector.

* The pass predicate: the real Run.collect() and verdict() over in-memory
  /metrics bodies and a written trawld log.
* The --work rules: a refused path is never created or deleted, and
  Run.remove_work deletes only the directory its own claim created.

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
import sys
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path

sys.dont_write_bytecode = True
HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("run", HERE / "run.py")
run = importlib.util.module_from_spec(spec)
spec.loader.exec_module(run)

SCRATCH_ROOT = run.ROOT / ".flow-scratch"


def metrics(regular=0, control=0):
    """A /metrics body. None leaves a refusal series out; a string is
    written verbatim as the value."""
    lines = [
        "# TYPE trawl_http_requests_in_progress gauge",
        'trawl_http_requests_in_progress{allowance="regular"} 3',
        'trawl_http_requests_in_progress{allowance="control"} 1',
        "# TYPE trawl_http_requests_refused_total counter",
    ]
    for allowance, value in (("regular", regular), ("control", control)):
        if value is not None:
            lines.append(f'trawl_http_requests_refused_total{{allowance="{allowance}"}} {value}')
    lines += [
        'trawl_http_request_allowance{allowance="regular"} 32',
        'trawl_http_request_allowance{allowance="control"} 4',
        "trawl_hot_buffer_events 10",
    ]
    return "\n".join(lines) + "\n"


def log_line(**fields):
    body = " ".join(f'{k}="{v}"' if isinstance(v, str) else f"{k}={v}" for k, v in fields.items())
    return f"2026-10-08T12:00:01.000000Z  WARN trawl_server::http: request failed {body}"


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

    def judge(self, samples, final, log_lines=(), collect_error=False):
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
        r.trawld_log = self.scratch / "trawld.log"
        if not collect_error:
            r.trawld_log.write_text("".join(line + "\n" for line in log_lines))
        r.summary.update({
            "delivery": {"complete": True},
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

    def test_out_inside_work_is_refused(self):
        r = run.Run(self.args(out=str(self.scratch / "work/out")))
        with self.assertRaises(SystemExit):
            r.claim_work()
        self.assertFalse((self.scratch / "work").exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
