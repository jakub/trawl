#!/usr/bin/env python3
"""Exercise an installed CLI against a fixed Parquet corpus, without a server."""

import argparse
import json
import re
from pathlib import Path
import subprocess
import tempfile


def verify_version(version: str, expected_version: str | None, expected_source_sha: str | None) -> None:
    assert version.startswith("trawl "), version
    if expected_version is not None:
        assert version.split()[1] == expected_version, version
    if expected_source_sha is not None:
        assert re.fullmatch(r"[0-9a-f]{40}", expected_source_sha), "expected source SHA must be a full commit SHA"
        embedded = re.match(r"^trawl \S+ \(([0-9a-f]{7,40})(\*?) ", version)
        assert embedded, f"missing embedded source provenance: {version}"
        assert not embedded[2], f"binary was built from dirty tracked source: {version}"
        assert expected_source_sha.startswith(embedded[1]), f"embedded commit does not match {expected_source_sha}: {version}"


def smoke(binary: Path, fixture: Path, expected_version: str | None = None, expected_source_sha: str | None = None) -> None:
    binary = binary.resolve(strict=True)
    fixture = fixture.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="trawl-cli-smoke-") as temporary:
        work = Path(temporary)
        # Use an explicit empty config and a minimal environment so the test
        # cannot select the runner's saved server, credentials, or loader path.
        config = work / "client.toml"
        config.write_text("")
        environment = {"PATH": "/usr/bin:/bin", "HOME": str(work), "LANG": "C", "TERM": "dumb"}

        def run(*arguments: str) -> str:
            return subprocess.check_output(
                [str(binary), "--config", str(config), *arguments],
                cwd=work, env=environment, text=True, timeout=60,
            )

        version = run("--version").strip()
        verify_version(version, expected_version, expected_source_sha)
        assert "query" in run("--help")
        rows = run(
            "query", "--data", str(fixture), "--format", "json",
            "* | sort id",
        )
        expected = [
            {"id": 1, "service": "launch", "message": "ready", "duration_ms": 10},
            {"id": 2, "service": "launch", "message": "request failed", "duration_ms": 30},
            {"id": 3, "service": "worker", "message": "complete", "duration_ms": 20},
        ]
        assert [json.loads(line) for line in rows.splitlines()] == expected, rows
        filtered = run(
            "query", "--data", str(fixture), "--format", "json",
            "service=launch | stats count() by service",
        )
        assert json.loads(filtered) == {"service": "launch", "count": 2}, filtered
        exported = work / "export.parquet"
        run(
            "query", "--data", str(fixture), "--format", "parquet",
            "--output", str(exported), "service=launch",
        )
        assert exported.is_file() and exported.stat().st_size > 0
        roundtrip = run(
            "query", "--data", str(exported), "--format", "json", "* | sort id",
        )
        assert [json.loads(line) for line in roundtrip.splitlines()] == expected[:2], roundtrip
        assert {path.name for path in work.iterdir()} == {"client.toml", "export.parquet"}, "query created unexpected files, such as an extension download"
        print(json.dumps({"version": version, "input_rows": 3, "export_rows": 2, "status": "passed"}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("fixture", type=Path)
    parser.add_argument("--expected-version", help="Required package version, without the v prefix")
    parser.add_argument("--expected-source-sha", help="Require clean embedded provenance matching this full commit SHA")
    args = parser.parse_args()
    smoke(args.binary, args.fixture, args.expected_version, args.expected_source_sha)
