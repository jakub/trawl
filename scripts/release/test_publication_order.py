"""Parse the real release YAML and exercise its failure propagation graph.

Helm supplies the existing YAML parser; tests do not need a Python YAML package.
No publishing action or workflow shell command is executed.
"""
import copy
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

WORKFLOW = Path(__file__).resolve().parents[2] / '.github/workflows/release.yml'
ANNOUNCEMENTS = ('release', 'publish-docs', 'publish-apt')
FAILURES = ('build-chart', 'build-linux', 'build-macos', 'build-docs', 'docker', 'publish-helm')


def parse_workflow(path=WORKFLOW):
    with tempfile.TemporaryDirectory(prefix='trawl-publication-dag-') as directory:
        root = Path(directory)
        (root / 'templates').mkdir()
        (root / 'Chart.yaml').write_text('apiVersion: v2\nname: publication-test\nversion: 0.0.0\n')
        (root / 'release.yml').write_bytes(path.read_bytes())
        (root / 'templates/graph.yaml').write_text(
            'apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: graph\ndata:\n'
            '  graph: {{ .Files.Get "release.yml" | fromYaml | toJson | quote }}\n'
        )
        rendered = subprocess.check_output(['helm', 'template', 'dag-test', str(root)], text=True)
        encoded = next(line.removeprefix('  graph: ') for line in rendered.splitlines() if line.startswith('  graph: '))
        parsed = json.loads(json.loads(encoded))
        assert 'jobs' in parsed, parsed
        return parsed


def dependencies(job):
    needs = job.get('needs', [])
    return [needs] if isinstance(needs, str) else needs


def simulate(jobs, failed=None):
    # These release jobs use GitHub's default success() dependency condition.
    # An explicit override or continue-on-error would invalidate that model,
    # so refuse it instead of silently claiming the same failure behavior.
    for name, job in jobs.items():
        assert not job.get('if'), f'{name}: explicit job condition needs review'
        assert not job.get('continue-on-error'), f'{name}: failure cannot be ignored'
    remaining = set(jobs)
    status = {}
    while remaining:
        progress = False
        for name in sorted(remaining):
            needs = dependencies(jobs[name])
            assert set(needs) <= jobs.keys(), f'{name}: missing dependency'
            if not all(n in status for n in needs):
                continue
            if any(status[n] != 'success' for n in needs):
                status[name] = 'skipped'
            else:
                status[name] = 'failure' if name == failed else 'success'
            remaining.remove(name)
            progress = True
        assert progress, 'workflow dependency cycle'
    return status


def assert_publication_order(jobs):
    for failed in FAILURES:
        status = simulate(jobs, failed)
        assert status[failed] == 'failure', f'{failed}: failure case never reached'
        for job in ANNOUNCEMENTS:
            assert status[job] == 'skipped', f'{job} can start after {failed} fails'
        if failed == 'build-chart':
            assert status['docker'] == 'skipped', 'registry write can start before chart preparation succeeds'
        if failed == 'docker':
            assert status['publish-helm'] == 'skipped', 'chart publication can start before image publication succeeds'
    assert all(value == 'success' for value in simulate(jobs).values())


class PublicationOrder(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.workflow = parse_workflow()

    def test_prep_and_registry_failures_block_all_announcements(self):
        assert_publication_order(self.workflow['jobs'])

    def test_original_parallel_announcement_defect_is_detected(self):
        jobs = copy.deepcopy(self.workflow['jobs'])
        jobs['release']['needs'] = ['resolve-source', 'build-linux', 'build-macos', 'build-docs']
        with self.assertRaisesRegex(AssertionError, 'release can start after'):
            assert_publication_order(jobs)

    def test_missing_chart_preflight_dependency_is_detected(self):
        jobs = copy.deepcopy(self.workflow['jobs'])
        jobs['docker']['needs'].remove('build-chart')
        with self.assertRaisesRegex(AssertionError, 'registry write can start before chart preparation'):
            assert_publication_order(jobs)

    def test_native_linux_failures_fail_the_called_release_gate(self):
        jobs = self.workflow['jobs']
        self.assertEqual(jobs['build-linux']['uses'], './.github/workflows/linux-distribution.yml')
        linux = parse_workflow(WORKFLOW.with_name('linux-distribution.yml'))
        self.assertEqual(linux['permissions'], {'contents': 'read'})
        self.assertEqual(set(linux['jobs']), {'build', 'verify'})
        self.assertEqual(dependencies(linux['jobs']['verify']), ['build'])
        source_check = next(s for s in linux['jobs']['build']['steps'] if s.get('name') == 'Verify committed release source and version')
        self.assertIn('test "$(git -C .release-tooling rev-parse HEAD)" = "$TOOLING_SHA"', source_check['run'])
        for failed in ('build', 'verify'):
            with self.subTest(failed=failed):
                native_status = simulate(linux['jobs'], failed)
                self.assertEqual(native_status[failed], 'failure')
                # A failed job makes the workflow_call fail; there is no
                # continue-on-error or condition bypass in the called graph.
                release_status = simulate(jobs, 'build-linux')
                for announcement in ANNOUNCEMENTS:
                    self.assertEqual(release_status[announcement], 'skipped')
        self.assertTrue(all(s == 'success' for s in simulate(linux['jobs']).values()))

    def test_both_native_preflights_accept_identical_required_inputs(self):
        for filename in ('linux-distribution.yml', 'macos-cli.yml'):
            workflow = parse_workflow(WORKFLOW.with_name(filename))
            # Helm's YAML 1.1 parser spells the unquoted `on` mapping key true.
            triggers = workflow.get('on') or workflow.get('true')
            self.assertEqual(set(triggers), {'workflow_call', 'workflow_dispatch'})
            for trigger in triggers.values():
                self.assertEqual(set(trigger['inputs']), {'source-sha', 'tooling-sha', 'release-tag'})
                for field in trigger['inputs'].values():
                    self.assertTrue(field['required'])
                    self.assertEqual(field['type'], 'string')

    def test_pr_preflight_calls_both_native_checks_without_publication(self):
        workflow = parse_workflow(WORKFLOW.with_name('distribution-preflight.yml'))
        triggers = workflow.get('on') or workflow.get('true')
        self.assertEqual(set(triggers), {'pull_request'})
        self.assertEqual(set(triggers['pull_request']['paths']), {
            '.github/workflows/release.yml', '.github/workflows/linux-distribution.yml',
            '.github/workflows/macos-cli.yml', '.github/workflows/distribution-preflight.yml',
            'scripts/release/**', 'Cargo.lock', 'Cargo.toml',
            'crates/trawl-cli/Cargo.toml', 'crates/trawl-core/build.rs',
            'crates/trawl-core/build_support/**',
            'crates/trawl-core/Cargo.toml', 'crates/trawl-engine/Cargo.toml',
            'crates/trawl-server/Cargo.toml',
        })
        self.assertEqual(workflow['permissions'], {'contents': 'read'})
        jobs = workflow['jobs']
        self.assertEqual(set(jobs), {'metadata', 'linux', 'macos'})
        metadata = jobs['metadata']
        self.assertEqual(metadata['outputs'], {
            'sha': '${{ steps.source.outputs.sha }}',
            'release-tag': '${{ steps.source.outputs.release-tag }}',
        })
        checkout = metadata['steps'][0]
        self.assertTrue(checkout['uses'].startswith('actions/checkout@'))
        self.assertEqual(checkout['with'], {'ref': '${{ github.sha }}', 'persist-credentials': False})
        self.assertEqual(len(metadata['steps']), 2)
        for job, filename in [('linux', 'linux-distribution.yml'), ('macos', 'macos-cli.yml')]:
            self.assertEqual(jobs[job], {
                'needs': 'metadata', 'uses': './.github/workflows/' + filename,
                'with': {'source-sha': '${{ needs.metadata.outputs.sha }}',
                         'tooling-sha': '${{ needs.metadata.outputs.sha }}',
                         'release-tag': '${{ needs.metadata.outputs.release-tag }}'},
            })
        self.assertFalse(any('permissions' in job for job in jobs.values()))
        self.assertEqual(simulate(jobs, 'metadata'),
                         {'metadata': 'failure', 'linux': 'skipped', 'macos': 'skipped'})

    def test_crashdump_image_checks_native_build_helpers_on_pr_and_push(self):
        workflow = parse_workflow(WORKFLOW.with_name('crashdump-image.yml'))
        triggers = workflow.get('on') or workflow.get('true')
        expected = {
            'Dockerfile', 'scripts/release/build-distribution.sh',
            'scripts/release/distribution.py', 'scripts/release/duckdb-runtime.json',
            'scripts/release/duckdb-LICENSE', 'crates/trawl-core/build.rs',
            'crates/trawl-core/build_support/**', 'crates/trawl-crashdump/**',
            'crates/trawl-server/src/main.rs', 'Cargo.lock',
            '.github/workflows/crashdump-image.yml', 'ci/crashdump-image.sh',
        }
        for event in ('pull_request', 'push'):
            with self.subTest(event=event):
                self.assertEqual(set(triggers[event]['paths']), expected)

    def test_pr_metadata_reads_committed_version_and_refuses_wrong_sha(self):
        workflow = parse_workflow(WORKFLOW.with_name('distribution-preflight.yml'))
        step = workflow['jobs']['metadata']['steps'][1]
        self.assertEqual(step['id'], 'source')
        self.assertEqual(step['env'], {'EXPECTED_SHA': '${{ github.sha }}'})
        with tempfile.TemporaryDirectory(prefix='trawl-preflight-source-') as directory:
            root = Path(directory)
            env = dict(os.environ, GIT_CONFIG_NOSYSTEM='1', GIT_CONFIG_GLOBAL=os.devnull)
            subprocess.run(['git', 'init', '-q', str(root)], env=env, check=True)
            (root / 'Cargo.toml').write_text('[workspace.package]\nversion = "0.4.0"\n')
            subprocess.run(['git', 'add', 'Cargo.toml'], cwd=root, env=env, check=True)
            subprocess.run(['git', '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid',
                            'commit', '-qm', 'committed source'], cwd=root, env=env, check=True)
            sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, env=env, text=True).strip()
            output = root / 'outputs'
            result = subprocess.run(['bash', '-e', '-c', step['run']], cwd=root,
                                    env=dict(env, EXPECTED_SHA=sha, GITHUB_OUTPUT=str(output)),
                                    text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(output.read_text(), f'sha={sha}\nrelease-tag=v0.4.0\n')
            output.unlink()
            result = subprocess.run(['bash', '-e', '-c', step['run']], cwd=root,
                                    env=dict(env, EXPECTED_SHA='0' * 40, GITHUB_OUTPUT=str(output)),
                                    text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('does not match the PR commit', result.stderr)
            self.assertFalse(output.exists())

    def test_curated_announcement_is_prepared_before_registry_publication(self):
        jobs = self.workflow['jobs']
        prepare = jobs['build-chart']
        self.assertEqual(prepare['env']['SOURCE_SHA'], '${{ needs.resolve-source.outputs.sha }}')
        upload = next(s for s in prepare['steps'] if s.get('with', {}).get('name') == 'release-announcement')
        self.assertEqual(upload['with']['path'], 'announcement-artifact/')
        self.assertEqual(upload['with']['if-no-files-found'], 'error')
        self.assertIn('build-chart', dependencies(jobs['docker']))
        steps = jobs['release']['steps']
        download = next(s for s in steps if s.get('with', {}).get('name') == 'release-announcement')
        self.assertEqual(download['with']['path'], 'announcement')
        verify = next(s for s in steps if s.get('name') == 'Verify exact prepared announcement')
        publish = next(s for s in steps if s.get('name') == 'Create release')
        self.assertLess(steps.index(download), steps.index(verify))
        self.assertLess(steps.index(verify), steps.index(publish))
        self.assertIn('sha256sum --check SHA256SUMS', verify['run'])
        self.assertIs(publish['with']['generate_release_notes'], False)
        self.assertEqual(publish['with']['body_path'], 'announcement/body.md')
        self.assertNotIn('body', publish['with'])

    def test_publisher_consumes_exact_prebuilt_chart_and_metadata(self):
        jobs = self.workflow['jobs']
        prepare = jobs['build-chart']
        publish = jobs['publish-helm']
        self.assertEqual(prepare['permissions'], {'contents': 'read'})
        uploaded = next(s['with']['name'] for s in prepare['steps'] if s.get('uses', '').startswith('actions/upload-artifact@') and s['with']['name'] == 'release-chart')
        downloaded = next(s['with']['name'] for s in publish['steps'] if s.get('uses', '').startswith('actions/download-artifact@'))
        self.assertEqual(uploaded, downloaded)
        prepare_commands = '\n'.join(s.get('run', '') for s in prepare['steps'])
        publish_commands = '\n'.join(s.get('run', '') for s in publish['steps'])
        self.assertNotIn('helm push', prepare_commands)
        self.assertIn('helm lint', prepare_commands)
        self.assertIn('helm template', prepare_commands)
        self.assertIn('sha256sum -c SHA256SUMS', publish_commands)
        self.assertIn('helm push chart-artifact/', publish_commands)
        self.assertNotIn('package-chart.sh', publish_commands)
        self.assertNotIn('helm package', publish_commands)
        checkout = next(s for s in prepare['steps'] if s.get('uses', '').startswith('actions/checkout@'))
        self.assertEqual(checkout['with']['ref'], '${{ needs.resolve-source.outputs.sha }}')
        tooling_checkout = next(s for s in prepare['steps'] if s.get('with', {}).get('path') == '.release-tooling')
        self.assertEqual(tooling_checkout['with']['ref'], '${{ github.sha }}')
        self.assertFalse(tooling_checkout['with']['persist-credentials'])
        self.assertIn('bash .release-tooling/scripts/release/package-chart.sh', prepare_commands)
        self.assertNotIn('git fetch', prepare_commands)
        docker_build = next(s for s in jobs['docker']['steps'] if s.get('uses', '').startswith('docker/build-push-action@'))
        self.assertEqual(docker_build['with']['tags'], '${{ needs.build-chart.outputs.image-tags }}')
        self.assertEqual(docker_build['with']['labels'], '${{ needs.build-chart.outputs.image-labels }}')


if __name__ == '__main__':
    unittest.main()
