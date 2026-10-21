"""Check container selection and isolation without starting Docker or a service."""
# SPDX-License-Identifier: Apache-2.0
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]
RUNNER = REPO / 'tests/containers/run.sh'


class ContainerRunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        tools = self.root / 'tools'
        tools.mkdir()
        self.log = self.root / 'docker-calls'
        docker = tools / 'docker'
        docker.write_text(f'#!{sys.executable}\nimport json, os, sys\n'
                          'with open(os.environ["DOCKER_CALLS"], "a") as log:\n'
                          '    log.write(json.dumps(sys.argv[1:]) + "\\n")\n')
        docker.chmod(0o755)
        binary = self.root / 'static-fixture'
        binary.write_text('This path is mounted, never executed by the runner.\n')
        binary.chmod(0o755)
        self.env = dict(os.environ, PATH=str(tools) + ':' + os.environ['PATH'],
                        DOCKER_CALLS=str(self.log), MINIBOOKX_TEST_BINARY=str(binary))

    def command(self, *arguments, **changes):
        return subprocess.run(['/bin/sh', str(RUNNER), *arguments], env=dict(self.env, **changes),
                              text=True, capture_output=True, timeout=5)

    def calls(self):
        return [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def test_help_and_invalid_selection_never_touch_docker(self):
        self.assertEqual(self.command('--help', MINIBOOKX_TEST_BINARY='/missing').returncode, 0)
        self.assertNotEqual(self.command('debian', 'unrecognized').returncode, 0)
        self.assertEqual(self.calls(), [])

    def test_full_matrix_uses_read_only_repo_and_binary_mounts(self):
        result = self.command()
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.calls()
        expected = ['debian:13-slim', 'ubuntu:24.04', 'ubuntu:26.04', 'fedora:43',
                    'fedora:44', 'archlinux:base', 'cachyos/cachyos:latest']
        self.assertEqual([args[1] for args in calls if args[0] == 'pull'], expected)
        runs = [args for args in calls if args[0] == 'run']
        self.assertEqual(len(runs), len(expected))
        for args in runs:
            mounts = [args[index + 1] for index, value in enumerate(args) if value == '--mount']
            self.assertEqual(len(mounts), 2)
            self.assertIn(f'type=bind,source={REPO},target=/source,readonly', mounts)
            self.assertTrue(all(mount.endswith(',readonly') for mount in mounts))
            self.assertIn('--pids-limit=256', args)
            self.assertIn('/source/tests/containers/check.sh', args[-3])
            self.assertFalse(any(value.startswith(('--privileged', '--device', '--cap-add')) for value in args))


if __name__ == '__main__':
    unittest.main()
