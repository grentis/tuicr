---
name: tuicr-connect
description: Connect to the tuicr reviewing this checkout's branch, opening one in a new herdr tab if none runs, and wait in the background for the user's `:submit`; then work through the review comments waiting on you and answer each in its thread. Use when the user types /tuicr-connect, when the background `tuicr review wait` finishes, or when a "/tuicr-connect comments" message arrives. (Upstream's /tuicr skill, which opens tuicr on a diff and reads its comments when it closes, is a separate skill.)
---

# tuicr connection

The user reviews the agent's work in tuicr (`tuicr -b`, a local branch
review). This skill links the two: the agent learns the review's session and
waits for it in the background, and `:submit` in tuicr ends that wait.
Nothing is typed into the agent's pane.

Local reviews only. Never attach to, wait on, or act on a PR session
(`gh:…/pr/N`).

Your name in the review is **`ai-agent`**: every reply you post carries it
(`--username ai-agent`), and you ask for what waits on it
(`--needs-reply ai-agent`). Use exactly this name on both sides: it is how
tuicr tells your messages from everyone else's.

## `/tuicr-connect` — connect

Run the launcher from the checkout being worked on:

```bash
~/.claude/skills/tuicr-connect/tuicr-connect.sh /path/to/checkout
```

It reuses the tuicr already reviewing the checkout's branch, leaving the
user where they are, or opens one in a new herdr tab and waits for it to come
up. It returns at once with JSON:

```json
{"session": "owner/repo@branch/branch", "repo": "/path", "branch": "branch", "agent": "w1N:p1", "how": "connected"}
```

`how` is `connected` (an instance was running) or `opened` (a new tab).
Remember `session` and `repo` for the rest of the conversation.

Then start the wait, **in the background** (the Bash tool's
`run_in_background`), so it never blocks you:

```bash
tuicr review wait --repo <repo> --session <session>
```

Tell the user in one line which instance you connected to, then carry on with
whatever you were doing. Do not poll or check on the wait: you are told when
it finishes.

If the launcher fails, report its error and stop. It refuses a detached HEAD,
several instances on one branch, and running outside herdr.

## The wait finished — the user submitted a review

When the background `tuicr review wait` completes:

- **Exit 0** (it printed the session and `submitted_at`): the user ran
  `:submit`. **First start the wait again**, in the background, the same
  way, so a `:submit` made while you work is not lost. Then process the
  comments below straight away; do not ask whether to.
- **It failed**: report its error in one line and do not restart it. "was
  closed" means the user quit tuicr; "another agent is waiting" means
  someone else took the session over; "no running tuicr" means it was not
  open. The user runs `/tuicr-connect` again to reconnect.

A `/tuicr-connect comments <session>` message (tuicr's typed fallback, used
when no wait is running) means the same: process the comments. If
`<session>` is not the one you connected to, or you no longer know its
`repo`, run the launcher again and use what it returns.

### 1. Read what is waiting

```bash
tuicr review comments --repo <repo> --session <session> --needs-reply ai-agent
```

It returns, in order, every thread whose last message is not yours: each
thread's first comment, then its replies (they carry `reply_to`, the first
comment's id). Earlier messages are context: yours say what you already did;
the messages after your last reply say what is wanted now. Handle all of
them, not only the last. An empty list means nothing is waiting: report done
(step 6's command, `--message "nothing waiting"`), say so in one line, and
stop.

`location` is `path:line` (`[old]` for a removed line), `path` for a file
comment, `review` for a review-level one. Read the code there before acting.

### 2. Recap what you received

Before changing anything, list what you were asked, one bullet per thread in
the order they came: where it is, and the request in a few words.

```
Received 3 threads:
- `src/lexer.rs:42`: rename this
- `src/main.rs`: handle the error
- `review`: aa
```

### 3. Act on each thread

Every thread the list returns is open work, whatever happened before. Judge
it against the code as it is now: if you answered it earlier and your reply
is not there any more, or you remember doing it, it still gets done and
answered now. Never ask the user why an earlier reply is missing, and never
mention it: just do the thread.

- A request you agree with: make the change in the working tree. Do not
  commit, stage, or amend: the user reviews it as an uncommitted change.
- A request you disagree with, or that is already done: change nothing and
  say why.
- An unclear comment: change nothing and ask, in the thread.
- A question or a remark: answer it.

### 4. Answer every thread, in the thread

Right before replying, read the waiting threads again (step 1's command): the
user may have added to this thread while you worked. If they did, handle the
new message too, and let your reply cover it.

```bash
tuicr review add --repo <repo> --session <session> --reply-to <id> \
  --username ai-agent "<what you did, or why not, or your question>"
```

`<id>` is the id of any message in the thread, review-level ones included.
Keep it short and concrete: what changed and where (`Renamed to parse_line in
src/lexer.rs`), or the reason, or the question. One reply per thread, and
every thread gets one: the reply is what takes it off the list until someone
answers. Answers go in the thread, never in the chat.

### 5. Check again before stopping

Run step 1's command once more. Anything it lists arrived while you worked (a
new `:submit`, or a message added to a thread you had already answered): do
another round from step 2 for those threads. Stop only when it is empty.

### 6. Recap what you did

End with the same list, now saying what you did for each:

```
Done, 3 threads answered:
- `src/lexer.rs:42` rename this: renamed to `parse_line` (uncommitted)
- `src/main.rs` handle the error: already handled by `run()`, no change
- `review` aa: unclear, asked what should change
```

Then tell tuicr, with the same summary:

```bash
tuicr review done --repo <repo> --session <session> --message "3 threads answered"
```

It shows in tuicr's status bar. The recap mirrors your replies in the threads;
the user sees them, with your code changes, after `:e` in tuicr. When the restarted wait later finishes for a
`:submit` you already handled in step 5, step 1 finds nothing: say so in one
line.
