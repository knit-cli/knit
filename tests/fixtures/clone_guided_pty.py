"""Real-terminal regressions for the guided clone credential flows.

Conversation 1 (grouped clone): a project whose export declares two groups on
one host plus a group on a second host. The clone prompts for one hidden
token per group — no credential naming, no token-source question, no mapping
confirmation — keeps the second same-host group distinct from the first, and
never reads the hosted forge-credential endpoint.

Conversation 2 (inferred fallback, the interrupted-clone repro shape): no
declared groups; a public repository clones with ambient Git, the private one
fails on the disabled raw prompt, and the clone itself asks for the missing
token once per host and recovers. The public repository's exact remote is
recorded as ambient access so the workspace's strict gate keeps letting it
through later.

Conversation 3 (pull recovery through the strict gate): a new declared group
arrives for a repository the workspace never cloned. `knit pull --bundles`
hits the missing-assignment strict-gate failure mid-reconcile, prompts for
the group's token, and clones the repository in place.
"""
import json
import os
from pathlib import Path
import socketserver
import shlex
import subprocess
import sys
import threading

binary, directory = sys.argv[1:]
root = Path(directory).resolve()
requests_log = root / 'remote-requests.txt'


# ---------------------------------------------------------------------------
# Fake sync remote: re-reads export.json per request so conversations can
# rewrite it; 403s the forge-credential export like the hosted server does
# for ordinary tokens.
# ---------------------------------------------------------------------------
class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        data = b''
        while b'\r\n\r\n' not in data:
            chunk = self.request.recv(65536)
            if not chunk:
                return
            data += chunk
        head, body = data.split(b'\r\n\r\n', 1)
        lines = head.decode(errors='replace').split('\r\n')
        method, target = lines[0].split(' ')[0:2]
        path = target.split('?')[0]
        length = next((int(l.split(':', 1)[1]) for l in lines
                       if l.lower().startswith('content-length:')), 0)
        while len(body) < length:
            body += self.request.recv(65536)
        with open(requests_log, 'a') as log:
            log.write(f'{method} {path}\n')
        if path == '/api/v1/me/forge-credentials':
            status, response = 403, '{"error":{"detail":"forbidden"}}'
        elif method == 'GET' and path.startswith('/api/v1/projects/') and path.endswith('/export'):
            status, response = 200, (Path(self.server.dir) / 'export.json').read_text()
        elif path.startswith('/api/v1/projects/') and path.endswith('/view'):
            if method == 'PUT':
                (Path(self.server.dir) / 'views-puts.jsonl').open('a').write(body.decode())
                status, response = 200, '{"data":{}}'
            else:
                status, response = 200, '{"data":{"views":{}}}'
        else:
            status, response = 404, '{"error":{"detail":"unexpected"}}'
        payload = response.encode()
        self.request.sendall(
            f'HTTP/1.1 {status} Fake\r\ncontent-type: application/json\r\n'
            f'content-length: {len(payload)}\r\nconnection: close\r\n\r\n'.encode() + payload)


class Server(socketserver.ThreadingTCPServer):
    daemon_threads = True
    allow_reuse_address = True


server = Server(('127.0.0.1', 0), Handler)
BASE_URL = f'http://127.0.0.1:{server.server_address[1]}'
threading.Thread(target=server.serve_forever, daemon=True).start()


def forge_credential_hits():
    if not requests_log.exists():
        return 0
    return sum(1 for line in requests_log.read_text().splitlines()
               if '/me/forge-credentials' in line)


# ---------------------------------------------------------------------------
# Fake git: clone/ls-remote only. `auth` URLs require the Knit credential
# helper (exactly like a private forge without ambient access) and fail with
# the disabled-prompt message otherwise; `public` URLs clone ambiently from a
# local source. Everything else passes through to the real git.
# ---------------------------------------------------------------------------
def write_fake_git(mapping):
    real_git = subprocess.run(['/bin/sh', '-c', 'command -v git'],
                              capture_output=True, text=True).stdout.strip()
    fake_bin = root / 'bin'
    fake_bin.mkdir(parents=True, exist_ok=True)
    cases = ''.join(
        f'  {shlex.quote(url)}) mode={mode}; src={shlex.quote(str(src))} ;;\n' for url, src, mode in mapping)
    script = (Path(__file__).with_name('forge_git.sh').read_text()
              .replace('__REAL_GIT__', shlex.quote(real_git))
              .replace('__ROOT__', shlex.quote(str(root)))
              .replace('__CASES__', cases))
    git = fake_bin / 'git'
    git.write_text(script)
    git.chmod(0o755)


_seed_counter = [0]


def seed_source(name):
    # Each seed gets unique content so re-seeding a name (a later
    # conversation reusing a repository) always has something to commit.
    _seed_counter[0] += 1
    src = root / 'sources' / name
    src.mkdir(parents=True, exist_ok=True)
    env = {'PATH': os.environ['PATH'], 'GIT_CONFIG_GLOBAL': str(root / 'empty.gitconfig'),
           'GIT_CONFIG_NOSYSTEM': '1', 'HOME': str(root / 'fixture-home'),
           # Deterministic identity: the empty isolated config and scrubbed
           # HOME leave nothing ambient for git to fall back on (CI runners
           # have no GECOS name, so commits would fail with "Author
           # identity unknown").
           'GIT_AUTHOR_NAME': 'Test', 'GIT_AUTHOR_EMAIL': 'test@example.test',
           'GIT_COMMITTER_NAME': 'Test', 'GIT_COMMITTER_EMAIL': 'test@example.test'}
    subprocess.run(['git', 'init', '-q', str(src)], env=env, check=True)
    (src / 'README').write_text(f'{name}-{_seed_counter[0]}')
    subprocess.run(['git', 'add', '.'], cwd=src, env=env, check=True)
    subprocess.run(['git', 'commit', '-qm', 'seed'], cwd=src, env=env,
                   check=True, stdout=subprocess.DEVNULL)
    return src


def export_body(repos, knit_project=None):
    return json.dumps({'data': {
        'project': {'slug': 'demo'},
        'knitProject': knit_project,
        'repositories': [
            {'localId': rid, 'name': rid, 'defaultBranch': 'main',
             'remoteUrl': url, 'visibility': visibility, 'metadata': {}}
            for rid, url, visibility in repos],
        'omittedRepositoryCount': 0,
        'bundles': [],
        'historyEvents': [],
    }})


def membership(repos, groups):
    return {'schemaVersion': '1', 'kind': 'KnitProject', 'id': 'demo',
            'createdAt': '', 'updatedAt': '',
            'repos': [{'id': rid, 'path': rid, 'remote': url, 'baseBranch': 'main'}
                      for rid, url, _ in repos],
            'auth': {'groups': groups}}


# ---------------------------------------------------------------------------
# PTY conversation runner (mirrors auth_groups_pty.py).
# ---------------------------------------------------------------------------


def conversation(cwd, home, args, script):
    from pty_session import conversation as run
    home.mkdir(parents=True, exist_ok=True)
    env = {'PATH': f'{root}/bin:/usr/bin:/bin', 'HOME': str(home),
           'KNIT_HOME': str(home), 'GIT_CONFIG_GLOBAL': str(root / 'empty.gitconfig'),
           'GIT_CONFIG_NOSYSTEM': '1', 'TERM': 'dumb'}
    return run(binary, cwd, env, args, script)


def registry(home):
    return json.loads((home / 'forge-auth.json').read_text())


def secrets(home):
    return json.loads((home / 'forge-secrets.json').read_text())


try:
    root.mkdir(parents=True, exist_ok=True)
    (root / 'empty.gitconfig').write_text('')
    remote_dir = root / 'remote'
    remote_dir.mkdir(parents=True)
    server.dir = str(remote_dir)

    # -----------------------------------------------------------------------
    # Conversation 1: grouped clone, same-host groups stay distinct.
    # -----------------------------------------------------------------------
    conv1 = root / 'conv1'
    conv1.mkdir()
    sources = {name: seed_source(name) for name in ['api', 'web', 'mobile', 'infra']}
    write_fake_git([
        (f'https://github.com/org/{name}.git', str(sources[name]), 'auth')
        for name in ['api', 'web', 'mobile']
    ] + [('https://gitlab.com/acme/infra.git', str(sources['infra']), 'auth')])
    groups = [
        {'id': 'gh-work', 'name': 'Work repos', 'provider': 'github',
         'host': 'github.com', 'repos': ['api', 'web'], 'tokenTypes': ['classic_pat']},
        {'id': 'gh-mobile', 'name': 'Mobile team', 'provider': 'github',
         'host': 'github.com', 'repos': ['mobile'], 'tokenTypes': ['fine_grained_pat']},
        {'id': 'gl-infra', 'name': 'Infra', 'provider': 'gitlab',
         'host': 'gitlab.com', 'repos': ['infra'], 'tokenTypes': ['personal_access_token']},
    ]
    repos = [(name, f'https://github.com/org/{name}.git'
              if name != 'infra' else 'https://gitlab.com/acme/infra.git', 'private')
             for name in ['api', 'web', 'mobile', 'infra']]
    (remote_dir / 'export.json').write_text(
        export_body(repos, membership(repos, groups)))
    before = forge_credential_hits()

    def grouped_script(expect, answer):
        expect('Setting up project credentials before cloning:')
        # First group on the host: one direct hidden token prompt; the first
        # token entered becomes the host's default.
        expect('Group gh-work: Work repos (github @ github.com)')
        expect('Token type(s): classic_pat')
        answer('Token for github.com (gh-work — hidden): ', 'CONV1-GH-WORK', hidden=True)
        expect('`github.com-gh-work` is now the default credential for github.com.')
        # The second group on the SAME host reuses that default with no
        # prompt and no per-repository links.
        expect('Host default `github.com-gh-work` covers github.com — using it for group `gh-mobile` without repository links.')
        # Third group on its own host: direct prompt again.
        expect('Group gl-infra: Infra (gitlab @ gitlab.com)')
        answer('Token for gitlab.com (gl-infra — hidden): ', 'CONV1-GL', hidden=True)
        expect('`gitlab.com-gl-infra` is now the default credential for gitlab.com.')
        expect('Imported: 4 repo(s)')

    transcript1 = conversation(conv1, conv1 / 'home', [
        'clone', 'demo', '--remote', 'hosted', '--url', BASE_URL,
        '--token', 'test-token', '--no-worktree'], grouped_script)
    joined = transcript1.decode(errors='replace')
    for forbidden in ['Name for this credential', 'Token environment variable',
                      'Apply this mapping?', 'Token (hidden): ']:
        assert forbidden not in joined, f'choreography leaked into guided clone: {forbidden}'
    assert forge_credential_hits() == before, \
        'grouped clone read the hosted forge-credential endpoint'
    project_key = str((conv1 / 'demo' / '.knit/projects/demo.project.json').resolve())
    reg = registry(conv1 / 'home')
    # Defaults serve both same-host groups; nothing was bound per-repository.
    assert reg['defaults'] == {'github.com': 'github.com-gh-work',
                               'gitlab.com': 'gitlab.com-gl-infra'}
    assert reg['projects'].get(project_key) is None
    assert secrets(conv1 / 'home') == {'github.com-gh-work': 'CONV1-GH-WORK',
                                       'gitlab.com-gl-infra': 'CONV1-GL'}
    assert (conv1 / 'demo' / 'api' / '.git').exists()
    assert (conv1 / 'demo' / 'mobile' / '.git').exists()
    print('guided grouped clone: first token defaults, same-host groups reuse it: PASS', flush=True)

    # -----------------------------------------------------------------------
    # Conversation 2: no declared groups; public clones ambiently, the private
    # repo's disabled prompt feeds the in-clone inferred fallback.
    # -----------------------------------------------------------------------
    conv2 = root / 'conv2'
    conv2.mkdir()
    sources2 = {name: seed_source(name) for name in ['pub', 'sec']}
    write_fake_git([
        ('https://github.com/org/pub.git', str(sources2['pub']), 'public'),
        ('https://github.com/org/sec.git', str(sources2['sec']), 'auth'),
    ])
    repos2 = [('pub', 'https://github.com/org/pub.git', 'public'),
              ('sec', 'https://github.com/org/sec.git', 'private')]
    (remote_dir / 'export.json').write_text(export_body(repos2))

    def inferred_script(expect, answer):
        expect('Private repositories: 1 repo(s) need a forge token')
        expect('Group github.com: github.com (github @ github.com)')
        answer('Token for github.com (github.com — hidden): ', 'CONV2-SEC', hidden=True)
        expect('`github.com` is now the default credential for github.com.')
        expect('Recovered after setup: sec')
        expect('Imported: 2 repo(s)')

    conversation(conv2, conv2 / 'home', [
        'clone', 'demo', '--remote', 'hosted', '--url', BASE_URL,
        '--token', 'test-token', '--no-worktree'], inferred_script)
    assert (conv2 / 'demo' / 'pub' / '.git').exists()
    assert (conv2 / 'demo' / 'sec' / '.git').exists()
    project_key2 = str((conv2 / 'demo' / '.knit/projects/demo.project.json').resolve())
    reg2 = registry(conv2 / 'home')
    # The inferred token became the github default and serves both repos —
    # no per-repository binding, and the public repo needs no ambient
    # allowance because the default covers it.
    assert reg2['defaults'] == {'github.com': 'github.com'}
    assert reg2['projects'].get(project_key2) is None
    assert reg2.get('ambient', {}).get(project_key2) is None
    print('inferred fallback: one hidden prompt, token becomes the host default: PASS', flush=True)

    # -----------------------------------------------------------------------
    # Conversation 3: a new declared group arrives via `knit pull --bundles`;
    # the strict missing-assignment gate failure prompts instead of dead-ending.
    # -----------------------------------------------------------------------
    sources3 = {name: seed_source(name) for name in ['fresh']}
    write_fake_git([
        ('https://github.com/org/pub.git', str(sources2['pub']), 'public'),
        ('https://github.com/org/sec.git', str(sources2['sec']), 'auth'),
        ('https://gitlab.com/acme/fresh.git', str(sources3['fresh']), 'auth'),
    ])
    repos3 = [('pub', 'https://github.com/org/pub.git', 'public'),
              ('sec', 'https://github.com/org/sec.git', 'private'),
              ('fresh', 'https://gitlab.com/acme/fresh.git', 'public')]
    # The new group is on a host the personal store has nothing for, so the
    # pull-recovery path must prompt for its token (the unique-compatible
    # reuse path is covered by conversation 1's second group). `fresh` is
    # PUBLIC but the declared group covers it: a group is the project's
    # statement that a credential is required, so it must take the guided
    # path — never a silent ambient allowance.
    (remote_dir / 'export.json').write_text(export_body(
        repos3, membership(repos3, [
            {'id': 'fresh-grp', 'name': 'New team', 'provider': 'gitlab',
             'host': 'gitlab.com', 'repos': ['fresh'], 'tokenTypes': ['personal_access_token']},
        ])))

    def pull_script(expect, answer):
        expect('Private repositories: 1 repo(s) need a forge token')
        expect('Group fresh-grp: New team (gitlab @ gitlab.com)')
        answer('Token for gitlab.com (fresh-grp — hidden): ', 'CONV3-FRESH', hidden=True)
        expect('`gitlab.com-fresh-grp` is now the default credential for gitlab.com.')
        expect('Project repo: added fresh')

    workspace = conv2 / 'demo'
    conversation(workspace, conv2 / 'home', ['pull', '--bundles'], pull_script)
    assert (workspace / 'fresh' / '.git').exists()
    reg3 = registry(conv2 / 'home')
    assert reg3['defaults']['gitlab.com'] == 'gitlab.com-fresh-grp'
    assert reg3['projects'].get(project_key2) is None
    assert secrets(conv2 / 'home')['gitlab.com-fresh-grp'] == 'CONV3-FRESH'
    # A group-covered repository never gains an ambient allowance, even
    # though the export marks it public.
    assert 'fresh' not in reg3.get('ambient', {}).get(project_key2, {})
    print('pull recovery: strict-gate failure prompts for the new group: PASS', flush=True)

    # -----------------------------------------------------------------------
    # Conversation 4 (scenario D): a declared private group plus an ungrouped
    # PUBLIC repository. The grouped repo gets its token; the ungrouped repo
    # keeps ambient Git access instead of being locked out by the strict
    # gate, and `--prefer-https` never needs the hosted forge-credential
    # endpoint (the ordinary-token 403) to answer anything.
    # -----------------------------------------------------------------------
    conv4 = root / 'conv4'
    conv4.mkdir()
    sources4 = {name: seed_source(name) for name in ['d-gh', 'd-pub']}
    write_fake_git([
        ('https://github.com/org/d-gh.git', str(sources4['d-gh']), 'auth'),
        ('https://github.com/org/d-pub.git', str(sources4['d-pub']), 'public'),
    ])
    repos4 = [('d-gh', 'https://github.com/org/d-gh.git', 'private'),
              ('d-pub', 'https://github.com/org/d-pub.git', 'public')]
    (remote_dir / 'export.json').write_text(export_body(
        repos4, membership(repos4, [
            {'id': 'd-gh-grp', 'name': 'Private work', 'provider': 'github',
             'host': 'github.com', 'repos': ['d-gh'], 'tokenTypes': ['classic_pat']},
        ])))
    before4 = forge_credential_hits()

    def scenario_d_script(expect, answer):
        expect('Setting up project credentials before cloning:')
        expect('Group d-gh-grp: Private work (github @ github.com)')
        answer('Token for github.com (d-gh-grp — hidden): ', 'CONV4-GH', hidden=True)
        expect('`github.com-d-gh-grp` is now the default credential for github.com.')
        expect('Imported: 2 repo(s)')

    conversation(conv4, conv4 / 'home', [
        'clone', 'demo', '--remote', 'hosted', '--url', BASE_URL,
        '--token', 'test-token', '--prefer-https', '--no-worktree'], scenario_d_script)
    assert (conv4 / 'demo' / 'd-gh' / '.git').exists()
    assert (conv4 / 'demo' / 'd-pub' / '.git').exists()
    project_key4 = str((conv4 / 'demo' / '.knit/projects/demo.project.json').resolve())
    reg4 = registry(conv4 / 'home')
    # The group's token became the github default and serves the ungrouped
    # public repository too — intentionally: a personal default is used for
    # EVERYTHING on its forge. No bindings, no ambient allowance, and no
    # hosted forge-credential lookup even with --prefer-https.
    assert reg4['defaults'] == {'github.com': 'github.com-d-gh-grp'}
    assert reg4['projects'].get(project_key4) is None
    assert reg4.get('ambient', {}).get(project_key4) is None
    assert forge_credential_hits() == before4, \
        'scenario D clone read the hosted forge-credential endpoint'
    print('scenario D: group token defaults, covers ungrouped public, no helper query: PASS', flush=True)

    # -----------------------------------------------------------------------
    # Conversation 5: the declared group's saved credential is revoked. The
    # clone reuses it, is denied, and the same repair pull uses rotates the
    # rejected token in place — the forge accepts the replacement.
    # -----------------------------------------------------------------------
    conv5 = root / 'conv5'
    conv5.mkdir()
    sources5 = {name: seed_source(name) for name in ['solo']}
    write_fake_git([
        ('https://github.com/org/solo.git', str(sources5['solo']), 'auth'),
    ])
    (root / 'forge-rejects').write_text('1')
    (conv5 / 'home').mkdir(parents=True, exist_ok=True)
    add_env = {'PATH': f'{root}/bin:/usr/bin:/bin', 'HOME': str(conv5 / 'home'),
               'KNIT_HOME': str(conv5 / 'home'),
               'GIT_CONFIG_GLOBAL': str(root / 'empty.gitconfig'),
               'GIT_CONFIG_NOSYSTEM': '1'}
    # cwd stays inside the fixture: `auth add` activates plain-Git
    # integration for the invoking cwd's project, and an inherited test-runner
    # cwd would configure the real workspace with synthetic tokens.
    subprocess.run([binary, 'auth', 'add', 'gh-bad', '--provider', 'github',
                    '--token-stdin'], input=b'REJECTED-TOKEN\n', env=add_env,
                   cwd=str(conv5),
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    repos5 = [('solo', 'https://github.com/org/solo.git', 'private')]
    (remote_dir / 'export.json').write_text(export_body(
        repos5, membership(repos5, [
            {'id': 'gh', 'name': 'Work', 'provider': 'github',
             'host': 'github.com', 'repos': ['solo'], 'tokenTypes': ['classic_pat']},
        ])))

    def repair_script(expect, answer):
        expect('Setting up project credentials before cloning:')
        # gh-bad is the only github token, so it is the host's implicit
        # default and the group reuses it without asking or linking.
        expect('Host default `gh-bad` covers github.com — using it for group `gh` without repository links.')
        # The forge rejects it; the repair offers a replacement that never
        # touches the shared default's own token.
        expect('Private repositories: 1 repo(s) need a forge token')
        expect('Credential `gh-bad`')
        expect('was used and access was denied for solo.')
        # The forge starts accepting the replacement token: unblock it before
        # answering the prompt.
        (root / 'forge-rejects').unlink()
        answer('Replacement token for github.com (saved as a new local credential for solo; '
               '`gh-bad` keeps its token — hidden): ',
               'REPLACEMENT-TOKEN', hidden=True)
        expect('Recovered after setup: solo')
        expect('Imported: 1 repo(s)')

    conversation(conv5, conv5 / 'home', [
        'clone', 'demo', '--remote', 'hosted', '--url', BASE_URL,
        '--token', 'test-token', '--no-worktree'], repair_script)
    assert (conv5 / 'demo' / 'solo' / '.git').exists()
    reg5 = registry(conv5 / 'home')
    project_key5 = str((conv5 / 'demo' / '.knit/projects/demo.project.json').resolve())
    # The rejected repository moved to the scoped replacement; the shared
    # default's secret and default status are untouched.
    assert secrets(conv5 / 'home')['gh-bad'] == 'REJECTED-TOKEN'
    assert secrets(conv5 / 'home')['github.com-gh'] == 'REPLACEMENT-TOKEN'
    assert reg5['defaults'] == {'github.com': 'gh-bad'}
    assert reg5['projects'][project_key5] == {'solo': 'github.com-gh'}
    print('revoked default: scoped local replacement, original secret+default preserved: PASS', flush=True)

    # -----------------------------------------------------------------------
    # Conversation 6: the group's saved credential reads its token from an
    # environment variable that is not set. The clone reuses it, the
    # credential-unavailable failure is classified auth-shaped, and the
    # repair path saves a replacement as a new local credential scoped to
    # the affected repository — the env reference stays untouched.
    # -----------------------------------------------------------------------
    conv6 = root / 'conv6'
    conv6.mkdir()
    sources6 = {name: seed_source(name) for name in ['solo']}
    write_fake_git([
        ('https://github.com/org/solo.git', str(sources6['solo']), 'auth'),
    ])
    (conv6 / 'home').mkdir(parents=True, exist_ok=True)
    add_env6 = {'PATH': f'{root}/bin:/usr/bin:/bin', 'HOME': str(conv6 / 'home'),
                'KNIT_HOME': str(conv6 / 'home'),
                'GIT_CONFIG_GLOBAL': str(root / 'empty.gitconfig'),
                'GIT_CONFIG_NOSYSTEM': '1'}
    subprocess.run([binary, 'auth', 'add', 'gh-env', '--provider', 'github',
                    '--token-env', 'GHOST_ENV'], env=add_env6,
                   cwd=str(conv6),
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    repos6 = [('solo', 'https://github.com/org/solo.git', 'private')]
    (remote_dir / 'export.json').write_text(export_body(
        repos6, membership(repos6, [
            {'id': 'gh', 'name': 'Work', 'provider': 'github',
             'host': 'github.com', 'repos': ['solo'], 'tokenTypes': ['classic_pat']},
        ])))

    def env_missing_script(expect, answer):
        expect('Setting up project credentials before cloning:')
        expect('Host default `gh-env` covers github.com — using it for group `gh` without repository links.')
        # The unset environment reference fails the clone and is classified
        # auth-shaped; the repair path never touches the env reference.
        expect('Private repositories: 1 repo(s) need a forge token')
        expect('Credential `gh-env`')
        answer('Replacement token for github.com (saved as a new local credential for solo; '
               '`gh-env` keeps its token — hidden): ',
               'DIRECT-TOKEN', hidden=True)
        expect('Recovered after setup: solo')
        expect('Imported: 1 repo(s)')

    conversation(conv6, conv6 / 'home', [
        'clone', 'demo', '--remote', 'hosted', '--url', BASE_URL,
        '--token', 'test-token', '--no-worktree'], env_missing_script)
    assert (conv6 / 'demo' / 'solo' / '.git').exists()
    reg6 = registry(conv6 / 'home')
    project_key6 = str((conv6 / 'demo' / '.knit/projects/demo.project.json').resolve())
    # The repository moved to the new local credential; the env-backed one is
    # unchanged and still carries no local token.
    assert reg6['projects'][project_key6] == {'solo': 'github.com-gh'}
    assert secrets(conv6 / 'home')['github.com-gh'] == 'DIRECT-TOKEN'
    assert 'gh-env' not in secrets(conv6 / 'home')
    assert reg6['credentials']['gh-env']['tokenEnv'] == 'GHOST_ENV'
    print('unset env credential: guided replacement without flags: PASS', flush=True)

    # -----------------------------------------------------------------------
    # Conversation 7 (the primary flow): bare `knit auth` sets personal
    # default tokens for two forges with no project involved, then a normal
    # clone needs no prompts, flags, or bindings; `knit auth --project`
    # gives one project its own github token while the default and other
    # state stay untouched, and switching back to the default clears only
    # that project's override.
    # -----------------------------------------------------------------------
    conv7 = root / 'conv7'
    (conv7 / 'plain').mkdir(parents=True)
    (conv7 / 'home').mkdir(parents=True)

    def global_wizard_script(expect, answer):
        expect('Personal forge tokens')
        expect('Current default tokens:')
        expect('(none yet)')
        expect('1. GitHub (github.com)')
        answer('Forge (1-4, or Enter to finish): ', '1')
        answer('Token for github.com (hidden): ', 'GH-MAIN', hidden=True)
        expect('`github.com` is now the default token for github.com.')
        answer('Forge (1-4, or Enter to finish): ', '3')
        answer('Which kind of Bitbucket token is it? (name/number): ', '1')
        answer('Atlassian account email for this API token: ', 'dev@example.org')
        answer('Token for bitbucket.org (hidden): ', 'BB-MAIN', hidden=True)
        expect('`bitbucket.org` is now the default token for bitbucket.org.')
        answer('Forge (1-4, or Enter to finish): ', '')
        expect('Done. Tokens are saved in your personal Knit store')

    conversation(conv7 / 'plain', conv7 / 'home', ['auth'], global_wizard_script)
    reg7 = registry(conv7 / 'home')
    assert reg7['defaults'] == {'github.com': 'github.com',
                                'bitbucket.org': 'bitbucket.org'}
    assert secrets(conv7 / 'home') == {'github.com': 'GH-MAIN',
                                       'bitbucket.org': 'BB-MAIN'}
    assert reg7['credentials']['bitbucket.org']['username'] == 'dev@example.org'
    assert reg7['projects'] == {}

    # A normal clone on both forges: zero prompts, zero flags, zero bindings.
    sources7 = {name: seed_source(name) for name in ['svc', 'docs']}
    write_fake_git([
        ('https://github.com/org/svc.git', str(sources7['svc']), 'auth'),
        ('https://bitbucket.org/acme/docs.git', str(sources7['docs']), 'auth'),
    ])
    repos7 = [('svc', 'https://github.com/org/svc.git', 'private'),
              ('docs', 'https://bitbucket.org/acme/docs.git', 'private')]
    (remote_dir / 'export.json').write_text(export_body(repos7))
    before7 = forge_credential_hits()

    def defaults_clone_script(expect, answer):
        expect('Imported: 2 repo(s)')

    conversation(conv7 / 'plain', conv7 / 'home', [
        'clone', 'demo', '--remote', 'hosted', '--url', BASE_URL,
        '--token', 'test-token', '--no-worktree'], defaults_clone_script)
    assert (conv7 / 'plain' / 'demo' / 'svc' / '.git').exists()
    assert (conv7 / 'plain' / 'demo' / 'docs' / '.git').exists()
    reg7b = registry(conv7 / 'home')
    project_key7 = str((conv7 / 'plain' / 'demo' / '.knit/projects/demo.project.json').resolve())
    assert reg7b.get('projects', {}).get(project_key7) is None
    assert forge_credential_hits() == before7, \
        'defaults-only clone read the hosted forge-credential endpoint'

    # Project wizard: github gets a project-only token; bitbucket keeps the
    # default; then switching github back clears only this project's override.
    # Hosts iterate in sorted order: bitbucket.org before github.com.
    def project_wizard_script(expect, answer):
        expect('Project `demo` — per forge, use the shared default token or give this project its own.')
        expect('bitbucket.org (docs): default token `bitbucket.org`')
        answer('Use the default token (Enter), `t` for a project-only token, or `s` to skip: ', '')
        expect('Using the default token for bitbucket.org.')
        expect('github.com (svc): default token `github.com`')
        answer('Use the default token (Enter), `t` for a project-only token, or `s` to skip: ', 't')
        answer('Token for github.com (demo — hidden): ', 'PROJ-TOK', hidden=True)
        expect('`github.com-demo` is used for svc in this project only; the default token for github.com is untouched.')
        expect('Done.')

    conversation(conv7 / 'plain' / 'demo', conv7 / 'home',
                 ['auth', '--project', 'demo'], project_wizard_script)
    reg7c = registry(conv7 / 'home')
    assert reg7c['projects'][project_key7] == {'svc': 'github.com-demo'}
    assert secrets(conv7 / 'home')['github.com-demo'] == 'PROJ-TOK'
    # The project token is scoped: it never becomes the implicit default, and
    # the global defaults/secrets are untouched.
    assert 'github.com-demo' in reg7c.get('scopedCredentials', [])
    assert reg7c['defaults'] == {'github.com': 'github.com',
                                 'bitbucket.org': 'bitbucket.org'}
    assert secrets(conv7 / 'home')['github.com'] == 'GH-MAIN'

    def switch_back_script(expect, answer):
        expect('bitbucket.org (docs): default token `bitbucket.org`')
        answer('Use the default token (Enter), `t` for a project-only token, or `s` to skip: ', '')
        expect('Using the default token for bitbucket.org.')
        expect('svc: project token `github.com-demo`')
        answer('Use the default token (Enter), `t` for a project-only token, or `s` to skip: ', '')
        expect('Using the default token for github.com.')
        expect('Done.')

    conversation(conv7 / 'plain' / 'demo', conv7 / 'home',
                 ['auth', '--project', 'demo'], switch_back_script)
    reg7d = registry(conv7 / 'home')
    assert reg7d.get('projects', {}).get(project_key7) is None
    assert reg7d['defaults'] == {'github.com': 'github.com',
                                 'bitbucket.org': 'bitbucket.org'}
    assert secrets(conv7 / 'home')['github.com'] == 'GH-MAIN'
    print('bare `knit auth`: global defaults, prompt-free clone, project override and back: PASS', flush=True)

    # A deliberate global replacement converts an environment-backed default
    # to a saved secret, without changing its name or other host defaults.
    reg7d['credentials']['github.com']['tokenEnv'] = 'KNIT_TEST_UNSET_DEFAULT'
    (conv7 / 'home' / 'forge-auth.json').write_text(json.dumps(reg7d))

    def replace_env_default(expect, answer):
        answer('Forge (1-4, or Enter to finish): ', '1')
        answer('Enter to keep it, or `t` to paste a replacement token: ', 't')
        answer('New token for github.com (hidden): ', 'GH-ROTATED', hidden=True)
        expect('Updated the token on `github.com`; it stays the default for github.com.')
        answer('Forge (1-4, or Enter to finish): ', '')

    conversation(conv7 / 'plain', conv7 / 'home', ['auth'], replace_env_default)
    rotated = registry(conv7 / 'home')
    assert not rotated['credentials']['github.com'].get('tokenEnv')
    assert secrets(conv7 / 'home')['github.com'] == 'GH-ROTATED'
    assert rotated['defaults'] == reg7d['defaults']

    # Legacy stores with several host tokens can explicitly establish a
    # default through the same bare wizard.
    rotated['defaults'].pop('github.com')
    rotated.pop('scopedCredentials', None)
    (conv7 / 'home' / 'forge-auth.json').write_text(json.dumps(rotated))

    def choose_legacy_default(expect, answer):
        answer('Forge (1-4, or Enter to finish): ', '1')
        answer('Token for github.com (hidden): ', 'GH-CHOSEN', hidden=True)
        expect('`github.com-2` is now the default token for github.com.')
        answer('Forge (1-4, or Enter to finish): ', '')

    conversation(conv7 / 'plain', conv7 / 'home', ['auth'], choose_legacy_default)
    assert registry(conv7 / 'home')['defaults']['github.com'] == 'github.com-2'
    assert secrets(conv7 / 'home')['github.com-2'] == 'GH-CHOSEN'
    print('bare auth: environment replacement and ambiguous legacy default: PASS', flush=True)

    server.shutdown()
    print('clone_guided_pty: all conversations PASS')
    sys.stdout.flush()
    sys.stderr.flush()
    os._exit(0)
except Exception:
    import traceback
    traceback.print_exc()
    sys.stdout.flush()
    sys.stderr.flush()
    os._exit(1)
