#!/usr/bin/env python3
"""Fail-closed GitHub fork API: all identities and storage are synthetic."""
import json
import os
import pathlib
import sys
import time
import urllib.parse

root = pathlib.Path(os.environ['GH_FAKE_DIR'])
args = sys.argv[1:]
assert args[0] == 'api', args
endpoint = next(a for a in args[1:] if a.startswith(('repos/', 'users/', 'orgs/', 'user/')) or a == 'user')
method = args[args.index('--method') + 1] if '--method' in args else 'GET'
payload = json.loads(sys.stdin.read()) if '--input' in args else {}
with (root / 'calls').open('a') as stream:
    stream.write(json.dumps({'method': method, 'endpoint': endpoint, 'payload': payload}) + '\n')
settings = json.loads((root / 'settings.json').read_text())
if settings.get('missing_credentials'):
    print('To get started with GitHub CLI, please run:  gh auth login', file=sys.stderr)
    sys.exit(1)
path = urllib.parse.urlsplit(endpoint).path.rstrip('/')
repo_settings = settings.pop('repos', {})
upstream_name = 'library'
for candidate, overrides in repo_settings.items():
    repo_path = '/'.join(path.split('/')[:3])
    if repo_path in (
        'repos/upstream-org/' + candidate,
        'repos/' + overrides.get('owner', settings.get('owner', 'contributor')) + '/' + overrides.get('name', candidate),
    ):
        upstream_name = candidate
        settings.update(overrides)
        break
created = root / ('created' if upstream_name == 'library' else 'created-' + upstream_name)
polled = root / ('polled' if upstream_name == 'library' else 'polled-' + upstream_name)
upstream_ref = 'upstream-org/' + upstream_name
if settings.get('expected_token'):
    assert os.environ.get('GH_TOKEN') == settings['expected_token'], 'wrong repository credential'
owner = settings.get('owner', 'contributor')
name = settings.get('name', upstream_name)
upstream = {'id': 101, 'full_name': upstream_ref, 'name': upstream_name,
            'owner': {'login': 'upstream-org'}, 'fork': False, 'private': False,
            'clone_url': 'https://github.com/' + upstream_ref + '.git',
            'ssh_url': 'git@github.com:' + upstream_ref + '.git', 'default_branch': 'main'}
if 'push' in settings:
    upstream['permissions'] = {'push': settings['push']}
fork = {'id': 202, 'full_name': owner + '/' + name, 'name': name, 'owner': {'login': owner},
        'fork': True, 'parent': dict(upstream), 'source': dict(upstream),
        'clone_url': 'https://github.com/' + owner + '/' + name + '.git',
        'ssh_url': 'git@github.com:' + owner + '/' + name + '.git', 'default_branch': 'main'}
if settings.get('indirect') or (settings.get('post_indirect') and created.exists()):
    fork['parent'] = {'id': 303, 'full_name': 'other/library'}
present = settings.get('existing', False) or created.exists()
if path == 'user':
    value = {'login': 'contributor', 'id': 404}
elif path == 'repos/' + upstream_ref:
    value = upstream
elif path in ('repos/' + upstream_ref + '/git/ref/heads/main', 'repos/' + owner + '/' + name + '/git/ref/heads/contribution'):
    value = {'object': {'sha': settings['head']}}
elif path == 'repos/' + upstream_ref + '/pulls' and method == 'POST':
    print('synthetic preflight complete (HTTP 422)', file=sys.stderr)
    sys.exit(1)
elif path == 'repos/' + upstream_ref + '/forks' and method == 'POST':
    if os.environ.get('FORK_CREATE_NOTICE_LOG'):
        notice = pathlib.Path(os.environ['FORK_CREATE_NOTICE_LOG']).read_text()
        assert 'public' in notice.lower(), 'notice must be visible before first POST'
        assert len(notice.strip().splitlines()) == 1, notice
    created.write_text('yes')
    value = dict(fork)
    if settings.get('post_indirect'):
        value['parent'] = {'id': 303, 'full_name': 'other/library'}
elif path == 'repos/' + upstream_ref + '/forks' or path in ('users/' + owner + '/repos', 'orgs/' + owner + '/repos', 'user/repos'):
    value = [fork] if present else []
elif path in ('repos/' + owner + '/' + upstream_name, 'repos/' + owner + '/' + name):
    if settings.get('hang_ready') and created.exists():
        (root / 'sleep-started').write_text(str(os.getpid()))
        time.sleep(30)
        (root / 'sleep-completed').write_text('unexpected')
    # A renamed fork must be discovered from a list, not guessed by upstream name.
    if not present or path != 'repos/' + owner + '/' + name or settings.get('never_ready'):
        print('gh: Not Found (HTTP 404)', file=sys.stderr)
        sys.exit(1)
    if settings.get('delay_once') and created.exists() and not polled.exists():
        polled.write_text('yes')
        print('gh: Not Found (HTTP 404)', file=sys.stderr)
        sys.exit(1)
    value = dict(fork)
    if settings.get('get_mismatch'):
        value['full_name'] = owner + '/unexpected-library'
        value['clone_url'] = 'https://github.com/' + value['full_name'] + '.git'
        value['ssh_url'] = 'git@github.com:' + value['full_name'] + '.git'
else:
    raise AssertionError((method, endpoint, args))
print(json.dumps(value))
