"""Real terminal fixture for grouped auth setup: two forges, hidden tokens, no provider or Git network calls."""
import json
import os
from pathlib import Path
import sys

binary, directory = sys.argv[1:3]
grouped = len(sys.argv) == 3
root = Path(directory).resolve()
(root / '.knit/projects').mkdir(parents=True)
home = root / 'home'
home.mkdir()
(root / '.knit/config.json').write_text(json.dumps({'schemaVersion': '1', 'activeProject': 'tools'}))
project = {
    'schemaVersion': '1', 'kind': 'KnitProject', 'id': 'tools', 'createdAt': '', 'updatedAt': '',
    'repos': [
        {'id': name, 'path': name,
         'remote': f'https://{host}/org/{name}.git', 'baseBranch': 'main'}
        for name, host in [('api', 'github.com'), ('web', 'github.com'),
                           ('bb', 'bitbucket.org')]
    ],
    'auth': {'groups': [
        {
            'id': 'gh-work', 'name': 'GitHub work token', 'provider': 'github',
            'host': 'github.com', 'repos': ['api', 'web'],
            'tokenTypes': ['fine_grained_pat'],
            'permissions': ['contents:read', 'pull_requests:write'],
            'instructions': 'Create the token in the org, scoped to both repos.',
            'tokenUrl': 'https://github.com/settings/personal-access-tokens/new',
        },
        {
            'id': 'bb-cloud', 'name': 'Bitbucket Cloud', 'provider': 'bitbucket',
            'host': 'bitbucket.org', 'repos': ['bb'],
            'tokenTypes': ['atlassian_api_token', 'access_token'],
            'tokenUrl': 'https://id.atlassian.com/manage-profile/security/api-tokens',
        },
        {
            'id': 'absent', 'name': 'Not cloned here', 'provider': 'github',
            'host': 'github.com', 'repos': ['extra'], 'tokenTypes': ['classic_pat'],
        },
    ]},
}
project_path = root / '.knit/projects/tools.project.json'
if not grouped:
    project.pop("auth")
project_path.write_text(json.dumps(project))
original = project_path.read_bytes()
# Membership sidecar: `extra` is a real project repo this workspace has not
# cloned, so the group that references it validates and is skipped, not
# treated as a typo.
(root / '.knit/projects/tools.known-repos.json').write_text(json.dumps(
    {'repos': {'extra': 'https://github.com/org/extra.git'}}))
env = {'PATH': '/usr/bin:/bin', 'HOME': str(home), 'KNIT_HOME': str(home),
       'GIT_CONFIG_GLOBAL': str(root / 'empty.gitconfig'), 'GIT_CONFIG_NOSYSTEM': '1',
       'TERM': 'dumb'}
from pty_session import conversation

choice = 'Use the default token (Enter), `t` for a project-only token, or `s` to skip: '

def script(expect, answer):
    if grouped:
        expect('Group gh-work: GitHub work token (github @ github.com)')
        expect('Permissions: contents:read, pull_requests:write')
        expect('Create a token at: https://github.com/settings/personal-access-tokens/new')
        expect('Instructions: Create the token in the org, scoped to both repos.')
    answer(choice, 'invalid')
    expect('Choose Enter, t, or s.')
    answer(choice, '')
    expect('No default token for github.com')
    answer(choice, 't')
    answer('Token for github.com (', 'PTY-GH-SECRET', hidden=True)
    if grouped:
        answer(choice, 't')
        answer('Token type (name or number): ', 'atlassian_api_token')
        answer('Token for bitbucket.org (', 'PTY-BB-SECRET', hidden=True)
        answer('Atlassian account email for this API token: ', 'dev@example.org')
        expect("Skipping group `absent`")
    expect('Done.')

args = ['auth', 'setup'] + ([] if grouped else ['--repo', 'api', '--repo', 'web'])
transcript = conversation(binary, root, env, args, script)
registry_path = home / 'forge-auth.json'
registry = json.loads(registry_path.read_text())
bindings = registry['projects'][str(project_path.resolve())]
assert bindings['api'] == bindings['web']
assert set(bindings) == ({'api', 'web', 'bb'} if grouped else {'api', 'web'})
assert not registry.get('defaults'), 'project setup must not change defaults'
assert set(bindings.values()) <= set(registry['scopedCredentials'])
secrets = json.loads((home / 'forge-secrets.json').read_text())
assert secrets[bindings['api']] == 'PTY-GH-SECRET'
if grouped:
    bb = registry['credentials'][bindings['bb']]
    assert bb['tokenType'] == 'atlassian_api_token'
    assert bb['username'] == 'dev@example.org'
    assert secrets[bindings['bb']] == 'PTY-BB-SECRET'
for secret in secrets.values():
    assert secret.encode() not in transcript, 'token echoed to terminal'
    assert secret.encode() not in registry_path.read_bytes()
assert project_path.read_bytes() == original, 'setup changed shared project metadata'
assert (home / 'forge-secrets.json').stat().st_mode & 0o777 == 0o600
assert home.stat().st_mode & 0o777 == 0o700
print('PTY project setup: scope, groups, hidden tokens, storage permissions: PASS')
