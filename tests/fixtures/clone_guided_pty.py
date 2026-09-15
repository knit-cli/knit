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
import errno
import json
import os
from pathlib import Path
import pty
import select
import signal
import socketserver
import subprocess
import sys
import termios
import threading
import time

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
        f'  {url}) mode={mode}; src={src} ;;\n' for url, src, mode in mapping)
    script = r'''#!/bin/sh
set -u
real_git=__REAL_GIT__
root=__ROOT__
op=
url=
for arg in "$@"; do
  if [ -n "$op" ] && [ -z "$url" ]; then
    case "$arg" in
      -*) ;;
      *) url="$arg" ;;
    esac
  fi
  case "$arg" in
    clone|ls-remote) op="$arg" ;;
  esac
done
if [ -z "$op" ]; then
  exec "$real_git" "$@"
fi
n=$(cat "$root/git-seq" 2>/dev/null || echo 0); n=$((n + 1))
printf '%s\n' "$n" > "$root/git-seq"
d="$root/git-call-$n"; mkdir -p "$d"
printf '%s\n' "$@" > "$d/args"
helper=
for arg in "$@"; do
  case "$arg" in
    credential.https://*.helper=!*) helper=${arg#*=!} ;;
  esac
done
printf '%s' "$helper" > "$d/helper"
mode=
src=
case "$url" in
__CASES__
  *) mode=unknown ;;
esac
printf '%s' "${mode:-unknown}" > "$d/mode"
if [ "$op" = ls-remote ]; then
  if [ "$mode" = auth ] && [ -z "$helper" ]; then
    printf 'fatal: could not read Username: terminal prompts disabled\n' >&2
    exit 128
  fi
  if [ -f "$root/forge-rejects" ] && [ "$mode" = auth ]; then
    printf "fatal: Authentication failed for '%s/'\n" "$url" >&2
    exit 128
  fi
  exit 0
fi
if [ "$mode" = unknown ]; then
  printf 'fake git: unexpected network url %s\n' "$url" >&2
  exit 1
fi
if [ "$mode" = auth ] && [ -z "$helper" ]; then
  printf "fatal: could not read Username for '%s': terminal prompts disabled\n" "$url" >&2
  exit 128
fi
if [ -f "$root/forge-rejects" ] && [ "$mode" = auth ]; then
  printf "fatal: Authentication failed for '%s/'\n" "$url" >&2
  exit 128
fi
target=
for arg in "$@"; do target="$arg"; done
"$real_git" clone -q "$src" "$target" || exit $?
exec "$real_git" -C "$target" remote set-url origin "$url"
'''.replace('__REAL_GIT__', f"'{real_git}'").replace('__ROOT__', f"'{root}'").replace('__CASES__', cases)
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
           'GIT_CONFIG_NOSYSTEM': '1', 'HOME': str(root / 'fixture-home')}
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
DEADLINE = time.monotonic() + 180


def conversation(cwd, home, args, script):
    env = {'PATH': f'{root}/bin:/usr/bin:/bin', 'HOME': str(home),
           'KNIT_HOME': str(home), 'GIT_CONFIG_GLOBAL': str(root / 'empty.gitconfig'),
           'GIT_CONFIG_NOSYSTEM': '1', 'TERM': 'dumb'}
    home.mkdir(parents=True, exist_ok=True)
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execve(binary, [binary] + args, env)
    transcript = b''
    unread = b''
    reaped = False

    def watchdog():
        while time.monotonic() < DEADLINE:
            time.sleep(0.5)
        print(f'FIXTURE TIMEOUT: {transcript.decode(errors="replace")!r}',
              file=sys.stderr, flush=True)
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        os._exit(1)

    threading.Thread(target=watchdog, daemon=True).start()

    def read_chunk(timeout=0.1):
        ready, _, _ = select.select([fd], [], [], timeout)
        if not ready:
            return None
        try:
            return os.read(fd, 65536)
        except OSError as e:
            if e.errno == errno.EIO:
                return b''
            raise

    def expect(needle):
        nonlocal transcript, unread
        wanted = needle.encode()
        prompt_deadline = min(DEADLINE, time.monotonic() + 30)
        while wanted not in unread:
            assert time.monotonic() < prompt_deadline, (
                f'timeout waiting for {needle!r}: {transcript.decode(errors="replace")}')
            chunk = read_chunk()
            if chunk is None:
                continue
            assert chunk, f'PTY closed waiting for {needle!r}: {transcript.decode(errors="replace")}'
            transcript += chunk
            unread += chunk
        unread = unread.split(wanted, 1)[1]

    def answer(prompt, text, hidden=False):
        expect(prompt)
        if hidden:
            deadline = time.monotonic() + 5
            while termios.tcgetattr(fd)[3] & termios.ECHO:
                assert time.monotonic() < deadline, 'password prompt never disabled echo'
                time.sleep(0.005)
        os.write(fd, (text + '\n').encode())

    try:
        script(expect, answer)
        exit_deadline = min(DEADLINE, time.monotonic() + 30)
        eof = False
        while not reaped:
            waited, status = os.waitpid(pid, os.WNOHANG)
            if waited:
                reaped = True
                assert os.waitstatus_to_exitcode(status) == 0, transcript.decode(errors='replace')
                break
            assert time.monotonic() < exit_deadline, (
                'conversation did not exit: ' + transcript.decode(errors='replace'))
            if eof:
                time.sleep(0.02)
                continue
            chunk = read_chunk(0.05)
            if chunk is None:
                continue
            if chunk:
                transcript += chunk
                unread += chunk
            else:
                eof = True
        return transcript
    finally:
        if not reaped:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.close(fd)
            os.waitpid(pid, 0)
        else:
            os.close(fd)


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
        # First group: one direct hidden token prompt.
        expect('Group gh-work: Work repos (github @ github.com)')
        expect('Token type(s): classic_pat')
        answer('Token for github.com (gh-work — hidden): ', 'CONV1-GH-WORK', hidden=True)
        # Second group on the SAME host must not silently reuse the first
        # group's brand-new credential: a choice with a new-token option.
        expect('Group gh-mobile: Mobile team (github @ github.com)')
        expect('Saved credentials for github @ github.com:')
        expect('n. enter a new token for this group')
        answer('Credential for github @ github.com (1-1, n, Enter = 1): ', 'n')
        answer('Token for github.com (gh-mobile — hidden): ', 'CONV1-GH-MOBILE', hidden=True)
        # Third group on its own host: direct prompt again.
        expect('Group gl-infra: Infra (gitlab @ gitlab.com)')
        answer('Token for gitlab.com (gl-infra — hidden): ', 'CONV1-GL', hidden=True)
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
    assert reg['projects'][project_key] == {
        'api': 'github.com-gh-work', 'web': 'github.com-gh-work',
        'mobile': 'github.com-gh-mobile', 'infra': 'gitlab.com-gl-infra'}
    assert secrets(conv1 / 'home') == {'github.com-gh-work': 'CONV1-GH-WORK',
                                       'github.com-gh-mobile': 'CONV1-GH-MOBILE',
                                       'gitlab.com-gl-infra': 'CONV1-GL'}
    assert (conv1 / 'demo' / 'api' / '.git').exists()
    assert (conv1 / 'demo' / 'mobile' / '.git').exists()
    print('guided grouped clone: direct prompts, same-host groups distinct: PASS', flush=True)

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
        expect('Recovered after setup: sec')
        expect('Imported: 2 repo(s)')

    conversation(conv2, conv2 / 'home', [
        'clone', 'demo', '--remote', 'hosted', '--url', BASE_URL,
        '--token', 'test-token', '--no-worktree'], inferred_script)
    assert (conv2 / 'demo' / 'pub' / '.git').exists()
    assert (conv2 / 'demo' / 'sec' / '.git').exists()
    project_key2 = str((conv2 / 'demo' / '.knit/projects/demo.project.json').resolve())
    reg2 = registry(conv2 / 'home')
    assert reg2['projects'][project_key2] == {'sec': 'github.com'}
    # The public repository keeps working through the strict gate: its exact
    # remote is recorded as ambient access.
    assert reg2['ambient'][project_key2]['pub'] == 'github.com/org/pub'
    print('inferred fallback: public ambient + one hidden prompt, ambient recorded: PASS', flush=True)

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
        expect('Project repo: added fresh')

    workspace = conv2 / 'demo'
    conversation(workspace, conv2 / 'home', ['pull', '--bundles'], pull_script)
    assert (workspace / 'fresh' / '.git').exists()
    reg3 = registry(conv2 / 'home')
    assert reg3['projects'][project_key2]['fresh'] == 'gitlab.com-fresh-grp'
    assert secrets(conv2 / 'home')['gitlab.com-fresh-grp'] == 'CONV3-FRESH'
    # A group-covered repository never gains an ambient allowance, even
    # though the export marks it public.
    assert 'fresh' not in reg3['ambient'].get(project_key2, {})
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
        expect('Imported: 2 repo(s)')

    conversation(conv4, conv4 / 'home', [
        'clone', 'demo', '--remote', 'hosted', '--url', BASE_URL,
        '--token', 'test-token', '--prefer-https', '--no-worktree'], scenario_d_script)
    assert (conv4 / 'demo' / 'd-gh' / '.git').exists()
    assert (conv4 / 'demo' / 'd-pub' / '.git').exists()
    project_key4 = str((conv4 / 'demo' / '.knit/projects/demo.project.json').resolve())
    reg4 = registry(conv4 / 'home')
    assert reg4['projects'][project_key4] == {'d-gh': 'github.com-d-gh-grp'}
    # The ungrouped public repository keeps ambient access through the strict
    # gate: its exact remote is recorded, and no hosted forge-credential
    # lookup was needed even with --prefer-https.
    assert reg4['ambient'][project_key4]['d-pub'] == 'github.com/org/d-pub', reg4
    assert forge_credential_hits() == before4, \
        'scenario D clone read the hosted forge-credential endpoint'
    print('scenario D: grouped private + ungrouped public ambient, no helper query: PASS', flush=True)

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
    subprocess.run([binary, 'auth', 'add', 'gh-bad', '--provider', 'github',
                    '--token-stdin'], input=b'REJECTED-TOKEN\n', env=add_env,
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    repos5 = [('solo', 'https://github.com/org/solo.git', 'private')]
    (remote_dir / 'export.json').write_text(export_body(
        repos5, membership(repos5, [
            {'id': 'gh', 'name': 'Work', 'provider': 'github',
             'host': 'github.com', 'repos': ['solo'], 'tokenTypes': ['classic_pat']},
        ])))

    def repair_script(expect, answer):
        expect('Setting up project credentials before cloning:')
        # The unique compatible saved credential is reused without asking...
        expect('Using saved credential `gh-bad` (github @ github.com) for this group.')
        # ...the forge rejects it, and the repair path offers one rotation.
        expect('Private repositories: 1 repo(s) need a forge token')
        expect('Credential `gh-bad`')
        expect('was used and access was denied for solo.')
        # The forge starts accepting the replacement token: unblock it before
        # answering the rotation prompt.
        (root / 'forge-rejects').unlink()
        answer('New token for `gh-bad` (Enter to keep the current one — hidden): ',
               'ROTATED-TOKEN', hidden=True)
        expect('Recovered after setup: solo')
        expect('Imported: 1 repo(s)')

    conversation(conv5, conv5 / 'home', [
        'clone', 'demo', '--remote', 'hosted', '--url', BASE_URL,
        '--token', 'test-token', '--no-worktree'], repair_script)
    assert (conv5 / 'demo' / 'solo' / '.git').exists()
    assert secrets(conv5 / 'home')['gh-bad'] == 'ROTATED-TOKEN'
    project_key5 = str((conv5 / 'demo' / '.knit/projects/demo.project.json').resolve())
    assert registry(conv5 / 'home')['projects'][project_key5] == {'solo': 'gh-bad'}
    print('revoked group token: denied clone rotates it in place and recovers: PASS', flush=True)

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
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    repos6 = [('solo', 'https://github.com/org/solo.git', 'private')]
    (remote_dir / 'export.json').write_text(export_body(
        repos6, membership(repos6, [
            {'id': 'gh', 'name': 'Work', 'provider': 'github',
             'host': 'github.com', 'repos': ['solo'], 'tokenTypes': ['classic_pat']},
        ])))

    def env_missing_script(expect, answer):
        expect('Setting up project credentials before cloning:')
        expect('Using saved credential `gh-env` (github @ github.com) for this group.')
        # The unset environment reference fails the clone and is classified
        # auth-shaped; the repair path never touches the env reference.
        expect('Private repositories: 1 repo(s) need a forge token')
        expect('Credential `gh-env`')
        answer('Replacement token for github.com (saved as a new local credential; '
               '`gh-env` keeps reading its environment variable — hidden): ',
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
