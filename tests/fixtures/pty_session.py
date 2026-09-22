"""Bounded real-terminal conversations shared by auth regressions."""
import errno
import os
import pty
import select
import signal
import sys
import termios
import threading
import time

def conversation(binary, cwd, env, args, script, exit_code=0):
    DEADLINE = time.monotonic() + 120
    finished = threading.Event()
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execve(binary, [binary] + args, env)
    transcript = b''
    unread = b''
    reaped = False

    def watchdog():
        if finished.wait(120):
            return
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
                assert os.waitstatus_to_exitcode(status) == exit_code, transcript.decode(errors='replace')
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
        finished.set()
        if not reaped:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.close(fd)
            os.waitpid(pid, 0)
        else:
            os.close(fd)

