#!/bin/sh
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
n=$(cat "$root/git-seq" 2>/dev/null || echo 0)
n=$((n + 1))
printf '%s\n' "$n" > "$root/git-seq"
d="$root/git-call-$n"
mkdir -p "$d"
printf '%s\n' "$@" > "$d/args"
printf '%s' "${KNIT_GIT_AUTH_HEADER_0-UNSET}" > "$d/header0"
printf '%s|%s|%s' "${GIT_TERMINAL_PROMPT-UNSET}" "${GIT_CURL_VERBOSE-UNSET}" "${GIT_TRACE_REDACT-UNSET}" > "$d/env"
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
printf '%s' "$helper" > "$d/helper"
printf '%s' "${mode:-unknown}" > "$d/mode"
if [ "$op" = ls-remote ]; then
  if [ "$mode" = auth ] && [ -z "$helper" ]; then
    printf 'fatal: could not read Username: terminal prompts disabled\n' >&2
    exit 128
  fi
  if [ -f "$root/forge-rejects" ] && [ "$mode" = auth ]; then
    printf 'fatal: Authentication failed\n' >&2
    exit 128
  fi
  exit 0
fi
if [ "$mode" = unknown ]; then
  printf 'fake git: unexpected network url %s\n' "$url" >&2
  exit 1
fi
if [ "$mode" = auth ] && [ -z "$helper" ]; then
  : > "$d/no-credential"
  printf "fatal: could not read Username for '%s': terminal prompts disabled\n" "$url" >&2
  exit 128
fi
if [ -f "$root/forge-rejects" ] && [ "$mode" = auth ]; then
  : > "$d/rejected"
  printf "fatal: Authentication failed for '%s/'\n" "$url" >&2
  cat "$d/helper-out" >&2
  printf '%s\n' "${KNIT_GIT_AUTH_HEADER_0-}" >&2
  exit 128
fi
if [ "$mode" = public ] && [ -n "$helper" ]; then
  : > "$d/unexpected-helper"
fi
target=
for arg in "$@"; do target="$arg"; done
"$real_git" clone -q "$src" "$target" || exit $?
exec "$real_git" -C "$target" remote set-url origin "$url"
