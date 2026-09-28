"""Real terminal fixture for remote token entry: hidden prompts that submit on
Enter, replacement without re-adding, cancellation (empty Enter, Ctrl-C, empty
Ctrl-D) that never clobbers the stored token, and remote-existence resolved
before any prompt."""
import json
import signal
import sys
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

def stored_token():
    return json.loads(config_path.read_text())['remotes']['hosted']['token']

# 1. `remote add --token-stdin` at a terminal: hidden prompt, Enter completes
#    (no Ctrl-D), guidance mentions the replacement and auth-status commands.
def add_script(expect, answer):
    answer('Token for remote `hosted` (input hidden, press Enter to submit): ',
           'PTY-ADD-SECRET', hidden=True)
    expect('configured global hosted')
    expect('Authentication has not been checked.')
    expect('knit remote auth-status hosted')
    expect('knit remote token hosted --global')

transcript = conversation(binary, outside, env,
                          ['remote', 'add', 'hosted', 'https://host.example',
                           '--global', '--token-stdin'], add_script)
assert stored_token() == 'PTY-ADD-SECRET'
assert b'PTY-ADD-SECRET' not in transcript, 'token echoed to terminal'

# 2. `remote token NAME --global` replacement: omitted token at a terminal is a
#    hidden prompt, Enter completes, the new token replaces the old one.
def replace_script(expect, answer):
    answer('Token for remote `hosted` (input hidden, press Enter to submit): ',
           'PTY-REPLACE-SECRET', hidden=True)
    expect('stored hosted')
    expect('knit remote auth-status hosted')

transcript = conversation(binary, outside, env,
                          ['remote', 'token', 'hosted', '--global'], replace_script)
assert stored_token() == 'PTY-REPLACE-SECRET'
assert b'PTY-REPLACE-SECRET' not in transcript, 'token echoed to terminal'

# 3. Empty Enter at the prompt fails without overwriting the stored token.
def empty_script(expect, answer):
    answer('Token for remote `hosted` (input hidden, press Enter to submit): ',
           '', hidden=True)
    expect('remote token is empty; nothing was saved')

conversation(binary, outside, env,
             ['remote', 'token', 'hosted', '--global'], empty_script, exit_code=1)
assert stored_token() == 'PTY-REPLACE-SECRET', 'empty submission clobbered the token'

# 4. Ctrl-C at the prompt cancels without touching the stored token.
def sigint_script(expect, answer):
    answer('Token for remote `hosted` (input hidden, press Enter to submit): ',
           '\x03', hidden=True)

conversation(binary, outside, env,
             ['remote', 'token', 'hosted', '--global'], sigint_script,
             exit_code=-signal.SIGINT)
assert stored_token() == 'PTY-REPLACE-SECRET', 'Ctrl-C clobbered the token'

# 5. An empty Ctrl-D (EOF) at the prompt errors without overwriting.
def eof_script(expect, answer):
    answer('Token for remote `hosted` (input hidden, press Enter to submit): ',
           '\x04', hidden=True)
    expect('could not read the token from the hidden prompt')

conversation(binary, outside, env,
             ['remote', 'token', 'hosted', '--global'], eof_script, exit_code=1)
assert stored_token() == 'PTY-REPLACE-SECRET', 'Ctrl-D clobbered the token'

# 6. An unknown remote errors before any prompt appears.
def missing_script(expect, answer):
    expect('No remote named `missing`')

conversation(binary, outside, env,
             ['remote', 'token', 'missing', '--global'], missing_script, exit_code=1)

print('PTY remote token: hidden Enter prompts, replacement, cancellation, resolve-first: PASS')
