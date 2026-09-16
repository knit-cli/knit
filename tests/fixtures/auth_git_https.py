"""Real local smart-HTTPS auth coverage; invoked by project_git_auth.rs.

Unix CI requires Python 3.9+, openssl (req -addext), and Git with http-backend.
All credentials are synthetic. The CONNECT proxy accepts only the four explicit
*.auth.test hosts below and serves them locally; it never forwards traffic.

Besides the Knit-launched operations, a plain-Git phase drives real `git
fetch`/`pull`/`push` with no Knit wrapper: the Knit-owned repository include
(knit-credentials.inc) must resolve the same selections, reset ambient
credential helpers per host, and rewrite the SSH remote to HTTPS. Plain Git
may probe unauthenticated first (a 401 challenge); an authenticated request
with a wrong credential is always a failure.
"""

import base64
import collections
import json
import os
import pathlib
import shutil
import socketserver
import ssl
import subprocess
import sys
import tempfile
import threading
import traceback
import urllib.parse


def main(binary, root):
    workspace = root / 'workspace'
    workspace.mkdir()
    bare_root = root / 'remotes'
    bare_root.mkdir()
    env = {'PATH': os.environ['PATH'], 'KNIT_HOME': str(root / 'personal'),
           'GIT_CONFIG_GLOBAL': str(root / 'gitconfig'), 'GIT_CONFIG_NOSYSTEM': '1',
           'GIT_TERMINAL_PROMPT': '0', 'KNIT_ADVICE': 'false',
           # Plain Git must never hang on a host-key or password prompt if an
           # SSH remote is ever (wrongly) left unwritten by the rewrite.
           'GIT_SSH_COMMAND': 'ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o ConnectTimeout=3'}
    specs = [
        ('a', 'github', 'github.auth.test', 'shared', 'x-access-token', 'secret-shared'),
        ('b', 'github', 'github.auth.test', 'shared', 'x-access-token', 'secret-shared'),
        ('c', 'github', 'github.auth.test', 'restricted', 'x-access-token', 'secret-restricted'),
        ('bb', 'bitbucket', 'bitbucket.auth.test', 'cloud', 'x-bitbucket-api-token-auth', 'secret-cloud'),
        ('gl', 'gitlab', 'gitlab.auth.test', 'lab', 'oauth2', 'secret-lab'),
        ('cb', 'forgejo', 'codeberg.auth.test', 'berg', 'oauth2', 'secret-berg'),
    ]
    for _, _, _, name, _, token in specs:
        env['FIXTURE_' + name.upper()] = token
    calls = []
    credential_mismatches = []
    reject = set()
    redirect = set()
    thread_errors = []
    # Plain Git (no proactiveAuth) first probes unauthenticated and retries
    # after the 401 challenge. During that phase an anonymous request is a
    # legitimate challenge, never a failure; a wrong authenticated credential
    # always is, in every phase.
    allow_anonymous_challenge = False

    def check_thread_errors():
        assert not thread_errors, "HTTPS fixture thread errors:\n" + "\n".join(thread_errors)

    def run(args, cwd=workspace, success=True):
        result = subprocess.run(args, cwd=cwd, env=env, stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE, text=True, timeout=90)
        check_thread_errors()
        output = result.stdout + result.stderr
        for *_, token in specs:
            assert token not in output, 'secret in captured CLI output'
        if success:
            assert result.returncode == 0, f'{args}: {output}'
        else:
            assert result.returncode != 0, f'Unexpected success: {args}'
        return result

    class Handler(socketserver.BaseRequestHandler):
        def handle(self):
            try:
                self.request.settimeout(10)
                reader = self.request.makefile('rb')
                line = reader.readline().decode().strip()
                assert line.startswith('CONNECT '), line
                host = line.split()[1].rsplit(':', 1)[0]
                assert host in {s[2] for s in specs}, host
                while reader.readline() not in (b'\r\n', b'\n', b''):
                    pass
                self.request.sendall(b'HTTP/1.1 200 Connection established\r\n\r\n')
                with tls.wrap_socket(self.request, server_side=True) as connection:
                    reader = connection.makefile('rb')
                    request = reader.readline().decode().strip().split()
                    if not request:
                        return
                    method, target, _ = request
                    headers = {}
                    while True:
                        line = reader.readline()
                        if line in (b'\r\n', b'\n', b''):
                            break
                        key, value = line.decode().split(':', 1)
                        headers[key.lower()] = value.strip()
                    parsed = urllib.parse.urlsplit(target)
                    repo = parsed.path.split('/')[2].removesuffix('.git')
                    spec = next((s for s in specs if s[0] == repo and s[2] == host), None)
                    expected = ('Basic ' + base64.b64encode((spec[4] + ':' + spec[5]).encode()).decode()) if spec else None
                    has_auth = 'authorization' in headers
                    accepted = spec is not None and has_auth and headers.get('authorization') == expected
                    calls.append((repo, method, parsed.path, accepted, has_auth))
                    if has_auth and not accepted:
                        credential_mismatches.append((repo, method, parsed.path, has_auth))
                    # Access is strict in every phase: an unauthenticated or
                    # wrong-credential request always gets the 401 challenge.
                    # The anonymous-challenge flag below only relaxes the
                    # *assertions* about plain Git's first probe, never access.
                    if not accepted or repo in reject:
                        connection.sendall(b'HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm="fixture"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
                        return
                    if repo in redirect:
                        connection.sendall(b'HTTP/1.1 302 Found\r\nLocation: https://github.auth.test/team/c.git/info/refs?service=git-upload-pack\r\nContent-Length: 0\r\nConnection: close\r\n\r\n')
                        return
                    body = reader.read(int(headers.get('content-length', '0')))
                    cgi = dict(env, GIT_PROJECT_ROOT=str(bare_root), GIT_HTTP_EXPORT_ALL='1',
                               PATH_INFO=parsed.path, QUERY_STRING=parsed.query,
                               REQUEST_METHOD=method, CONTENT_TYPE=headers.get('content-type', ''),
                               CONTENT_LENGTH=str(len(body)), REMOTE_USER='fixture',
                               SERVER_PROTOCOL='HTTP/1.1', REMOTE_ADDR='127.0.0.1')
                    response = subprocess.run(['git', 'http-backend'], input=body,
                                              env=cgi, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
                    assert response.returncode == 0, response.stderr.decode(errors='replace')
                    raw_headers, data = response.stdout.split(b'\r\n\r\n', 1)
                    status = b'200 OK'
                    lines = []
                    for line in raw_headers.split(b'\r\n'):
                        if line.lower().startswith(b'status:'):
                            status = line.split(b':', 1)[1].strip()
                        else:
                            lines.append(line)
                    connection.sendall(b'HTTP/1.1 ' + status + b'\r\n' + b'\r\n'.join(lines) +
                                       b'\r\nContent-Length: ' + str(len(data)).encode() +
                                       b'\r\nConnection: close\r\n\r\n' + data)
            except Exception:
                thread_errors.append(traceback.format_exc())

    class Server(socketserver.ThreadingTCPServer):
        daemon_threads = False
        allow_reuse_address = True

    server = None
    try:
        subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes',
                        '-keyout', str(root/'key.pem'), '-out', str(root/'cert.pem'),
                        '-days', '1', '-subj', '/CN=*.auth.test', '-addext', 'subjectAltName=DNS:*.auth.test'],
                       check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=30)
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(root/'cert.pem', root/'key.pem')
        server = Server(('127.0.0.1', 0), Handler)
        server_thread = threading.Thread(target=server.serve_forever, daemon=True)
        server_thread.start()
        config = ('[user]\n name = Fixture\n email = fixture@example.invalid\n'
                  '[init]\n defaultBranch = main\n[http]\n proxy = http://127.0.0.1:' + str(server.server_address[1]) +
                  '\n sslCAInfo = ' + str(root/'cert.pem') + '\n'
                  '[credential]\n helper = "!f() { echo username=ambient; echo password=ambient-wrong; }; f"\n')
        (root/'gitconfig').write_text(config)
        for repo, provider, host, credential, username, token in specs:
            checkout = workspace/repo
            run(['git', 'init', str(checkout)])
            (checkout/'README').write_text(repo)
            run(['git', 'add', '.'], checkout)
            run(['git', 'commit', '-m', 'seed'], checkout)
            bare = bare_root/'team'/f'{repo}.git'
            bare.parent.mkdir(exist_ok=True)
            run(['git', 'clone', '--bare', str(checkout), str(bare)])
            run(['git', 'config', 'http.receivepack', 'true'], bare)
            run(['git', 'remote', 'add', 'origin', str(bare)], checkout)
            run(['git', 'fetch', 'origin'], checkout)
            remote = f'git@{host}:team/{repo}.git' if repo == 'b' else f'https://{host}/team/{repo}.git'
            run(['git', 'remote', 'set-url', 'origin', remote], checkout)
        run([binary, 'init', 'demo'])
        added = set()
        for repo, provider, host, credential, username, token in specs:
            run([binary, 'project', 'add', repo, str(workspace/repo), '--base', 'main'])
            if credential not in added:
                args = [binary, 'auth', 'add', credential, '--provider', provider, '--host', host,
                        '--token-env', 'FIXTURE_' + credential.upper()]
                if provider == 'bitbucket':
                    args += ['--username', 'fixture@example.invalid']
                run(args)
                added.add(credential)
            run([binary, 'auth', 'use', credential, '--repo', repo])
        # An inherited exact-URL Authorization header (a synthetic wrong
        # credential, base64 of "forgejo:invalid") must never beat Knit's
        # selection in plain Git: the generated repository config resets
        # inherited HTTP auth settings for taken-over targets.
        with (root / 'gitconfig').open('a') as handle:
            handle.write('[http "https://github.auth.test/team/a.git"]\n'
                         '\textraHeader = Authorization: Basic Zm9yZWlnbjppbnZhbGlk\n')
        originals = {repo: run(['git', 'config', '--get-regexp', r'^(remote\.|credential\.|http\.|url\.)'], workspace/repo).stdout for repo, *_ in specs}
        # Project setup may probe remotes before credentials are assigned.
        # Measure transport isolation only after every assignment is installed.
        calls.clear()
        credential_mismatches.clear()
        run([binary, 'bundle', 'auth-validation', '--offline'])
        run([binary, '--bundle', 'auth-validation', 'git', '--repo', 'a', 'ls-remote', 'origin', 'HEAD'])
        run([binary, 'auth', 'status', '--check'])
        assert {repo for repo, method, path, accepted, _ in calls
                if method == 'GET' and path.endswith('/info/refs') and accepted} == {s[0] for s in specs}
        print('PASS: real Git HTTPS reads for six repos, five credentials and four backend types', flush=True)
        run([binary, '--bundle', 'auth-validation', 'push', '--set-upstream'])
        for repo, *_ in specs:
            run(['git', 'rev-parse', '--verify', 'refs/heads/knit/auth-validation'], bare_root/'team'/f'{repo}.git')
        print('PASS: real smart-HTTP pushes created all six remote feature branches', flush=True)
        assert all(call[3] for call in calls), f'wrong credential reached a repository (repo, method, path, auth present): {credential_mismatches}'
        assert {repo for repo, method, path, _, _ in calls if method == 'POST' and path.endswith('/git-receive-pack')} == {s[0] for s in specs}
        reject.add('a')
        before = len(calls)
        run([binary, 'auth', 'status', '--check'], success=False)
        assert any(repo == 'a' for repo, *_ in calls[before:])
        assert all(call[3] for call in calls[before:]), 'fell back to ambient credential'
        reject.clear()
        print('PASS: server rejection fails without trying ambient credentials', flush=True)
        redirect.add('a')
        before = len(calls)
        run([binary, '--bundle', 'auth-validation', 'git', '--repo', 'a', 'ls-remote', 'origin', 'HEAD'], success=False)
        assert len(calls) > before, 'redirect test did not reach server'
        assert all(repo == 'a' for repo, *_ in calls[before:]), 'followed cross-repository redirect'
        redirect.clear()
        print('PASS: cross-repository redirect blocked', flush=True)
        run([binary, 'auth', 'clear', '--repo', 'a'])
        before = len(calls)
        run([binary, '--bundle', 'auth-validation', 'git', '--repo', 'a', 'ls-remote', 'origin', 'HEAD'])
        assert len(calls) > before and all(call[3] for call in calls[before:])
        print('PASS: clearing an override restores the shared host default', flush=True)
        # With only project-scoped tokens, the same missing assignment must
        # still fail before transport.
        registry_path = root / 'personal' / 'forge-auth.json'
        registry = json.loads(registry_path.read_text())
        registry['defaults'].pop('github.auth.test')
        registry['scopedCredentials'] = ['shared', 'restricted']
        registry_path.write_text(json.dumps(registry))
        before = len(calls)
        run([binary, '--bundle', 'auth-validation', 'git', '--repo', 'a', 'ls-remote', 'origin', 'HEAD'], success=False)
        assert len(calls) == before, 'unassigned repository reached network'
        print('PASS: missing link blocks network access', flush=True)
        for repo, *_ in specs:
            assert run(['git', 'config', '--get-regexp', r'^(remote\.|credential\.|http\.|url\.)'], workspace/repo).stdout == originals[repo], 'source remote/auth config changed'
        print('PASS: remote/auth Git configuration unchanged; no token in CLI output', flush=True)

        # --- Plain Git (no Knit wrapper) ---------------------------------
        # The earlier phase cleared repo a's assignment and popped the host
        # default; restore the shared selection before exercising plain Git.
        run([binary, 'auth', 'use', 'shared', '--repo', 'a'])
        # Existing installs activate via two noninteractive `auth status`
        # refreshes; the second must rewrite nothing (idempotence).
        run([binary, 'auth', 'status'])
        include_a = workspace / 'a' / '.git' / 'knit-credentials.inc'
        include_b = workspace / 'b' / '.git' / 'knit-credentials.inc'
        assert include_a.exists(), 'plain-Git include missing for repo a'
        assert include_b.exists(), 'plain-Git include missing for SSH repo b'
        content_after_first = include_a.read_text()
        assert 'auth git-credential --resolve' in content_after_first
        assert 'secret-shared' not in content_after_first and 'secret-restricted' not in content_after_first
        # The SSH remote of b keeps its saved URL; the exact HTTPS rewrite is
        # asserted through Git's own (unescaped) config view.
        assert run(['git', 'config', '--get-all', 'url.https://github.auth.test/team/b.git.insteadOf'], workspace/'b').stdout.strip() == 'git@github.auth.test:team/b.git'
        assert 'git@github.auth.test:team/b.git' in run(['git', 'config', '--get', 'remote.origin.url'], workspace/'b').stdout
        run([binary, 'auth', 'status'])
        assert include_a.read_text() == content_after_first, 'second activation rewrote the include'
        # The inherited exact-URL Authorization header is knocked out for the
        # taken-over repository: plain Git starts anonymous, not wrongly
        # authenticated by an inherited header.
        assert run(['git', 'config', '--get-urlmatch', 'http.extraHeader', 'https://github.auth.test/team/a.git'], workspace/'a').stdout.strip() == '', 'inherited HTTP Authorization header was not reset'
        print('PASS: two auth-status activations install an idempotent plain-Git include', flush=True)

        allow_anonymous_challenge = True
        plain_start = len(calls)
        before = len(calls)
        # Reads across providers and selection kinds: host defaults (a, bb,
        # gl, cb) and the project override (c: restricted).
        for repo in ['a', 'c', 'bb', 'gl', 'cb']:
            run(['git', 'ls-remote', 'origin', 'HEAD'], workspace/repo)
        assert len(calls) > before, 'plain ls-remote did not reach the server'
        # Real plain fetches, including the SSH-remote repository b whose
        # generated rewrite must ride the HTTPS transport.
        for repo in ['a', 'b']:
            run(['git', 'fetch', 'origin'], workspace/repo)
        # A linked bundle worktree resolves from its own context too.
        run(['git', 'fetch', 'origin'], workspace / '.knit' / 'worktrees' / 'auth-validation' / 'a')
        print('PASS: plain Git ls-remote and fetch across five credentials, SSH rewrite included', flush=True)

        # An advancing plain pull: new commits land on the bare remotes via
        # the local filesystem, then `git pull --ff-only` must fetch them.
        for repo in ['a', 'b', 'bb']:
            bare = bare_root / 'team' / f'{repo}.git'
            advancing = root / f'advancing-{repo}'
            run(['git', 'clone', str(bare), str(advancing)])
            (advancing / 'ADVANCE.txt').write_text(f'{repo} advanced\n')
            run(['git', 'add', 'ADVANCE.txt'], advancing)
            run(['git', 'commit', '-m', 'advance for plain pull'], advancing)
            run(['git', 'push', 'origin', 'main'], advancing)
            expected_head = run(['git', 'rev-parse', 'HEAD'], advancing).stdout
            before = len(calls)
            run(['git', 'pull', '--ff-only', 'origin', 'main'], workspace/repo)
            assert len(calls) > before, f'plain pull for {repo} did not reach the server'
            assert run(['git', 'rev-parse', 'HEAD'], workspace/repo).stdout == expected_head, f'plain pull did not advance {repo}'
        print('PASS: plain Git pull advances github (default, override host), bitbucket, and SSH-rewritten remotes', flush=True)

        # Plain commits and pushes create real branches through receive-pack.
        for repo, branch in [('a', 'plain-git'), ('b', 'plain-git-ssh'), ('bb', 'plain-git-bb')]:
            checkout = workspace / repo
            (checkout / 'PLAIN.txt').write_text(f'{repo} plain push\n')
            run(['git', 'add', 'PLAIN.txt'], checkout)
            run(['git', 'commit', '-m', 'plain git push'], checkout)
            before = len(calls)
            run(['git', 'push', 'origin', f'HEAD:refs/heads/{branch}'], checkout)
            assert len(calls) > before, f'plain push for {repo} did not reach the server'
            run(['git', 'rev-parse', '--verify', f'refs/heads/{branch}'], bare_root/'team'/f'{repo}.git')
        print('PASS: plain Git commit/push on GitHub default, SSH-rewritten, and Bitbucket repositories', flush=True)

        # Anonymous requests in the plain window were only the documented
        # first 401 challenge: each is followed by an accepted retry, and no
        # wrong credential ever reached the server (ambient-wrong is
        # configured globally).
        window = calls[plain_start:]
        assert not credential_mismatches, f'wrong credential reached a repository: {credential_mismatches}'
        for index, (repo, method, path, accepted, has_auth) in enumerate(window):
            assert not has_auth or accepted, f'wrong credential for {repo} {method} {path}'
            if accepted or has_auth:
                continue
            assert method == 'GET' and path.endswith('/info/refs'), (repo, method, path)
            assert any(later[0] == repo and later[2] == path and later[3]
                       for later in window[index + 1:]), f'anonymous probe for {repo} was never retried with credentials'
        allow_anonymous_challenge = False
        print('PASS: anonymous requests were 401 challenges only; every authenticated request was correct', flush=True)

        # Fail-closed removal check: with no assignment, no default, and both
        # GitHub tokens marked project-scoped, repo a refuses selection. Plain
        # Git may probe unauthenticated first and the helper refuses at the
        # latest then — so at most anonymous 401 challenges may appear, never
        # an authenticated request and never a wrong credential.
        run([binary, 'auth', 'clear', '--repo', 'a'])
        marker = len(calls)
        run(['git', 'ls-remote', 'origin', 'HEAD'], workspace/'a', success=False)
        refused = calls[marker:]
        assert all(not has_auth for *_, has_auth in refused), f'authenticated request after a refusal: {refused}'
        assert not credential_mismatches
        # Restoring an assignment (and refreshing) re-enables plain Git: the
        # first probe may stay anonymous, the retry must be accepted.
        run([binary, 'auth', 'use', 'shared', '--repo', 'a'])
        run([binary, 'auth', 'status'])
        marker = len(calls)
        run(['git', 'ls-remote', 'origin', 'HEAD'], workspace/'a')
        restored = calls[marker:]
        assert any(accepted for *_, accepted, _ in restored), 'restored assignment never authenticated'
        assert all(accepted or not has_auth for *_, accepted, has_auth in restored), 'restored assignment used a wrong credential'
        print('PASS: missing selection fails closed without ambient fallback; reassignment restores access', flush=True)

        for repo, *_ in specs:
            assert run(['git', 'config', '--get-regexp', r'^(remote\.|credential\.|http\.|url\.)'], workspace/repo).stdout == originals[repo], 'source remote/auth config changed'
        print('HTTP request counts:', dict(collections.Counter(repo for repo, *_ in calls)), flush=True)
    finally:
        if server:
            server.shutdown()
            server.server_close()
            server_thread.join(timeout=10)
            assert not server_thread.is_alive(), "HTTPS fixture server did not stop"
        check_thread_errors()


if __name__ == "__main__":
    assert sys.version_info >= (3, 9), "HTTPS auth fixture requires Python 3.9+"
    assert len(sys.argv) == 2, "usage: auth_git_https.py /path/to/built/knit"
    for dependency in ("git", "openssl"):
        assert shutil.which(dependency), f"HTTPS auth fixture requires {dependency} on PATH"
    binary = str(pathlib.Path(sys.argv[1]).resolve(strict=True))
    try:
        with tempfile.TemporaryDirectory(prefix="knit-auth-real-https-") as directory:
            main(binary, pathlib.Path(directory).resolve())
    except subprocess.CalledProcessError as error:
        raise AssertionError(
            f"HTTPS fixture setup failed: {error.cmd}: {error.stderr!r}"
        ) from error
