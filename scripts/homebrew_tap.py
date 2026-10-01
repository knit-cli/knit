#!/usr/bin/env python3
"""Read-only completion gate: the default-branch tap must match exact bytes."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import time


class TapReadError(Exception):
    pass


def read_formula(repo, timeout=20):
    """No ref means GitHub resolves the repository's current default branch."""
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        raise ValueError("expected owner/repository")
    try:
        result = subprocess.run(
            ["gh", "api", "--method", "GET", "-H", "Cache-Control: no-cache",
             f"repos/{repo}/contents/Formula/knit.rb"],
            capture_output=True, timeout=timeout, check=False,
        )
        if result.returncode:
            # Never echo gh stderr: it can include authentication diagnostics.
            raise TapReadError(f"GitHub formula read failed (gh exit {result.returncode})")
        data = json.loads(result.stdout)
        if data.get("encoding") != "base64" or data.get("type") != "file":
            raise ValueError("expected a base64 file response")
        return base64.b64decode("".join(data["content"].split()), validate=True)
    except (subprocess.TimeoutExpired, OSError, ValueError, KeyError) as exc:
        raise TapReadError(f"GitHub formula read failed ({type(exc).__name__})") from exc


def wait_for_formula(expected, fetch, timeout=1800, interval=20,
                     clock=time.monotonic, sleep=time.sleep, report=print):
    if timeout <= 0 or interval <= 0:
        raise ValueError("timeout and interval must be positive")
    deadline = clock() + timeout
    wanted = hashlib.sha256(expected).hexdigest()
    last = "not checked"
    while clock() < deadline:
        try:
            actual = fetch(min(20, deadline - clock()))
            if actual == expected:
                report(f"Live tap formula matches verified bytes (sha256 {wanted}).")
                return
            last = f"formula differs: sha256 {hashlib.sha256(actual).hexdigest()}, expected {wanted}"
        except TapReadError as exc:
            last = str(exc)
        remaining = deadline - clock()
        report(f"Tap publication pending: {last}; {max(0, int(remaining))}s remaining.")
        if remaining > 0:
            sleep(min(interval, remaining))
    raise TimeoutError(f"Tap publication NOT complete after {timeout}s: {last}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("check", "wait", "preflight"))
    parser.add_argument("--repo", required=True)
    parser.add_argument("--formula", type=Path, required=True)
    parser.add_argument("--mode", choices=("manual", "automatic"), default="automatic")
    parser.add_argument("--timeout", type=float, default=1800)
    parser.add_argument("--interval", type=float, default=20)
    args = parser.parse_args()
    expected = args.formula.read_bytes()
    if args.command == "wait":
        wait_for_formula(expected, lambda timeout: read_formula(args.repo, timeout),
                         args.timeout, args.interval)
        return 0
    if args.command == "preflight" and (args.mode == "manual" or os.environ.get("TAP_TOKEN")):
        return 0
    # An already completed tap needs no publishing credential, including recovery.
    if read_formula(args.repo) == expected:
        print("Live tap already matches verified formula.")
        return 0
    if args.command == "preflight":
        raise ValueError("Automatic tap publication requires HOMEBREW_TAP_TOKEN; "
                         "live tap differs. Select manual mode explicitly for a manual publisher.")
    return 1


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (TapReadError, TimeoutError, ValueError, OSError) as error:
        print(f"::error::{error}", file=sys.stderr)
        sys.exit(2)
