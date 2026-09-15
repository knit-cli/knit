"""Real terminal fixture for grouped auth setup: two forges, hidden tokens, no provider or Git network calls."""
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
pid, fd = pty.fork()
if pid == 0:
    os.chdir(root)
    os.execve(binary, [binary, 'auth', 'setup'], env)
transcript = b''
unread = b''
reaped = False

# Hard ceiling for the whole conversation: even a pathological child can
# only waste this much time before the fixture fails instead of hanging.
DEADLINE = time.monotonic() + 120

def watchdog():
    while time.monotonic() < DEADLINE:
        time.sleep(0.5)
    print(f'FIXTURE TIMEOUT after 120s: {transcript.decode(errors="replace")!r}',
          file=sys.stderr, flush=True)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    # A child stuck in an exiting state can leave the final waitpid below
    # blocked forever; hard-exit so the parent's Command::output unblocks.
    os._exit(1)

watchdog_thread = __import__('threading').Thread(target=watchdog, daemon=True)
watchdog_thread.start()

def read_chunk(timeout=0.1):
    """One bounded master read: None when nothing is ready, b'' on EOF/EIO."""
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
    global transcript, unread
    wanted = needle.encode()
    # Fixed per-prompt deadline: computing it inside the loop would reset it
    # on every iteration and let a dripping child stall forever.
    prompt_deadline = min(DEADLINE, time.monotonic() + 15)
    while wanted not in unread:
        assert time.monotonic() < prompt_deadline, (
            f'timeout waiting for {needle!r}: {transcript.decode(errors="replace")}')
        chunk = read_chunk()
        if chunk is None:
            continue
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
    # The group requirements are displayed before any prompt.
    expect('Group gh-work: GitHub work token (github @ github.com)')
    expect('Token type(s): fine_grained_pat')
    expect('Permissions: contents:read, pull_requests:write')
    expect('Create a token at: https://github.com/settings/personal-access-tokens/new')
    expect('Instructions: Create the token in the org, scoped to both repos.')
    # GitHub credential: create it, token hidden.
    answer("Credential for this group (name/number), 'new', or Enter to skip it for now: ", 'new')
    answer('Name for this credential: ', 'gh')
    answer('private local file: ', '')
    answer('Token (hidden): ', 'PTY-GH-SECRET')
    expect('No repository links needed: `gh` is the default credential for github.com.')
    # Bitbucket credential: pick the API-token kind explicitly, then email.
    expect('Group bb-cloud: Bitbucket Cloud (bitbucket @ bitbucket.org)')
    expect('Token type(s): atlassian_api_token, access_token')
    expect('Create a token at: https://id.atlassian.com/manage-profile/security/api-tokens')
    answer("Credential for this group (name/number), 'new', or Enter to skip it for now: ", 'new')
    answer('Token type (name or number): ', 'atlassian_api_token')
    answer('Name for this credential: ', 'bb')
    answer('Atlassian account email for this API token: ', 'dev@example.org')
    answer('private local file: ', '')
    answer('Token (hidden): ', 'PTY-BB-SECRET')
    expect('No repository links needed: `bb` is the default credential for bitbucket.org.')
    # The group whose repositories were never cloned is skipped, not prompted.
    expect('Not in this workspace (out of clone scope or not cloned yet): extra')
    expect("Skipping group `absent`: none of its repositories are in this workspace.")
    expect('All forge repositories have credential links; permissions remain unchecked.')
    answer('Check Git read access now? [y/N]: ', 'n')
    # The wizard's tail (final summary and exit) completes only once the
    # master is drained: a child blocked writing its tail never exits, so
    # keep reading while polling for the exit. EOF/EIO means the tail is
    # fully consumed and only the exit remains. Tail bytes join the
    # transcript, so the token-echo assertions cover the whole conversation.
    exit_deadline = min(DEADLINE, time.monotonic() + 10)
    eof = False
    while not reaped:
        waited, status = os.waitpid(pid, os.WNOHANG)
        if waited:
            reaped = True
            assert os.waitstatus_to_exitcode(status) == 0
            break
        assert time.monotonic() < exit_deadline, (
            'grouped wizard did not exit: '
            f'{transcript.decode(errors="replace")}')
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
    registry_path = home / 'forge-auth.json'
    registry = json.loads(registry_path.read_text())
    resolved = str(project_path.resolve())
    assert not registry.get('projects', {}).get(resolved)
    assert registry['defaults'] == {'github.com': 'gh', 'bitbucket.org': 'bb'}
    assert registry['credentials']['gh']['tokenType'] == 'fine_grained_pat'
    assert registry['credentials']['bb']['tokenType'] == 'atlassian_api_token'
    assert registry['credentials']['bb']['username'] == 'dev@example.org'
    secrets = json.loads((home / 'forge-secrets.json').read_text())
    assert secrets == {'gh': 'PTY-GH-SECRET', 'bb': 'PTY-BB-SECRET'}
    assert b'PTY-GH-SECRET' not in transcript, 'github token echoed to the terminal'
    assert b'PTY-BB-SECRET' not in transcript, 'bitbucket token echoed to the terminal'
    assert b'PTY-GH-SECRET' not in registry_path.read_bytes()
    assert project_path.read_bytes() == original, 'setup must not rewrite the project artifact'
    assert (home / 'forge-secrets.json').stat().st_mode & 0o777 == 0o600
    print('PTY grouped wizard: two forges, token kinds, hidden tokens, email, absent-group skip: PASS')
finally:
    if not reaped:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        # Close the master before the blocking reap: a child draining the
        # PTY on its exit path must not be able to hold the reap forever.
        os.close(fd)
        os.waitpid(pid, 0)
    else:
        os.close(fd)
