#!/usr/bin/env bash
# Connect the agent in this herdr pane to the tuicr reviewing a checkout.
#
# A tuicr already reviewing the checkout's branch (`tuicr -b`) is reused,
# where it is, without switching to it; otherwise one is started in a new
# herdr tab of the agent's workspace, left in the background, and awaited. Either way the agent's pane is attached to its
# session (`tuicr review attach`), so `:submit` in tuicr runs `[agent]
# send_command` with this pane's id in TUICR_AGENT. Prints the session as
# JSON and returns at once: nothing here waits on the review.
#
# Usage: tuicr-connect.sh [checkout]   (default: the current directory)
#
# TUICR_LAUNCH_COMMAND  what to start when nothing is running (default: tuicr -b)
# TUICR_CONNECT_TIMEOUT seconds to wait for it to come up (default: 60)

set -euo pipefail

dir=${1:-$PWD}
launch=${TUICR_LAUNCH_COMMAND:-tuicr -b}
timeout=${TUICR_CONNECT_TIMEOUT:-60}

die() {
  printf 'tuicr-connect: %s\n' "$*" >&2
  exit 1
}

[[ ${HERDR_ENV:-} == 1 ]] || die "not running inside a herdr pane"
root=$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null) || die "$dir is not a git checkout"
branch=$(git -C "$root" branch --show-current)
[[ -n $branch ]] || die "$root has a detached HEAD; a branch review needs a branch"
current=$(herdr pane current) || die "could not find this herdr pane"
agent=$(jq -er '.result.pane.pane_id' <<<"$current") || die "could not find this herdr pane"
workspace=$(jq -er '.result.pane.workspace_id' <<<"$current") || die "could not find this pane's workspace"

# "<slug> <pid>" for each running tuicr reviewing this checkout's branch.
# Slugs write a branch's `/` as `-` (tuicr's `sanitize_ref`).
running() {
  tuicr review list --repo "$root" | jq -r --arg branch "$branch" '
    .[] | select(.kind == "local" and .active
                 and (.slug | endswith("@" + ($branch | gsub("/"; "-")) + "/branch")))
        | "\(.slug) \(.pid // "")"'
}

mapfile -t found < <(running)
if ((${#found[@]} > 1)); then
  die "several tuicr instances review $branch: $(printf '%s ' "${found[@]%% *}")"
fi

if ((${#found[@]} == 1)); then
  read -r session _ <<<"${found[0]}"
  how=connected
else
  pane=$(herdr tab create --workspace "$workspace" --cwd "$root" --label "tuicr $branch" --no-focus |
    jq -er '.result.root_pane.pane_id') || die "could not open a herdr tab"
  herdr pane run "$pane" "$launch" >/dev/null
  session=
  deadline=$((SECONDS + timeout))
  while ((SECONDS < deadline)); do
    read -r session _ < <(running | head -n 1) || true
    [[ -n $session ]] && break
    sleep 1
  done
  [[ -n $session ]] || die "'$launch' did not start a review of $branch within ${timeout}s (pane $pane)"
  how=opened
fi

tuicr review attach --repo "$root" --session "$session" --agent "$agent" >/dev/null

jq -n --arg session "$session" --arg repo "$root" --arg branch "$branch" \
  --arg agent "$agent" --arg how "$how" \
  '{session: $session, repo: $repo, branch: $branch, agent: $agent, how: $how}'
