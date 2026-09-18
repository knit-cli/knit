#!/bin/bash
# Synthetic git for the Bitbucket host-default fallback regressions.
#
# Rejects every attempt that carries the saved Knit credential (the proactive
# Basic header or the hidden helper's vendored password) and accepts only the
# ambient paths ordinary Git would use: a native credential helper answering
# `git credential fill`, or the same repository over SSH. Everything else —
# no credential over HTTPS, refused SSH, unknown URLs — fails the way real
# Git does. Each network invocation is captured under <root>/git-call-N/
# with its args, environment, helper output, and the decision markers the
# Rust side asserts on.
set -e
real_git=__REAL_GIT__
root=__ROOT__
saved_header=__SAVED_HEADER__
saved_secret=__SAVED_SECRET__
ambient_secret=__AMBIENT_SECRET__

op=
url=
prefix=()
for arg in "$@"; do
  if [ -z "$op" ]; then
    case "$arg" in
      clone|ls-remote|fetch) op="$arg"; continue ;;
    esac
    prefix+=("$arg")
  elif [ -z "$url" ]; then
    case "$arg" in
      -*) ;;
      *) url="$arg" ;;
    esac
  fi
done
if [ -z "$op" ]; then
  exec "$real_git" "$@"
fi
case "$url" in
  https://*|ssh://*|git@*) ;;
  *) url=$("$real_git" "${prefix[@]}" remote get-url "${url:-origin}" 2>/dev/null || printf '') ;;
esac
# Resolve insteadOf exactly as the actual Git transport does.
url=$("$real_git" "${prefix[@]}" ls-remote --get-url "$url")

n=$(cat "$root/git-seq" 2>/dev/null || printf 0)
n=$((n + 1))
printf '%s\n' "$n" > "$root/git-seq"
d="$root/git-call-$n"
mkdir -p "$d"
printf '%s\n' "$@" > "$d/args"
printf '%s' "${KNIT_GIT_AUTH_HEADER_0-UNSET}" > "$d/header0"
printf 'GIT_SSH_COMMAND=%s\n' "${GIT_SSH_COMMAND-}" > "$d/envdump"
printf '%s' "$url" > "$d/url"

helper=
scope=
for arg in "$@"; do
  case "$arg" in
    credential.https://*/*.helper=!*) helper=${arg#*=!}; scope=${arg%%=*} ;;
  esac
done
if [ -n "$helper" ]; then
  t=${scope#credential.https://}
  t=${t%.helper}
  h=${t%%/*}
  p=${t#*/}
  printf 'protocol=https\nhost=%s\npath=%s\n\n' "$h" "$p" | /bin/sh -c "$helper get" > "$d/helper-out" 2> "$d/helper-err" || true
else
  : > "$d/helper-undef"
fi

mode=
src=
case "$url" in
__CASES__
  *) mode=unknown ;;
esac
printf '%s' "${helper}" > "$d/helper"
printf '%s' "${mode:-unknown}" > "$d/mode"

reject_saved() {
  : > "$d/rejected-saved"
  printf "fatal: Authentication failed for '%s/'\n" "$url" >&2
  exit 128
}
refuse_no_credential() {
  : > "$d/no-credential"
  printf "fatal: could not read Username for '%s': terminal prompts disabled\n" "$url" >&2
  exit 128
}

# Resolve host/path for the credential fill emulation.
h=
p=
case "$url" in
  https://*)
    rest=${url#https://}
    h=${rest%%/*}
    p=${rest#*/}
    p=${p%.git}
    ;;
  ssh://*)
    rest=${url#ssh://}
    rest=${rest#git@}
    h=${rest%%/*}
    p=${rest#*/}
    p=${p%.git}
    ;;
  git@*)
    rest=${url#git@}
    h=${rest%%:*}
    p=${rest#*:}
    p=${p%.git}
    ;;
esac

case "$mode" in
  unknown)
    printf 'synthetic git: unconfigured target\n' >&2
    exit 128
    ;;
  public)
    if [ -n "$helper" ] || [ "${KNIT_GIT_AUTH_HEADER_0-UNSET}" != "UNSET" ]; then
      : > "$d/unexpected-helper"
    fi
    ;;
  ssh-ok)
    : > "$d/ssh-ok"
    ;;
  ssh-refused)
    : > "$d/ssh-refused"
    printf 'ssh: connect to host bitbucket.org port 22: Connection refused\n' >&2
    printf "fatal: Could not read from remote repository.\n" >&2
    exit 128
    ;;
  reject-saved)
    # Any attempt that carries the saved Knit credential is rejected, exactly
    # like a forge answering 401 to that token.
    if [ "${KNIT_GIT_AUTH_HEADER_0-UNSET}" = "$saved_header" ]; then
      reject_saved
    fi
    if [ -f "$d/helper-out" ] && grep -q "^password=$saved_secret\$" "$d/helper-out"; then
      reject_saved
    fi
    # Ordinary Git: ask the configured helpers (global config now, repo
    # config — including any Knit-generated include — for in-repo fetch)
    # exactly like a real transport would.
    fill=$(printf 'protocol=https\nhost=%s\npath=%s\n\n' "$h" "$p" \
      | "$real_git" "${prefix[@]}" credential fill 2> "$d/fill-err") || fill=
    printf '%s\n' "$fill" > "$d/fill-out"
    case "$fill" in
      *"password=$ambient_secret"*)
        : > "$d/ambient-ok"
        ;;
      *"password=$saved_secret"*|*"password="*)
        reject_saved
        ;;
      *)
        refuse_no_credential
        ;;
    esac
    ;;
esac

if [ "$op" = ls-remote ] || [ "$op" = fetch ]; then
  exit 0
fi
if [ "$mode" = unknown ]; then
  printf 'fake git: unexpected network url %s\n' "$url" >&2
  exit 1
fi
target=
for arg in "$@"; do target="$arg"; done
"$real_git" clone -q "$src" "$target" || exit $?
exec "$real_git" -C "$target" remote set-url origin "$url"
