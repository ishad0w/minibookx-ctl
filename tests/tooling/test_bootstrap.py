#!/usr/bin/env python3
"""Exercise the piped bootstrap without network, privilege or host installation."""
# SPDX-License-Identifier: Apache-2.0
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[2]
TAG = 'v0.1.1'
ASSET = 'minibookx-ctl-x86_64-linux'
URL = 'https://github.com/ishad0w/minibookx-ctl'


class BootstrapTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.tools = self.root / 'tools'
        self.tools.mkdir()
        self.binary = b'#!/bin/sh\nprintf "%s\\n" "$@" > "$FIXTURE/binary-args"\n'
        self.manifest = hashlib.sha256(self.binary).hexdigest() + '  ' + ASSET + '\n'
        (self.root / ASSET).write_bytes(self.binary)
        (self.root / 'SHA256SUMS').write_text(self.manifest)
        (self.root / 'tag').write_text(TAG)
        (self.root / 'redirect').write_text(URL + '/releases/tag/' + TAG)
        self.env = dict(os.environ, PATH=str(self.tools) + ':' + os.environ['PATH'], FIXTURE=str(self.root), TMPDIR=str(self.root))
        self.tool('id', 'print("1000")\n')
        self.tool('sudo', '''
args = sys.argv[1:]
assert args.pop(0) == '--'
(root / 'sudo-called').write_text(json.dumps(args))
raise SystemExit(subprocess.run(args).returncode)
''')
        self.tool('curl', '''
args = sys.argv[1:]
assert args[args.index('--proto') + 1] == '=https'
assert args[args.index('--proto-redir') + 1] == '=https'
url = args[-1]
with (root / 'urls').open('a') as output: output.write(url + '\\n')
if url.endswith('/releases/latest'):
    print((root / 'redirect').read_text(), end='')
else:
    expected = 'https://github.com/ishad0w/minibookx-ctl/releases/download/' + (root / 'tag').read_text() + '/'
    assert url.startswith(expected), url
    name = url[len(expected):]
    assert name in ('SHA256SUMS', 'minibookx-ctl-x86_64-linux')
    Path(args[args.index('--output') + 1]).write_bytes((root / name).read_bytes())
''')

    def tool(self, name, code):
        path = self.tools / name
        path.write_text('#!' + sys.executable + '\nimport os, sys, json, subprocess\nfrom pathlib import Path\nroot = Path(os.environ["FIXTURE"])\n' + code)
        path.chmod(0o755)

    def run_bootstrap(self, *args, success=True):
        # stdin execution proves there is no dependency on sibling source files.
        result = subprocess.run(['/bin/sh', '-s', '--', *args], input=(REPO / 'install.sh').read_text(),
                                env=self.env, capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
        self.assertFalse(list(self.root.glob('minibookx-install.*')), 'private download directory must be removed')
        return result

    def test_latest_is_resolved_once_then_all_downloads_use_exact_tag(self):
        self.run_bootstrap()
        self.assertEqual((self.root / 'urls').read_text().splitlines(), [URL + '/releases/latest',
            URL + '/releases/download/' + TAG + '/SHA256SUMS', URL + '/releases/download/' + TAG + '/' + ASSET])
        self.assertEqual((self.root / 'binary-args').read_text().splitlines(), ['--install', '--enable'])
        self.assertTrue((self.root / 'sudo-called').is_file())

    def test_explicit_release_and_staging_need_no_sudo_or_service_enable(self):
        self.run_bootstrap('--release', TAG, '--destdir', str(self.root / 'stage'))
        self.assertFalse((self.root / 'sudo-called').exists())
        self.assertNotIn('/releases/latest', (self.root / 'urls').read_text())
        self.assertEqual((self.root / 'binary-args').read_text().splitlines(),
                         ['--install', '--destdir', str(self.root / 'stage')])

    def test_local_binary_needs_no_network_and_no_enable_is_forwarded(self):
        self.run_bootstrap('--binary', str(self.root / ASSET), '--no-enable')
        self.assertFalse((self.root / 'urls').exists())
        self.assertEqual((self.root / 'binary-args').read_text().splitlines(), ['--install'])

    def test_foreign_redirect_and_noncanonical_tag_never_reach_sudo(self):
        for redirect in ('https://example.invalid/releases/tag/' + TAG,
                         URL + '/releases/tag/' + TAG + '?x=1', URL + '/releases/tag/' + TAG + '/extra',
                         URL + '/releases/tag/0.1.1', URL + '/releases/tag/v0.01.1',
                         URL + '/releases/tag/v0.1.1-aaaaaaaaaaaa',
                         URL + '/releases/tag/v0.1.1\nextra', URL + '/releases/tag/v0.1.1-rc1'):
            (self.root / 'redirect').write_text(redirect)
            self.run_bootstrap(success=False)
        self.assertFalse((self.root / 'sudo-called').exists())
        self.assertFalse((self.root / 'binary-args').exists())

    def test_invalid_explicit_release_is_rejected_before_network_or_sudo(self):
        for tag in ('0.1.1', 'v0.01.1', 'v0.1.1-aaaaaaaaaaaa', 'v0.1.1-rc1',
                    'v0.1.1/extra', 'v0.1.1?x=1', 'v0.1.1\nextra'):
            with self.subTest(tag=tag):
                self.run_bootstrap('--release', tag, success=False)
                self.assertFalse((self.root / 'urls').exists())
                self.assertFalse((self.root / 'sudo-called').exists())
                self.assertFalse((self.root / 'binary-args').exists())

    def test_bad_checksum_manifest_or_binary_never_executes(self):
        cases = ['0' * 64 + '  ' + ASSET + '\n', self.manifest * 2,
                 self.manifest + 'f' * 64 + '  ../untrusted\n',
                 self.manifest.replace('  ', ' '), 'f' * 64 + '  another-file\n',
                 self.manifest + '\n']
        for manifest in cases:
            with self.subTest(manifest=manifest):
                (self.root / 'SHA256SUMS').write_text(manifest)
                self.run_bootstrap(success=False)
                self.assertFalse((self.root / 'sudo-called').exists())
                self.assertFalse((self.root / 'binary-args').exists())

    def test_root_and_unsupported_architecture_refuse_before_downloading(self):
        self.tool('id', 'print("0")\n')
        self.run_bootstrap(success=False)
        self.assertFalse((self.root / 'urls').exists())
        self.tool('id', 'print("1000")\n')
        self.tool('uname', 'print("Linux" if sys.argv[1] == "-s" else "aarch64")\n')
        self.run_bootstrap(success=False)
        self.assertFalse((self.root / 'urls').exists())

    def test_invalid_or_conflicting_arguments_refuse_before_download(self):
        for args in (('--all',), ('--release',), ('--release', TAG, '--binary', 'fixture'),
                     ('--destdir', '/'), ('--destdir', 'relative'), ('--release', TAG, '--release', TAG)):
            with self.subTest(args=args): self.run_bootstrap(*args, success=False)
        self.assertFalse((self.root / 'urls').exists())


if __name__ == '__main__':
    unittest.main()
