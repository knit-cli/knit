"""Real Git full-clone regression: token kind controls HTTPS authentication.

All traffic is served on loopback; no real forge or user credentials are used.
Run with a binary path; --expect-rejection verifies the pre-fix binary fails.
Requires Git, Python 3, and OpenSSL (as available on Unix CI runners).
"""
import base64
import contextlib
import http.server
import json
import os
from pathlib import Path
import shlex
import socketserver
import ssl
import subprocess
import sys
import tempfile
import threading


@contextlib.contextmanager
def serving(server):
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def scenario(binary, kind, expect_rejection):
    with tempfile.TemporaryDirectory(prefix='knit-typed-https-') as directory:
        root = Path(directory).resolve()
        home = root / 'home'
        home.mkdir()
        config = home / 'gitconfig'
        config.write_text('')
        # Start with no ambient credential, proxy, Git configuration, or .netrc.
        env = {'PATH': os.environ['PATH'], 'HOME': str(home),
               'KNIT_HOME': str(home), 'GIT_CONFIG_GLOBAL': str(config),
               'GIT_CONFIG_NOSYSTEM': '1', 'GIT_TERMINAL_PROMPT': '0',
               'GIT_AUTHOR_NAME': 'Test', 'GIT_AUTHOR_EMAIL': 'test@example.invalid',
               'GIT_COMMITTER_NAME': 'Test', 'GIT_COMMITTER_EMAIL': 'test@example.invalid'}
        tokens = {'github.com': 'synthetic-gh-secret',
                  'bitbucket.org': 'synthetic-bb-secret'}
        usernames = {'github.com': 'x-access-token', 'bitbucket.org':
                     'x-token-auth' if kind == 'access_token' else 'x-bitbucket-api-token-auth'}
        headers = {host: 'Basic ' + base64.b64encode(
            f'{usernames[host]}:{token}'.encode()).decode() for host, token in tokens.items()}

        def run(args, cwd=root, stdin=None, check=True):
            result = subprocess.run([str(arg) for arg in args], cwd=cwd, env=env,
                                    input=stdin, text=True, capture_output=True, timeout=45)
            if check and result.returncode:
                output = result.stdout + result.stderr
                for value in [*tokens.values(), *headers.values()]:
                    output = output.replace(value, '[REDACTED]')
                raise AssertionError(f'{Path(str(args[0])).name} failed: {output[-3000:]}')
            return result

        marker = root / 'unexpected-native-auth'
        denied = root / 'deny-native'
        denied.write_text('#!/bin/sh\nprintf attempted >> ' + shlex.quote(str(marker)) + '\nexit 1\n')
        denied.chmod(0o755)
        env['GIT_SSH_COMMAND'] = str(denied)
        env['GIT_ASKPASS'] = str(denied)
        run(['git', 'config', '--global', 'credential.helper', str(denied)])
        webroot = root / 'web'
        revisions = {}
        repositories = [('github.com', 'web'), ('bitbucket.org', 'service')]
        for host, name in repositories:
            source = root / f'{name}-source'
            source.mkdir()
            run(['git', 'init', '-q', '-b', 'main'], source)
            (source / 'hello.txt').write_text(f'{name} fixture\n')
            run(['git', 'add', 'hello.txt'], source)
            run(['git', 'commit', '-qm', 'Initial'], source)
            revisions[name] = run(['git', 'rev-parse', 'HEAD'], source).stdout.strip()
            bare = webroot / host / 'example' / f'{name}.git'
            bare.parent.mkdir(parents=True)
            run(['git', 'clone', '--bare', source, bare])
            run(['git', 'update-server-info'], bare)

        cert, key = root / 'cert.pem', root / 'key.pem'
        openssl_config = root / 'openssl.cnf'
        openssl_config.write_text('[req]\ndistinguished_name=dn\nx509_extensions=ext\n'
                                  '[dn]\n[ext]\nsubjectAltName=DNS:bitbucket.org,DNS:github.com\n'
                                  'basicConstraints=critical,CA:TRUE\n')
        run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
             '-subj', '/CN=synthetic-git', '-config', openssl_config, '-keyout', key, '-out', cert])
        tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        tls.load_cert_chain(cert, key)
        requests = []

        class GitHandler(http.server.SimpleHTTPRequestHandler):
            def __init__(self, *args, host, **options):
                self.forge_host = host
                super().__init__(*args, directory=str(webroot / host), **options)

            def log_message(self, *args):
                pass

            def do_GET(self):
                header = self.headers.get('Authorization', '')
                accepted = header == headers[self.forge_host]
                requests.append((self.forge_host, accepted, bool(header)))
                if not accepted:
                    self.send_response(401)
                    self.send_header('WWW-Authenticate', 'Basic realm="synthetic"')
                    self.send_header('Content-Length', '0')
                    self.end_headers()
                    return
                super().do_GET()

        class Proxy(socketserver.StreamRequestHandler):
            def handle(self):
                self.connection.settimeout(10)
                line = self.rfile.readline().decode('ascii')
                host = line.split()[1].removesuffix(':443')
                if not line.startswith('CONNECT ') or host not in headers:
                    self.wfile.write(b'HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n')
                    return
                while self.rfile.readline().strip():
                    pass
                self.wfile.write(b'HTTP/1.1 200 Connection established\r\n\r\n')
                self.wfile.flush()
                with tls.wrap_socket(self.connection, server_side=True) as connection:
                    GitHandler(connection, self.client_address, self.server, host=host)

        class ProxyServer(socketserver.ThreadingTCPServer):
            daemon_threads = True

        slug = 'demo'

        class Ledger(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                if self.path.split('?')[0].endswith('/export'):
                    data = {'project': {'slug': slug, 'name': slug}, 'knitProject': None,
                            'repositories': [
                                {'localId': name, 'name': name, 'defaultBranch': 'main',
                                 'remoteUrl': f'https://{host}/example/{name}.git',
                                 'visibility': 'private', 'metadata': {}}
                                for host, name in repositories],
                            'omittedRepositoryCount': 0, 'bundles': [], 'historyEvents': []}
                else:
                    data = {'views': {}}
                payload = json.dumps({'data': data}).encode()
                self.send_response(200)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        with serving(ProxyServer(('127.0.0.1', 0), Proxy)) as proxy, \
                serving(http.server.ThreadingHTTPServer(('127.0.0.1', 0), Ledger)) as ledger:
            run(['git', 'config', '--global', 'http.proxy',
                 f'http://127.0.0.1:{proxy.server_address[1]}'])
            run(['git', 'config', '--global', 'http.sslCAInfo', cert])
            for host, provider in [('github.com', 'github'), ('bitbucket.org', 'bitbucket')]:
                args = [binary, 'auth', 'add', host, '--provider', provider, '--token-stdin']
                if provider == 'bitbucket':
                    args += ['--token-type', kind]
                    if kind == 'access_token':
                        args += ['--username', 'stale@example.invalid']
                run(args, stdin=tokens[host] + '\n')
            auth_before = (home / 'forge-auth.json').read_bytes()
            secrets_before = (home / 'forge-secrets.json').read_bytes()
            for slug in ['demo', 'renamed-demo']:
                requests.clear()
                workspace = root / slug
                result = run([binary, 'clone', f'example/{slug}', workspace,
                              '--remote', 'hosted', '--url', f'http://127.0.0.1:{ledger.server_port}',
                              '--token', 'synthetic-ledger', '--no-worktree'], check=False)
                output = result.stdout + result.stderr
                if expect_rejection:
                    assert not (workspace / 'service' / 'hello.txt').exists()
                    assert any(h == 'bitbucket.org' and not ok and supplied
                               for h, ok, supplied in requests), 'must reject actual wrong credentials'
                    assert (workspace / 'web' / 'hello.txt').exists()
                    assert 'Authentication failed' in output
                    continue
                assert result.returncode == 0, 'full clone failed'
                assert 'Imported: 2 repo(s)' in output
                assert 'skipped; local credentials cover every forge repository' in output
                assert 'Replacement token' not in output
                assert all(ok for _, ok, _ in requests), 'first clone request must authenticate'
                assert not marker.exists(), 'native helper/SSH fallback must never be invoked'
                for host, name in repositories:
                    checkout = workspace / name
                    assert (checkout / 'hello.txt').read_text() == f'{name} fixture\n'
                    assert run(['git', 'rev-parse', 'HEAD'], checkout).stdout.strip() == revisions[name]
                    assert any(h == host and ok for h, ok, _ in requests)
                run([binary, 'auth', 'status'], workspace)
                for _, name in repositories:
                    run(['git', 'fetch', 'origin'], workspace / name)
                assert not marker.exists(), 'plain Git must use the installed Knit helper'
                assert (home / 'forge-auth.json').read_bytes() == auth_before
                assert (home / 'forge-secrets.json').read_bytes() == secrets_before
                for secret in tokens.values():
                    assert secret not in output
            print(f'{kind}: {"rejection reproduced" if expect_rejection else "full clones and fetch passed without fallback"}')


if __name__ == '__main__':
    for token_kind in ['access_token', 'atlassian_api_token']:
        scenario(Path(sys.argv[1]).resolve(), token_kind, '--expect-rejection' in sys.argv[2:])
