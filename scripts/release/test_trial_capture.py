"""test-trial.sh prints no captured byte before its secrets are scanned.

The proof runs against a stub `docker` and a stub `trawl` that play a
regression: `trawl trial up` writes a token file and prints the token while
it fails, or `trawl trial down`, run by the exit trap, prints it. Under
GitHub Actions the only lines of the job log that may hold the token are
its `::add-mask::` lines, and the evidence log may not hold it at all.

Ports 15514, 18090, 25514, and 28090 must be free, as for a real proof.
"""
import os
from pathlib import Path
import secrets
import shutil
import subprocess
import tempfile
import unittest

HERE = Path(__file__).resolve().parent
SCRIPT = HERE / "test-trial.sh"

DOCKER = r"""#!/usr/bin/env bash
# A Docker engine with no trial on it, until the stub trawl writes
# state.json: then one trial container is listed.
state="$(dirname "$DOCKER_CONFIG")/state/trawl/trial/state.json"
case "$1 $2" in
  "version --format") echo 27.0.0 ;;
  "compose version") echo "Docker Compose version v2.29.0" ;;
  "image inspect") echo sha256:0000 ;;
  "volume create") echo "$3" ;;
  "container create") echo 0123456789ab ;;
  "ps -a")
    if [[ -e "$state" && "$*" == *"--format container"* && "$*" == *"project=trawl-trial"* ]]; then
      echo "container trawl-trial-postgres-1 id=f00d"
    fi ;;
esac
exit 0
"""

TRAWL = r"""#!/usr/bin/env bash
dir="$XDG_STATE_HOME/trawl/trial"
case "$*" in
  --version) echo "trawl 9.9.9" ;;
  "trial up"*)
    mkdir -p "$dir"
    printf '%s\n' "$STUB_TOKEN" >"$dir/operator.token"
    if [[ $STUB_LEAK == up ]]; then
      echo "minting the trial-operator key: $STUB_TOKEN" >&2
      echo up >>"$STUB_LEAKED"
    else
      printf '{"trial_id": "f00d", "keys": {"operator": null, "ingest": null}}\n' >"$dir/state.json"
    fi
    exit 1 ;;
  "trial down --yes")
    echo "revoking $(cat "$dir/operator.token")"
    echo down >>"$STUB_LEAKED"
    rm -rf "$dir" ;;
esac
"""


class Capture(unittest.TestCase):
    def run_proof(self, leak):
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root)
        bin_dir = root / "bin"
        bin_dir.mkdir()
        for name, body in (("docker", DOCKER), ("trawl", TRAWL)):
            (bin_dir / name).write_text(body)
            (bin_dir / name).chmod(0o755)
        token = "flt_" + secrets.token_urlsafe(32)
        env = {k: v for k, v in os.environ.items()
               if k not in ("DOCKER_HOST", "DOCKER_CONTEXT", "TRIAL_IMAGE", "TRIAL_BROWSER")}
        leaked = root / "leaked"
        env.update(PATH=f"{bin_dir}:{env['PATH']}", GITHUB_ACTIONS="true", STUB_TOKEN=token, STUB_LEAK=leak,
                   STUB_LEAKED=str(leaked))
        result = subprocess.run(["bash", str(SCRIPT), str(bin_dir / "trawl"), str(root / "evidence"),
                                 str(root / "private")], env=env, capture_output=True, text=True, timeout=120)
        self.assertTrue(leaked.exists(), "the stub did not leak: " + result.stdout + result.stderr)
        return token, result, root

    def assert_gated(self, token, result, root):
        self.assertNotEqual(result.returncode, 0, result.stdout)
        log = result.stdout + result.stderr
        leaked = [line for line in log.splitlines() if token in line and not line.startswith("::add-mask::")]
        self.assertEqual(leaked, [], "the token reached the job log unmasked")
        self.assertIn(f"::add-mask::{token}", log.splitlines())
        self.assertIn("FOUND operator token", result.stdout)
        self.assertIn("::error::the output holds a secret value", result.stdout)
        self.assertNotIn(token, (root / "evidence/trial.log").read_text())

    def test_a_secret_printed_by_a_failing_command_is_withheld(self):
        self.assert_gated(*self.run_proof("up"))

    def test_a_secret_printed_by_the_exit_trap_is_withheld(self):
        token, result, root = self.run_proof("down")
        self.assert_gated(token, result, root)
        # Output released before the leak still reached the log.
        self.assertIn("==== preconditions", result.stdout)


if __name__ == "__main__":
    unittest.main()
