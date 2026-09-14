"""Parse the real release YAML and exercise its failure propagation graph.

Helm supplies the existing YAML parser; tests do not need a Python YAML package.
No publishing action or workflow shell command is executed.
"""
import copy
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

WORKFLOW = Path(__file__).resolve().parents[2] / '.github/workflows/release.yml'
ANNOUNCEMENTS = ('release', 'publish-docs', 'publish-apt')
FAILURES = ('build-chart', 'build-linux', 'verify-linux', 'build-macos', 'build-docs', 'docker', 'publish-helm')


def parse_workflow():
    with tempfile.TemporaryDirectory(prefix='trawl-publication-dag-') as directory:
        root = Path(directory)
        (root / 'templates').mkdir()
        (root / 'Chart.yaml').write_text('apiVersion: v2\nname: publication-test\nversion: 0.0.0\n')
        (root / 'release.yml').write_bytes(WORKFLOW.read_bytes())
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
        jobs['release']['needs'] = ['resolve-source', 'verify-linux', 'build-macos', 'build-docs']
        with self.assertRaisesRegex(AssertionError, 'release can start after'):
            assert_publication_order(jobs)

    def test_missing_chart_preflight_dependency_is_detected(self):
        jobs = copy.deepcopy(self.workflow['jobs'])
        jobs['docker']['needs'].remove('build-chart')
        with self.assertRaisesRegex(AssertionError, 'registry write can start before chart preparation'):
            assert_publication_order(jobs)

    def test_publisher_consumes_exact_prebuilt_chart_and_metadata(self):
        jobs = self.workflow['jobs']
        prepare = jobs['build-chart']
        publish = jobs['publish-helm']
        self.assertEqual(prepare['permissions'], {'contents': 'read'})
        uploaded = next(s['with']['name'] for s in prepare['steps'] if s.get('uses', '').startswith('actions/upload-artifact@'))
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
        self.assertIn('WORKFLOW_SHA', prepare_commands)
        docker_build = next(s for s in jobs['docker']['steps'] if s.get('uses', '').startswith('docker/build-push-action@'))
        self.assertEqual(docker_build['with']['tags'], '${{ needs.build-chart.outputs.image-tags }}')
        self.assertEqual(docker_build['with']['labels'], '${{ needs.build-chart.outputs.image-labels }}')


if __name__ == '__main__':
    unittest.main()
