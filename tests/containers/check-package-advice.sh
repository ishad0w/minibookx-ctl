#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
# Resolve Doctor's recommendations in disposable containers; never run on a host.
set -eu
test -f /.dockerenv || { echo 'Run only in the disposable Docker fixture.' >&2; exit 1; }
case "$1" in
    debian)
        # Firmware/full media packages use these official repository components.
        # This changes only the disposable fixture, never the product installer.
        python3 - <<'PY'
from pathlib import Path
path = Path('/etc/apt/sources.list.d/debian.sources')
lines = []
for line in path.read_text().splitlines():
    if line.startswith('Components:'):
        components = line.split()[1:]
        for component in ('non-free', 'non-free-firmware'):
            if component not in components:
                components.append(component)
        line = 'Components: ' + ' '.join(components)
    lines.append(line)
path.write_text('\n'.join(lines) + '\n')
PY
        apt-get update
        ;;
    ubuntu|ubuntu24|fedora|fedora43|arch|cachyos) ;;
    *) exit 2 ;;
esac
/tmp/minibookx-ctl --debug --json > /tmp/package-advice.json
python3 - "$1" <<'PY'
import json
import os
from pathlib import Path
import re
import subprocess
import sys

distro = sys.argv[1]
report = json.loads(Path('/tmp/package-advice.json').read_text())
graphics = next(section for section in report['sections'] if section['key'] == 'graphics')
rows = {row['key']: row for row in graphics['fields']}
environment = dict(os.environ, LC_ALL='C')

def run(args):
    return subprocess.run(args, env=environment, text=True, stdout=subprocess.PIPE,
                          stderr=subprocess.STDOUT, timeout=120)

def available(name):
    if distro in ('debian', 'ubuntu', 'ubuntu24'):
        result = run(['apt-cache', 'show', '--no-all-versions', name])
        return result.returncode == 0 and ('Package: ' + name + '\n') in result.stdout
    if distro in ('fedora', 'fedora43'):
        result = run(['dnf', '--cacheonly', 'repoquery', '--available', '--queryformat', '%{name}', name])
        return result.returncode == 0 and name in result.stdout.splitlines()
    result = run(['pacman', '-Si', name])
    return result.returncode == 0

selected = []
for key in ('mesa_gl', 'mesa_vulkan', 'intel_media', 'intel_firmware', 'vainfo'):
    candidates = rows[key]['candidates']
    assert isinstance(candidates, list) and candidates
    assert all(isinstance(name, str) and re.fullmatch(r'[a-z0-9][a-z0-9+.-]{0,80}', name)
               for name in candidates), candidates
    found = [name for name in candidates if available(name)]
    assert found, (distro, key, candidates)
    selected.append(found[0])
    print(f'ADVICE_PACKAGE PASS {distro} {key}: {found[0]}', flush=True)

# Check both documented APT media alternatives without installing them together.
if distro in ('debian', 'ubuntu', 'ubuntu24'):
    assert available('intel-media-va-driver-non-free'), distro

if distro in ('debian', 'ubuntu', 'ubuntu24'):
    result = run(['apt-get', '--simulate', '--no-install-recommends', 'install', *selected])
    assert result.returncode == 0, result.stdout
elif distro in ('fedora', 'fedora43'):
    result = run(['dnf', '--cacheonly', '--assumeno', '--setopt=install_weak_deps=False',
                  'install', *selected])
    assert result.returncode == 0 or (
        result.returncode == 1 and any(message in result.stdout for message in (
            'Operation aborted.', 'Operation aborted by the user.'))
        and 'Transaction Summary' in result.stdout
    ), result.stdout
else:
    result = run(['pacman', '-Sp', '--noconfirm', '--print-format', '%n %v', *selected])
    assert result.returncode == 0, result.stdout
print(f'PASS: {distro} recommended native package set resolves without installation.', flush=True)
print('Not checked: RPM Fusion transactions, codec coverage or hardware acceleration.', flush=True)
PY
