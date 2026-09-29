"""Checks ci/changed-scopes.py against this repository's real manifests."""
import importlib.util
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location('changed_scopes', HERE / 'changed-scopes.py')
scopes = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scopes)
ROOT = scopes.ROOT


class CrateClosure(unittest.TestCase):
    def test_the_spa_compiles_its_shared_crates_and_no_server_crate(self):
        closure = scopes.crate_closure(ROOT, 'crates/trawl-web-ui')
        self.assertLessEqual(
            {'crates/trawl-web-ui', 'crates/trawl-api', 'crates/fleet-ui', 'crates/trawl-core'},
            closure)
        self.assertNotIn('crates/trawl-server', closure)
        self.assertNotIn('crates/trawl-engine', closure)

    def test_the_server_follows_workspace_and_dev_dependencies(self):
        closure = scopes.crate_closure(ROOT, 'crates/trawl-server')
        # trawl-client and trawl-cli arrive only as dev-dependencies.
        self.assertLessEqual(
            {'crates/trawl-server', 'crates/trawl-core', 'crates/trawl-engine',
             'crates/fleet-auth', 'crates/trawl-client', 'crates/trawl-cli'},
            closure)
        self.assertNotIn('crates/trawl-web-ui', closure)


class Scopes(unittest.TestCase):
    def scopes(self, *paths):
        return scopes.scopes_for(ROOT, list(paths))

    def test_docs_alone_need_no_mutation_job(self):
        self.assertEqual(self.scopes('docs/src/content/docs/index.md', 'README.md'),
                         {'web_mutations': False, 'server_mutations': False})

    def test_a_server_change_skips_the_web_mutations(self):
        self.assertEqual(self.scopes('crates/trawl-server/src/main.rs'),
                         {'web_mutations': False, 'server_mutations': True})

    def test_an_e2e_change_skips_the_server_mutations(self):
        self.assertEqual(self.scopes('crates/trawl-web-ui/e2e/tests/routing.spec.ts'),
                         {'web_mutations': True, 'server_mutations': False})

    def test_a_shared_crate_runs_both(self):
        self.assertEqual(self.scopes('crates/trawl-core/src/lib.rs'),
                         {'web_mutations': True, 'server_mutations': True})

    def test_build_wide_files_run_both(self):
        for path in ('Cargo.lock', 'rust-toolchain.toml', '.github/workflows/ci.yml',
                     '.cargo/config.toml', 'ci/changed-scopes.py'):
            with self.subTest(path=path):
                self.assertEqual(self.scopes(path),
                                 {'web_mutations': True, 'server_mutations': True})

    def test_server_only_inputs_outside_the_crates_gate_only_the_server(self):
        for path in ('.config/nextest.toml', 'ci/postgres-no-durability.sh',
                     'scripts/release/distribution.py',
                     'scripts/release/duckdb-runtime.json',
                     'scripts/release/duckdb-LICENSE'):
            with self.subTest(path=path):
                self.assertEqual(self.scopes(path),
                                 {'web_mutations': False, 'server_mutations': True})

    def test_a_crate_name_prefix_is_not_the_crate(self):
        # crates/trawl-web is the browser proxy, not crates/trawl-web-ui.
        self.assertFalse(self.scopes('crates/trawl-web/src/lib.rs')['web_mutations'])


if __name__ == '__main__':
    unittest.main()
