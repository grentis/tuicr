#!/usr/bin/env bash
# Connect the agent in this herdr pane to the tuicr reviewing a checkout.
#
# A tuicr already reviewing the checkout's branch (`tuicr -b`) is reused,
# where it is, without switching to it; otherwise one is started in a new
# herdr tab of the agent's workspace (the one its pane is in now, found by
# Claude session rather than the inherited HERDR_* variables), left in the
# background, and awaited. Either way the agent's pane is attached to its
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
# The agent's pane and workspace. HERDR_PANE_ID and HERDR_WORKSPACE_ID (what
# `herdr pane current` reads) are inherited, so they go stale when the agent
# outlives its pane: a session resumed or forked through the Claude daemon
# keeps the IDs of the pane that started the daemon. Herdr tags every agent
# pane with its Claude session, so the pane is looked up by this session's ID,
# then by the session it resumed or forked from; the inherited IDs are only
# the fallback for an agent herdr does not know.
session_ids() {
  [[ -n ${CLAUDE_CODE_SESSION_ID:-} ]] && printf '%s\n' "$CLAUDE_CODE_SESSION_ID"
  [[ -n ${CLAUDE_PID:-} && -r /proc/$CLAUDE_PID/cmdline ]] || return 0
  tr '\0' '\n' </proc/"$CLAUDE_PID"/cmdline |
    awk 'prev == "--session-id" || prev == "--resume" { print } { prev = $0 }' |
    sed 's|.*/||; s|\.jsonl$||'
}

agent=
workspace=
panes=$(herdr pane list) || die "could not list the herdr panes"
while read -r id; do
  [[ -n $id ]] || continue
  mapfile -t matches < <(jq -r --arg id "$id" '
    .result.panes[] | select(.agent_session.value == $id)
                    | "\(.pane_id) \(.workspace_id)"' <<<"$panes")
  if ((${#matches[@]} > 1)); then
    die "several herdr panes run Claude session $id: $(printf '%s ' "${matches[@]%% *}")"
  fi
  if ((${#matches[@]} == 1)); then
    read -r agent workspace <<<"${matches[0]}"
    break
  fi
done < <(session_ids | awk '!seen[$0]++')

if [[ -z $agent ]]; then
  current=$(herdr pane current) || die "could not find this herdr pane"
  agent=$(jq -er '.result.pane.pane_id' <<<"$current") || die "could not find this herdr pane"
  workspace=$(jq -er '.result.pane.workspace_id' <<<"$current") || die "could not find this pane's workspace"
fi

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
