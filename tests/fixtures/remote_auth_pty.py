"""Real terminal fixture for `knit auth remote` and the bare-auth `r` entry:
discovery from the forge menu, hidden Enter prompts, wrong-token retry, new
remote URL prompting, cancellation that preserves stored state, and the
`auth add --token-stdin` terminal regression — all against a real loopback
service (stdlib http.server), synthetic tokens only."""
import http.server
import json
import signal
import subprocess
import sys
import threading
from pathlib import Path

binary, directory = sys.argv[1:3]
root = Path(directory).resolve()
home = root / 'home'
outside = root / 'outside'
outside.mkdir(parents=True)
home.mkdir()
env = {'PATH': '/usr/bin:/bin', 'HOME': str(home), 'KNIT_HOME': str(home),
       'GIT_CONFIG_GLOBAL': str(root / 'empty.gitconfig'), 'GIT_CONFIG_NOSYSTEM': '1',
       'TERM': 'dumb'}
from pty_session import conversation

config_path = home / 'config.json'

# ---------------------------------------------------------------------------
# Loopback sync service: the first /me/access-token request is rejected with
# 401, every later one is accepted with a proper envelope.
# ---------------------------------------------------------------------------

state = {'access_requests': 0}
state_lock = threading.Lock()

class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path.startswith('/api/v1/me/access-token'):
            with state_lock:
                state['access_requests'] += 1
                rejected = state['access_requests'] == 1
            if rejected:
                body = json.dumps({'errors': {'detail': 'invalid token'}}).encode()
                self.send_response(401)
            else:
                body = json.dumps({'data': {'tokenKind': 'legacy',
                                            'subjectUserId': 'user-1',
                                            'scopes': ['bundle:read']}}).encode()
                self.send_response(200)
        else:
            body = json.dumps({'errors': {'detail': 'not found'}}).encode()
            self.send_response(404)
        self.send_header('content-type', 'application/json')
        self.send_header('content-length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)

server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
base = f'http://127.0.0.1:{server.server_address[1]}'
threading.Thread(target=server.serve_forever, daemon=True).start()

def stored(remote):
    config = json.loads(config_path.read_text())
    entry = config['remotes'].get(remote)
    return (entry or {}).get('token'), (entry or {}).get('url')

def setup(args, token):
    subprocess.run([binary] + args + ['--global', '--token', token],
                   cwd=outside, env=env, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

# Seed one existing remote so discovery, URL reuse, and preservation have
# real state to work against.
setup(['remote', 'add', 'hosted', base], 'old-synthetic-token')

token_prompt = f'Token for remote `hosted` at {base} (input hidden, press Enter to submit): '

# 1. Bare `knit auth` offers `r`; it enters the same remote flow: wrong token
#    rejected with a retry prompt, right token verified and saved, wizard
#    continues and finishes normally.
def wizard_script(expect, answer):
    expect('Personal forge tokens')
    expect('r. Sync remote token (hosted Knit service)')
    answer('Choice (1-4 for Git hosting, r for sync, Enter to finish): ', 'r')
    expect('Configured sync remotes (user-level):')
    expect('hosted')
    answer('Remote name to set up or update (an existing name, or a new one): ', 'hosted')
    answer(token_prompt, 'PTY-WRONG-TOKEN', hidden=True)
    expect('Verifying token with')
    expect('rejected this token (HTTP 401)')
    expect('nothing was saved')
    answer(token_prompt, 'PTY-GOOD-TOKEN', hidden=True)
    expect('updated hosted')
    expect(f'token verified with {base}')
    expect('Token saved to:')
    expect('knit remote auth-status hosted')
    expect('knit auth remote hosted')
    answer('Choice (1-4 for Git hosting, r for sync, Enter to finish): ', '')
    expect('Done.')

transcript = conversation(binary, outside, env, ['auth'], wizard_script)
assert stored('hosted') == ('PTY-GOOD-TOKEN', base), stored('hosted')
for secret in [b'PTY-WRONG-TOKEN', b'PTY-GOOD-TOKEN']:
    assert secret not in transcript, 'token echoed to terminal'

# 2. Empty submission at the prompt fails without overwriting the token.
def empty_script(expect, answer):
    answer(token_prompt, '', hidden=True)
    expect('remote token is empty; nothing was saved')

conversation(binary, outside, env, ['auth', 'remote', 'hosted'], empty_script,
             exit_code=1)
assert stored('hosted') == ('PTY-GOOD-TOKEN', base), 'empty submission clobbered the token'

# 3. Ctrl-C at the prompt cancels without touching stored state.
def sigint_script(expect, answer):
    answer(token_prompt, '\x03', hidden=True)

conversation(binary, outside, env, ['auth', 'remote', 'hosted'], sigint_script,
             exit_code=-signal.SIGINT)
assert stored('hosted') == ('PTY-GOOD-TOKEN', base), 'Ctrl-C clobbered the token'

# 4. A new remote prompts for its URL, rejects an invalid one, and completes
#    once a valid endpoint and token are given.
def new_remote_script(expect, answer):
    answer('Remote name to set up or update (an existing name, or a new one): ', 'fresh')
    answer('Service URL for `fresh` (for example https://host.example): ', 'notaurl')
    expect('Service URL must be a valid HTTP(S) URL')
    answer('Service URL for `fresh` (for example https://host.example): ', base)
    answer(f'Token for remote `fresh` at {base} (input hidden, press Enter to submit): ',
           'PTY-FRESH-TOKEN', hidden=True)
    expect('configured fresh')
    expect('knit remote auth-status fresh')

transcript = conversation(binary, outside, env, ['auth', 'remote'], new_remote_script)
assert stored('fresh') == ('PTY-FRESH-TOKEN', base), stored('fresh')
assert b'PTY-FRESH-TOKEN' not in transcript, 'token echoed to terminal'

# 5. `auth add --token-stdin` at a terminal is a hidden Enter prompt, not a
#    read-to-EOF waiting on Ctrl-D (the regression this fixture guards).
def add_stdin_script(expect, answer):
    answer('Token for `classic` (input hidden, press Enter to submit): ',
           'PTY-ADD-SECRET', hidden=True)
    expect('Saved `classic`')

conversation(binary, outside, env,
             ['auth', 'add', 'classic', '--provider', 'github', '--token-stdin'],
             add_stdin_script)
secrets = json.loads((home / 'forge-secrets.json').read_text())
assert secrets['classic'] == 'PTY-ADD-SECRET'

print('PTY remote auth: r discovery, verified retry, cancel preserves, new remote, add stdin: PASS')
