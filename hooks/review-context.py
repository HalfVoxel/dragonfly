#!/usr/bin/env python3
"""SubagentStart hook for Claude Code.

When a `review-agent` subagent (see agents/review-agent.md) is spawned by
the parent dragonfly flow, this hook shells out to
    dragonfly prompt review-agent
and pipes stdout through to the subagent. Claude Code delivers a
SubagentStart hook's stdout to the subagent as a system reminder before
its first turn (documented for SessionStart / Setup / SubagentStart in
[hooks docs](https://code.claude.com/docs/en/hooks)).

The orchestration heavy-lifting — assembling commit list, changed files,
per-file diff files, and the scored <relevant-context> block — lives in
the Rust binary. That command serializes parallel callers behind a
filesystem flock so a multi-agent fan-out only pays the build cost once
within a four-minute TTL.

`dragonfly` is expected to be on PATH (the orchestrator adds the
binary's own dir before exec'ing `claude`). When the hook is run outside
that context (manual `claude` invocation against this settings file)
the subprocess will simply fail and we exit 0 — failing open is
preferable to breaking the parent's review flow.

The matcher in settings/dragonfly-settings.json gates this hook on
agent_type == "review-agent", but we double-check here so an accidental
wildcard matcher doesn't shell out for every subagent.
"""

import json
import shutil
import subprocess
import sys

EXPECTED_AGENT = "review-agent"
BIN_NAME = "dragonfly"


def _debug(msg: str) -> None:
    # Debug trace for development; harmless in prod. Set
    # DRAGONFLY_HOOK_DEBUG=1 in the environment to enable.
    import os
    if os.environ.get("DRAGONFLY_HOOK_DEBUG"):
        with open("/tmp/dragonfly-hook.log", "a") as fh:
            fh.write(msg + "\n")


def main() -> int:
    raw_stdin = sys.stdin.read()
    _debug(f"[hook] invoked, stdin_len={len(raw_stdin)}")
    try:
        payload = json.loads(raw_stdin)
    except json.JSONDecodeError:
        _debug(f"[hook] bad json: {raw_stdin[:200]!r}")
        # Unparseable input is a hook-runner bug, not ours. Fail open.
        return 0
    _debug(
        f"[hook] event={payload.get('hook_event_name')!r} "
        f"agent_type={payload.get('agent_type')!r} "
        f"agent_id={payload.get('agent_id')!r}"
    )

    if payload.get("hook_event_name") != "SubagentStart":
        return 0
    if payload.get("agent_type") != EXPECTED_AGENT:
        return 0

    bin_path = shutil.which(BIN_NAME)
    if bin_path is None:
        print(
            f"review-context: {BIN_NAME!r} not on PATH; skipping context injection.",
            file=sys.stderr,
        )
        return 0

    try:
        # cwd defaults to the subagent's cwd (same as parent's main cwd),
        # which is what `dragonfly prompt review-agent` keys its
        # cache by. Don't override it.
        result = subprocess.run(
            [bin_path, "prompt", "review-agent"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            check=False,
            timeout=600,
        )
    except (OSError, subprocess.TimeoutExpired) as e:
        print(f"review-context: {BIN_NAME} invocation failed: {e}", file=sys.stderr)
        return 0

    # Propagate the subprocess's stderr so build/timing logs surface in
    # the parent agent's transcript when something goes wrong.
    if result.stderr:
        sys.stderr.buffer.write(result.stderr)
    if result.returncode != 0:
        print(
            f"review-context: {BIN_NAME} exited {result.returncode}; skipping.",
            file=sys.stderr,
        )
        return 0
    if not result.stdout:
        return 0

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
