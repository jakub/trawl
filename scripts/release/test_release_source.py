#!/usr/bin/env python3
"""Release identity and version checks against real committed Git fixtures."""
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("check_release", Path(__file__).with_name("check-release-source.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class ReleaseSource(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / 'crates/cli').mkdir(parents=True)
        (self.root / 'Cargo.toml').write_text('[workspace]\nmembers=["crates/*"]\n[workspace.package]\nversion="1.0.0"\n')
        (self.root / 'crates/cli/Cargo.toml').write_text('[package]\nname="cli"\nversion.workspace=true\n')
        (self.root / 'Cargo.lock').write_text('[[package]]\nname="cli"\nversion="1.0.0"\n')
        self.git('init', '-q')
        self.git('config', 'user.name', 'Fixture')
        self.git('config', 'user.email', 'fixture@example.invalid')
        self.commit()

    def git(self, *args):
        return subprocess.check_output(['git', '-C', str(self.root), *args], text=True).strip()

    def commit(self):
        self.git('add', '.')
        self.git('commit', '-qm', 'fixture')
        self.sha = self.git('rev-parse', 'HEAD')

    def test_matching_committed_source_is_accepted_without_writes(self):
        before = (self.root / 'Cargo.lock').read_bytes()
        module.check(self.root, 'v1.0.0', self.sha)
        self.assertEqual(self.git('status', '--porcelain'), '')
        self.assertEqual((self.root / 'Cargo.lock').read_bytes(), before)

    def test_tag_and_sha_mismatch_are_refused(self):
        for tag, sha, error in [('v1.0.1', self.sha, 'workspace version'), ('v1.0.0', '0' * 40, 'source SHA')]:
            with self.subTest(tag=tag, sha=sha), self.assertRaisesRegex(SystemExit, error):
                module.check(self.root, tag, sha)

    def test_dirty_or_committed_lock_mismatch_is_refused(self):
        (self.root / 'Cargo.lock').write_text('[[package]]\nname="cli"\nversion="0.9.0"\n')
        with self.assertRaisesRegex(SystemExit, 'tracked changes'):
            module.check(self.root, 'v1.0.0', self.sha)
        self.commit()
        with self.assertRaisesRegex(SystemExit, 'Cargo.lock'):
            module.check(self.root, 'v1.0.0', self.sha)


if __name__ == '__main__':
    unittest.main()
