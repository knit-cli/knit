#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: scripts/forge-smoke.sh <github|gitlab|codeberg|bitbucket> [--keep] [--dry-run]" >&2
  exit 2
}

[[ $# -ge 1 ]] || usage
forge=$1
shift
keep=0
dry_run=0
for arg in "$@"; do
  case "$arg" in
    --keep) keep=1 ;;
    --dry-run) dry_run=1 ;;
    *) usage ;;
  esac
done
case "$forge" in
  github|gitlab|codeberg|bitbucket) ;;
  *) usage ;;
esac

if [[ $dry_run -eq 1 ]]; then
  cat <<EOF
forge-smoke dry run: $forge
1. validate local tools and forge credentials
2. create two explicitly private repositories and verify visibility
3. clone and initialize main branches in an isolated temporary workspace
4. run knit bundle -> commit -> push -> publish create -> publish status --live
5. run knit land -> knit land apply
6. verify both reviews are merged and main advanced
7. delete both temporary repositories (unless --keep)
EOF
  exit 0
fi

missing=()
bitbucket_username=
need_command() {
  command -v "$1" >/dev/null 2>&1 || missing+=("command:$1")
}
need_env() {
  [[ -n ${!1:-} ]] || missing+=("env:$1")
}

need_command git
need_command jq
need_command curl
need_command "${KNIT_BIN:-knit}"
case "$forge" in
  github)
    need_command gh
    ;;
  gitlab)
    need_command glab
    [[ -n ${KNIT_GITLAB_TOKEN:-${GITLAB_TOKEN:-}} ]] ||
      missing+=("env:KNIT_GITLAB_TOKEN or GITLAB_TOKEN")
    ;;
  codeberg)
    need_env KNIT_FORGEJO_TOKEN
    ;;
  bitbucket)
    need_env KNIT_BITBUCKET_EMAIL
    need_env KNIT_BITBUCKET_API_TOKEN
    need_env BITBUCKET_SMOKE_WORKSPACE
    ;;
esac
if [[ ${#missing[@]} -gt 0 ]]; then
  printf 'forge-smoke preflight missing for %s:\n' "$forge" >&2
  printf '  %s\n' "${missing[@]}" >&2
  exit 2
fi
case "$forge" in
  github) gh auth status >/dev/null ;;
  gitlab) glab auth status --hostname gitlab.com >/dev/null ;;
  codeberg)
    curl --fail --silent --show-error \
      -H "Authorization: token ${KNIT_FORGEJO_TOKEN}" \
      https://codeberg.org/api/v1/user >/dev/null
    ;;
  bitbucket)
    bitbucket_user=$(curl --fail --silent --show-error \
      --user "${KNIT_BITBUCKET_EMAIL}:${KNIT_BITBUCKET_API_TOKEN}" \
      https://api.bitbucket.org/2.0/user)
    bitbucket_username=$(jq -r '.username // empty' <<<"$bitbucket_user")
    [[ -n $bitbucket_username ]] || {
      echo "forge-smoke could not resolve the Bitbucket username" >&2
      exit 1
    }
    ;;
esac

root=$(mktemp -d)
workspace=$root/workspace
knit_home=$root/knit-home
git_config=$root/gitconfig
mkdir -p "$workspace" "$knit_home"
touch "$git_config"
export KNIT_HOME=$knit_home
export GIT_CONFIG_GLOBAL=$git_config
knit_bin=${KNIT_BIN:-knit}
timestamp=$(date -u +%Y%m%d%H%M%S)-$$
backend_name=knit-smoke-backend-$timestamp
frontend_name=knit-smoke-frontend-$timestamp
backend_full=
frontend_full=
backend_url=
frontend_url=

askpass=$root/git-askpass.sh
printf '%s\n' '#!/bin/sh' \
  'case "$1" in' \
  '  *Username*) printf "%s" "$FORGE_GIT_USER" ;;' \
  '  *) printf "%s" "$FORGE_GIT_TOKEN" ;;' \
  'esac' >"$askpass"
chmod 700 "$askpass"
export GIT_ASKPASS=$askpass
export GIT_TERMINAL_PROMPT=0
case "$forge" in
  github)
    export FORGE_GIT_USER=x-access-token
    FORGE_GIT_TOKEN=$(gh auth token)
    export FORGE_GIT_TOKEN
    ;;
  gitlab)
    export FORGE_GIT_USER=oauth2
    FORGE_GIT_TOKEN=${KNIT_GITLAB_TOKEN:-${GITLAB_TOKEN:-}}
    export FORGE_GIT_TOKEN
    ;;
  codeberg)
    export FORGE_GIT_USER=oauth2 FORGE_GIT_TOKEN=$KNIT_FORGEJO_TOKEN
    ;;
  bitbucket)
    export FORGE_GIT_USER=$bitbucket_username FORGE_GIT_TOKEN=$KNIT_BITBUCKET_API_TOKEN
    ;;
esac

forge_delete_repo() {
  local full=$1
  case "$forge" in
    github) gh repo delete "$full" --yes >/dev/null ;;
    gitlab) glab api --method DELETE "projects/$(jq -rn --arg value "$full" '$value|@uri')" >/dev/null ;;
    codeberg)
      curl --fail --silent --show-error -X DELETE \
        -H "Authorization: token ${KNIT_FORGEJO_TOKEN}" \
        "https://codeberg.org/api/v1/repos/$full" >/dev/null
      ;;
    bitbucket)
      curl --fail --silent --show-error -X DELETE \
        --user "${KNIT_BITBUCKET_EMAIL}:${KNIT_BITBUCKET_API_TOKEN}" \
        "https://api.bitbucket.org/2.0/repositories/$full" >/dev/null
      ;;
  esac
}

cleanup() {
  status=$?
  trap - EXIT INT TERM
  if [[ $keep -eq 0 ]]; then
    [[ -z $backend_full ]] || forge_delete_repo "$backend_full" || true
    [[ -z $frontend_full ]] || forge_delete_repo "$frontend_full" || true
  else
    printf 'kept repositories: %s %s\n' "$backend_full" "$frontend_full"
  fi
  rm -rf "$root"
  exit "$status"
}
trap cleanup EXIT INT TERM

forge_create_repo() {
  local name=$1
  local response full clone visibility
  case "$forge" in
    github)
      owner=$(gh api user --jq .login)
      full=$owner/$name
      gh repo create "$full" --private >/dev/null
      response=$(gh api "repos/$full")
      visibility=$(jq -r '.private' <<<"$response")
      [[ $visibility == true ]] || { echo "$full was not created private" >&2; exit 1; }
      clone=$(jq -r '.clone_url' <<<"$response")
      ;;
    gitlab)
      response=$(glab api --method POST projects \
        -f "name=$name" -f "visibility=private" \
        -f "initialize_with_readme=true" -f "default_branch=main")
      full=$(jq -r .path_with_namespace <<<"$response")
      response=$(glab api "projects/$(jq -rn --arg value "$full" '$value|@uri')")
      visibility=$(jq -r .visibility <<<"$response")
      [[ $visibility == private ]] || { echo "$full was not created private" >&2; exit 1; }
      clone=$(jq -r .http_url_to_repo <<<"$response")
      ;;
    codeberg)
      response=$(curl --fail --silent --show-error -X POST \
        -H "Authorization: token ${KNIT_FORGEJO_TOKEN}" \
        -H "Content-Type: application/json" \
        -d "$(jq -nc --arg name "$name" '{name:$name,private:true,auto_init:true,default_branch:"main"}')" \
        https://codeberg.org/api/v1/user/repos)
      full=$(jq -r .full_name <<<"$response")
      response=$(curl --fail --silent --show-error \
        -H "Authorization: token ${KNIT_FORGEJO_TOKEN}" \
        "https://codeberg.org/api/v1/repos/$full")
      visibility=$(jq -r .private <<<"$response")
      [[ $visibility == true ]] || { echo "$full was not created private" >&2; exit 1; }
      clone=$(jq -r .clone_url <<<"$response")
      ;;
    bitbucket)
      full=${BITBUCKET_SMOKE_WORKSPACE}/$name
      response=$(curl --fail --silent --show-error -X POST \
        --user "${KNIT_BITBUCKET_EMAIL}:${KNIT_BITBUCKET_API_TOKEN}" \
        -H "Content-Type: application/json" \
        -d "$(jq -nc --arg name "$name" '{name:$name,scm:"git",is_private:true}')" \
        "https://api.bitbucket.org/2.0/repositories/$full")
      response=$(curl --fail --silent --show-error \
        --user "${KNIT_BITBUCKET_EMAIL}:${KNIT_BITBUCKET_API_TOKEN}" \
        "https://api.bitbucket.org/2.0/repositories/$full")
      visibility=$(jq -r .is_private <<<"$response")
      [[ $visibility == true ]] || { echo "$full was not created private" >&2; exit 1; }
      clone=$(jq -r '.links.clone[]|select(.name=="https")|.href' <<<"$response")
      ;;
  esac
  printf '%s\t%s\n' "$full" "$clone"
}

IFS=$'\t' read -r backend_full backend_url < <(forge_create_repo "$backend_name")
IFS=$'\t' read -r frontend_full frontend_url < <(forge_create_repo "$frontend_name")
git clone "$backend_url" "$root/backend" >/dev/null
git clone "$frontend_url" "$root/frontend" >/dev/null

for repo in "$root/backend" "$root/frontend"; do
  git -C "$repo" config user.name "Knit Forge Smoke"
  git -C "$repo" config user.email "knit-forge-smoke@example.test"
  if ! git -C "$repo" rev-parse --verify HEAD >/dev/null 2>&1; then
    printf 'smoke\n' >"$repo/README.md"
    git -C "$repo" add README.md
    git -C "$repo" commit -m "Initialize smoke repository" >/dev/null
    git -C "$repo" branch -M main
    git -C "$repo" push -u origin main >/dev/null
  fi
done

cd "$workspace"
"$knit_bin" init smoke
"$knit_bin" project add backend "$root/backend"
"$knit_bin" project add frontend "$root/frontend"
run_nonce=$(printf '%s' "$timestamp-$RANDOM" | git hash-object --stdin | cut -c1-16)
bundle_slug=$run_nonce-forge-validation-$forge
"$knit_bin" bundle "$run_nonce Forge validation $forge"
printf 'backend smoke %s\n' "$timestamp" >>"$workspace/.knit/worktrees/$bundle_slug/backend/smoke.txt"
printf 'frontend smoke %s\n' "$timestamp" >>"$workspace/.knit/worktrees/$bundle_slug/frontend/smoke.txt"
"$knit_bin" commit --all -m "Exercise $forge forge flow"
"$knit_bin" push --set-upstream
"$knit_bin" publish create --no-sync
"$knit_bin" publish sync
"$knit_bin" publish status --live
bundle=$workspace/.knit/bundles/$bundle_slug.bundle.json
review_urls=()
while IFS= read -r review_url; do
  review_urls+=("$review_url")
done < <(jq -r '.publications[].url' "$bundle")
before_backend=$(git ls-remote "$backend_url" refs/heads/main | cut -f1)
before_frontend=$(git ls-remote "$frontend_url" refs/heads/main | cut -f1)
"$knit_bin" land
"$knit_bin" land apply --no-remote

forge_pr_state() {
  local url=$1
  case "$forge" in
    github) gh pr view "$url" --json state --jq .state ;;
    gitlab)
      path=${url#https://gitlab.com/}
      project=$(printf '%s' "$path" | sed 's#/-/merge_requests/.*##')
      iid=${path##*/}
      glab api "projects/$(jq -rn --arg value "$project" '$value|@uri')/merge_requests/$iid" |
        jq -r '.state|ascii_upcase'
      ;;
    codeberg)
      curl --fail --silent --show-error \
        -H "Authorization: token ${KNIT_FORGEJO_TOKEN}" \
        "https://codeberg.org/api/v1/repos/${url#https://codeberg.org/}" |
        jq -r 'if .merged then "MERGED" else .state|ascii_upcase end'
      ;;
    bitbucket)
      path=${url#https://bitbucket.org/}
      path=${path/\/pull-requests\//\/pullrequests\/}
      curl --fail --silent --show-error \
        --user "${KNIT_BITBUCKET_EMAIL}:${KNIT_BITBUCKET_API_TOKEN}" \
        "https://api.bitbucket.org/2.0/repositories/$path" |
        jq -r .state
      ;;
  esac
}

for url in "${review_urls[@]}"; do
  state=$(forge_pr_state "$url")
  case "$state" in
    MERGED|merged) ;;
    *) echo "review did not merge: $url ($state)" >&2; exit 1 ;;
  esac
done
after_backend=$(git ls-remote "$backend_url" refs/heads/main | cut -f1)
after_frontend=$(git ls-remote "$frontend_url" refs/heads/main | cut -f1)
[[ -n $after_backend && $after_backend != "$before_backend" ]]
[[ -n $after_frontend && $after_frontend != "$before_frontend" ]]
printf 'forge-smoke passed: %s (%s, %s)\n' "$forge" "$backend_full" "$frontend_full"
