"""Real terminal fixture: `m` repairs saved Bitbucket token metadata in project setup.

Recovery shape: a workspace whose Bitbucket checkout already exists (ordinary
Git clone) next to a saved host default with unclassified metadata. The
existing checkout means no clone retry can fail, so the guided failed-clone
menu is unreachable; `knit auth setup --repo <id>` with `m` is the way in.
Proves: only the selected repository is repaired, a deliberate sibling
override is repaired as itself (never swapped for the host default), the
shared default keeps its token and spec, and no replacement token is read.
"""
import json
import subprocess
from pathlib import Path
import sys

binary, directory = sys.argv[1:3]
root = Path(directory).resolve()
(root / '.knit/projects').mkdir(parents=True)
home = root / 'home'
home.mkdir()
(root / '.knit/config.json').write_text(json.dumps(
    {'schemaVersion': '1', 'activeProject': 'tools'}))
project = {
    'schemaVersion': '1', 'kind': 'KnitProject', 'id': 'tools',
    'createdAt': '', 'updatedAt': '',
    'repos': [
        {'id': name, 'path': name,
         'remote': f'https://bitbucket.org/org/{name}.git', 'baseBranch': 'main'}
        for name in ('bb', 'bb2')
    ],
    'auth': {'groups': [
        {
            'id': 'bb-cloud', 'name': 'Bitbucket Cloud', 'provider': 'bitbucket',
            'host': 'bitbucket.org', 'repos': ['bb', 'bb2'],
            'tokenTypes': ['atlassian_api_token', 'access_token'],
            'tokenUrl': 'https://id.atlassian.com/manage-profile/security/api-tokens',
        },
    ]},
}
project_path = root / '.knit/projects/tools.project.json'
project_path.write_text(json.dumps(project))
original = project_path.read_bytes()
env = {'PATH': '/usr/bin:/bin', 'HOME': str(home), 'KNIT_HOME': str(home),
       'GIT_CONFIG_GLOBAL': str(root / 'empty.gitconfig'), 'GIT_CONFIG_NOSYSTEM': '1',
       'TERM': 'dumb'}
(root / 'empty.gitconfig').write_text('')
# Existing checkouts on disk: the ordinary-Git recovery left real repositories
# at their expected standard-layout paths.
for name in ('bb', 'bb2'):
    checkout = root / name
    checkout.mkdir()
    subprocess.run(['git', 'init', '-q', '-b', 'main', str(checkout)],
                   env=env, check=True)
    subprocess.run(['git', '-C', str(checkout), 'remote', 'add', 'origin',
                    f'https://bitbucket.org/org/{name}.git'], env=env, check=True)
    subprocess.run(['git', '-C', str(checkout), '-c', 'user.email=dev@example.org',
                    '-c', 'user.name=Dev', 'commit', '-q', '--allow-empty',
                    '-m', 'init'], env=env, check=True)

def seed(args, token):
    subprocess.run([binary] + args, input=(token + '\n').encode(), env=env,
                   cwd=root, check=True, capture_output=True)

# The host default (first saved token on the host) with unclassified
# metadata, plus a deliberate sibling override for `bb2`.
seed(['auth', 'add', 'shared', '--provider', 'bitbucket', '--token-stdin'],
     'PTY-SHARED-SECRET')
seed(['auth', 'add', 'bb2-alt', '--provider', 'bitbucket', '--token-stdin'],
     'PTY-ALT-SECRET')
subprocess.run([binary, 'auth', 'use', 'bb2-alt', '--repo', 'bb2'], env=env,
               cwd=root, check=True, capture_output=True)

from pty_session import conversation

menu = ("`m` repairs the saved Bitbucket token's kind/email for the selected "
        "repositories. Use the default token (Enter), `t` for a project-only "
        "token, or `s` to skip: ")
reuse = ('Reuse the same token with a corrected token kind/account email '
         'for these repositories? [y/N]: ')
kind_prompt = 'Which kind of Bitbucket token is it? (name/number): '
email_prompt = 'Atlassian account email for this API token: '

def never_read_a_token(transcript):
    assert b'Replacement token' not in transcript, 'm path asked for a token'
    assert b'Token for bitbucket.org' not in transcript, 'm path asked for a token'

# Phase 1: repair only the selected repository (`bb`, served by the host
# default) on an existing checkout.
def phase1(expect, answer):
    expect('Group bb-cloud:')
    expect('bitbucket.org (bb): default token `shared`')
    answer(menu, 'm')
    expect('Repairing the saved credential `shared` for bb.')
    answer(reuse, 'y')
    answer(kind_prompt, 'atlassian_api_token')
    answer(email_prompt, 'dev@example.org')
    expect('`shared` keeps its token, default, and other assignments; '
           '`bitbucket.org-bb-cloud` serves bb with the corrected metadata.')
    expect('Done.')

transcript1 = conversation(binary, root, env, ['auth', 'setup', '--repo', 'bb'], phase1)
never_read_a_token(transcript1)

registry = json.loads((home / 'forge-auth.json').read_text())
secrets = json.loads((home / 'forge-secrets.json').read_text())
assert registry['defaults'] == {'bitbucket.org': 'shared'}
assert registry['credentials']['shared'].get('tokenType') is None
assert registry['credentials']['shared'].get('username') is None
assert secrets['shared'] == 'PTY-SHARED-SECRET'
repaired = registry['credentials']['bitbucket.org-bb-cloud']
assert repaired['tokenType'] == 'atlassian_api_token'
assert repaired['username'] == 'dev@example.org'
assert 'bitbucket.org-bb-cloud' in registry['scopedCredentials']
# Same secret, copied: the shared original keeps its token untouched.
assert secrets['bitbucket.org-bb-cloud'] == 'PTY-SHARED-SECRET'
# Only the selected repository was rebound; the sibling override stands.
bindings = registry['projects'][str(project_path.resolve())]
assert bindings == {'bb': 'bitbucket.org-bb-cloud', 'bb2': 'bb2-alt'}
assert registry['credentials']['bb2-alt'].get('tokenType') is None
assert secrets['bb2-alt'] == 'PTY-ALT-SECRET'
assert (root / 'bb' / '.git' / 'knit-credentials.inc').exists(), \
    'existing checkout did not get the plain-Git helper'
for secret in ('PTY-SHARED-SECRET', 'PTY-ALT-SECRET'):
    assert secret.encode() not in transcript1, 'token echoed to terminal'
assert project_path.read_bytes() == original, 'setup changed shared project metadata'

# Phase 2: both repositories in one menu — two saved credentials serve them,
# so the repair partitions by the credential each repository resolves to
# (the explicit override, never the host default) and repairs each.
def phase2(expect, answer):
    expect('bitbucket.org (bb, bb2): default token `shared`')
    expect('  bb: project token `bitbucket.org-bb-cloud`')
    expect('  bb2: project token `bb2-alt`')
    answer(menu, 'm')
    expect('Repairing the saved credential `bb2-alt` for bb2.')
    answer(reuse, 'y')
    answer(kind_prompt, 'access_token')
    expect('`bb2-alt` keeps its token, default, and other assignments; '
           '`bitbucket.org-bb-cloud-2` serves bb2 with the corrected metadata.')
    expect('Repairing the saved credential `bitbucket.org-bb-cloud` for bb.')
    answer(reuse, 'y')
    answer(kind_prompt, 'access_token')
    expect('`bitbucket.org-bb-cloud` keeps its token, default, and other assignments; '
           '`bitbucket.org-bb-cloud-3` serves bb with the corrected metadata.')
    expect('Done.')

transcript2 = conversation(binary, root, env,
                           ['auth', 'setup', '--repo', 'bb', '--repo', 'bb2'], phase2)
never_read_a_token(transcript2)

registry = json.loads((home / 'forge-auth.json').read_text())
secrets = json.loads((home / 'forge-secrets.json').read_text())
assert registry['defaults'] == {'bitbucket.org': 'shared'}
assert secrets['shared'] == 'PTY-SHARED-SECRET'
assert secrets['bb2-alt'] == 'PTY-ALT-SECRET'
# Each partition copied its own credential's secret.
assert secrets['bitbucket.org-bb-cloud-2'] == 'PTY-ALT-SECRET'
assert secrets['bitbucket.org-bb-cloud-3'] == 'PTY-SHARED-SECRET'
bindings = registry['projects'][str(project_path.resolve())]
assert bindings == {'bb': 'bitbucket.org-bb-cloud-3', 'bb2': 'bitbucket.org-bb-cloud-2'}
for secret in ('PTY-SHARED-SECRET', 'PTY-ALT-SECRET'):
    assert secret.encode() not in transcript2, 'token echoed to terminal'
print('PTY Bitbucket metadata repair: existing checkout, selection, partitioning, '
      'shared default preserved, no token read: PASS')
