#!/usr/bin/env python3
"""SubagentStart hook for Claude Code.

When a `review-agent`, `comment-reviewer`, `dedup-reviewer`, or
`test-reviewer` subagent (see agents/) is spawned by the parent dragonfly
flow, this hook shells out to
    dragonfly prompt review-agent [--inline-diffs]   # review/comment/test agents
    dragonfly prompt dedup-reviewer                  # dedup agent
and returns the output as the subagent's initial context via
`hookSpecificOutput.additionalContext` JSON (plain hook stdout does not
reach SubagentStart subagents; see the comment at the emit site below).

`comment-reviewer` gets `--inline-diffs` (full diffs inlined in the
context); `review-agent` and `test-reviewer` get the default /tmp
diff-file references (both read full source files anyway);
`dedup-reviewer` gets its own context with the full duplicate-function
hint list inlined.

The orchestration heavy-lifting — assembling commit list, changed files,
per-file diff files, and the scored <relevant-context> block — lives in
the Rust binary. That command serializes parallel callers behind a
filesystem flock so a multi-agent fan-out only pays the build cost once
within a four-minute TTL.

Contract: the hook never fails open. Every failure to produce context
exits 2 (blocking) rather than letting the reviewer start empty; see
[_fail]. `dragonfly` is expected to be on PATH — the orchestrator adds
the binary's own dir before exec'ing `claude`, so a manual `claude`
invocation against this settings file fails the spawn until the binary
is installed.

Matchers in settings/dragonfly-settings.json gate this hook on agent_type
"review-agent", "comment-reviewer", "dedup-reviewer", and "test-reviewer";
the hook keys the --inline-diffs flag off that same agent_type. These
matchers only ever see bare agent types, so the namespace strip below is
a no-op here; it exists to keep this body in sync with the plugin's
vendored copy, which receives "dragonfly-review:"-prefixed agent types
and deliberately runs its subprocess with a shorter (550s) timeout.
"""

import json
import shutil
import subprocess
import sys

BIN_NAME = "dragonfly"


def _debug(msg: str) -> None:
    # Debug trace for development; harmless in prod. Set
    # DRAGONFLY_HOOK_DEBUG=1 in the environment to enable.
    import os

    if os.environ.get("DRAGONFLY_HOOK_DEBUG"):
        with open("/tmp/dragonfly-hook.log", "a") as fh:
            fh.write(msg + "\n")


def _fail(reason: str) -> int:
    """Aborts the spawn with a blocking hook error.

    Contract: this hook never fails open. A reviewer subagent that starts
    without <dragonfly-context> reviews an empty diff and reports "no
    issues found", which reads as a clean review rather than a broken
    one. Exit 2 is Claude Code's blocking-error code: the spawn fails and
    stderr surfaces, so a stale cache or a missing binary is a visible
    failure the caller must fix, not a silently degraded review.
    """
    _debug(f"[hook] failing loudly: {reason}")
    print(f"review-context: {reason}", file=sys.stderr)
    print(
        "review-context: refusing to start the reviewer without "
        "<dragonfly-context>. Fix the cause and re-spawn.",
        file=sys.stderr,
    )
    return 2


def main() -> int:
    raw_stdin = sys.stdin.read()
    _debug(f"[hook] invoked, stdin_len={len(raw_stdin)}")
    try:
        payload = json.loads(raw_stdin)
    except json.JSONDecodeError:
        _debug(f"[hook] bad json: {raw_stdin[:200]!r}")
        return _fail(f"unparseable hook payload: {raw_stdin[:200]!r}")
    _debug(
        f"[hook] event={payload.get('hook_event_name')!r} "
        f"agent_type={payload.get('agent_type')!r} "
        f"agent_id={payload.get('agent_id')!r}"
    )

    if payload.get("hook_event_name") != "SubagentStart":
        return 0

    bin_path = shutil.which(BIN_NAME)
    if bin_path is None:
        return _fail(f"{BIN_NAME!r} not on PATH; cannot build review context")

    # dedup-reviewer has its own tailored context (full hint list inlined);
    # the other reviewers share the review-agent context, with only the
    # comment reviewer paying for inlined diffs.
    agent_type = (payload.get("agent_type") or "").split(":")[-1]
    if agent_type == "dedup-reviewer":
        cmd = [bin_path, "prompt", "dedup-reviewer"]
    else:
        cmd = [bin_path, "prompt", "review-agent"]
        if agent_type == "comment-reviewer":
            cmd.append("--inline-diffs")

    try:
        # cwd defaults to the subagent's cwd (same as parent's main cwd),
        # which is what `dragonfly prompt review-agent` keys its
        # cache by. Don't override it.
        result = subprocess.run(
            cmd,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=600,
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        return _fail(f"{BIN_NAME} invocation failed: {e}")

    # Propagate the subprocess's stderr so build/timing logs surface in
    # the parent agent's transcript when something goes wrong.
    if result.stderr:
        sys.stderr.buffer.write(result.stderr)
    if result.returncode != 0:
        return _fail(f"{BIN_NAME} exited {result.returncode}")
    if not result.stdout:
        return _fail(f"{BIN_NAME} produced no context on stdout")

    # Claude Code routes a SubagentStart hook's
    # `hookSpecificOutput.additionalContext` into the subagent's initial
    # system reminder. Empirically plain stdout does NOT land for this
    # event in current builds (it works for SessionStart); the JSON form
    # is the reliable channel.
    body = result.stdout.decode("utf-8", errors="replace")
    _debug(f"[hook] returning {len(body)}ch via additionalContext")
    json.dump(
        {
            "hookSpecificOutput": {
                "hookEventName": "SubagentStart",
                "additionalContext": body,
            }
        },
        sys.stdout,
    )
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
