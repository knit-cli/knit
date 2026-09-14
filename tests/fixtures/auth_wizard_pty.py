"""Real terminal fixture: synthetic personal store, no provider or Git network calls."""
import errno
import json
import os
from pathlib import Path
import pty
import select
import signal
import sys
import termios
import time

binary, directory = sys.argv[1:]
root = Path(directory).resolve()
(root / '.knit/projects').mkdir(parents=True)
home = root / 'home'
home.mkdir()
(root / '.knit/config.json').write_text(json.dumps({'schemaVersion': '1', 'activeProject': 'tools'}))
project = {'schemaVersion': '1', 'kind': 'KnitProject', 'id': 'tools', 'createdAt': '', 'updatedAt': '', 'repos': [
    {'id': name, 'path': name, 'remote': f'https://{host}/org/{name}.git', 'baseBranch': 'main'}
    for name, host in [('api', 'github.com'), ('web', 'github.com'), ('bb', 'bitbucket.org')]]
}
project_path = root / '.knit/projects/tools.project.json'
project_path.write_text(json.dumps(project))
original = project_path.read_bytes()
env = {'PATH': '/usr/bin:/bin', 'HOME': str(home), 'KNIT_HOME': str(home),
       'GIT_CONFIG_GLOBAL': str(root / 'empty.gitconfig'), 'GIT_CONFIG_NOSYSTEM': '1', 'TERM': 'dumb'}
pid, fd = pty.fork()
if pid == 0:
    os.chdir(root)
    os.execve(binary, [binary, 'auth', 'setup', '--repo', 'api', '--repo', 'web'], env)
transcript = b''
unread = b''
reaped = False

def expect(needle):
    global transcript, unread
    wanted = needle.encode()
    deadline = time.monotonic() + 15
    while wanted not in unread:
        assert time.monotonic() < deadline, f'timeout waiting for {needle!r}: {transcript.decode(errors="replace")}'
        ready, _, _ = select.select([fd], [], [], 0.1)
        if ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError as e:
                if e.errno == errno.EIO:
                    chunk = b''
                else:
                    raise
            assert chunk, f'PTY closed while waiting for {needle!r}: {transcript.decode(errors="replace")}'
            transcript += chunk
            unread += chunk
    unread = unread.split(wanted, 1)[1]

def answer(prompt, text):
    expect(prompt)
    if prompt == 'Token (hidden): ':
        # rpassword prints its prompt before disabling echo. Synchronize on
        # the terminal state, as a human typing after seeing the prompt would.
        deadline = time.monotonic() + 5
        while termios.tcgetattr(fd)[3] & termios.ECHO:
            assert time.monotonic() < deadline, 'password prompt never disabled echo'
            time.sleep(0.005)
    os.write(fd, (text + '\n').encode())

try:
    answer("'done' to finish: ", 'new')
    answer('Host (name or number): ', '999')
    expect("Choose one of the project's hosts.")
    assert not (home / 'forge-auth.json').exists()
    answer("'done' to finish: ", 'new')
    answer('Host (name or number): ', 'github.com')
    answer('Name for this credential: ', 'local')
    answer('private local file: ', '')
    answer('Token (hidden): ', 'PTY-SYNTHETIC-SECRET')
    answer('Existing links selected will be replaced: ', 'api,bb')
    expect('Unknown or ineligible repository')
    registry_path = home / 'forge-auth.json'
    assert json.loads(registry_path.read_text()).get('projects', {}) == {}
    answer('Existing links selected will be replaced: ', '0')
    expect('Unknown or ineligible repository')
    assert json.loads(registry_path.read_text()).get('projects', {}) == {}
    answer('Existing links selected will be replaced: ', '1, web,1')
    expect('Missing links: bb')
    state = registry_path.read_bytes()
    answer("'done' to finish: ", 'local')
    answer('Existing links selected will be replaced: ', '')
    answer("'done' to finish: ", 'done')
    expect('Setup incomplete: missing links for bb. Saved links have been kept.')
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        waited, status = os.waitpid(pid, os.WNOHANG)
        if waited:
            reaped = True
            assert os.waitstatus_to_exitcode(status) == 0
            break
        time.sleep(0.02)
    assert reaped, 'wizard did not exit after done'
    assert registry_path.read_bytes() == state, 'cancel modified existing links'
    registry = json.loads(state)
    assert registry['projects'][str(project_path.resolve())] == {'api': 'local', 'web': 'local'}
    assert json.loads((home / 'forge-secrets.json').read_text()) == {'local': 'PTY-SYNTHETIC-SECRET'}
    assert b'PTY-SYNTHETIC-SECRET' not in transcript, 'token echoed to the terminal'
    assert b'PTY-SYNTHETIC-SECRET' not in state
    assert project_path.read_bytes() == original
    assert (home / 'forge-secrets.json').stat().st_mode & 0o777 == 0o600
    assert home.stat().st_mode & 0o777 == 0o700
    print('PTY wizard: hidden input, invalid host, incompatible repo, invalid number, deduplication, scope, cancel, incomplete done, storage modes: PASS')
finally:
    if not reaped:
        os.kill(pid, signal.SIGKILL)
        os.waitpid(pid, 0)
    os.close(fd)
