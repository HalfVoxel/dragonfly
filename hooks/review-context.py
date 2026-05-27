#!/usr/bin/env python3
"""SubagentStart hook for Claude Code.

When a `review-agent` subagent (see agents/review-agent.md) is spawned by
the parent push-and-fix flow, this hook prints a `<dragonfly-context>`
block to stdout. Plain stdout from a SubagentStart hook is delivered to
the subagent as a system reminder before its first turn, so the body
below lands in the subagent's context without going through the parent's
prompt.

The context comes from `$DRAGONFLY_CONTEXT_FILE`, which the dragonfly
orchestrator writes before invoking `claude`. The file is a free-form
markdown blob — typically the same files-index that lands in the main
prompt, plus a `<relevant-context>` block and any `initial-review`
findings. We pass it through verbatim; the agent's frontmatter describes
the expected shape.

The matcher in settings/push-and-check-settings.json gates this hook on
agent_type == "review-agent", but we double-check here so an
accidental wildcard matcher doesn't leak the review context into other
subagents.

No output is emitted (and exit 0 still) when:
  - DRAGONFLY_CONTEXT_FILE is unset or points at a missing file
  - the file is empty
  - the spawned agent's type isn't review-agent
That way the hook fails open: the subagent simply runs without injected
context instead of breaking the parent's review flow.
"""

import json
import os
import sys

EXPECTED_AGENT = "review-agent"


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        # Unparseable input is a hook-runner bug, not ours. Fail open.
        return 0

    if payload.get("hook_event_name") != "SubagentStart":
        return 0
    if payload.get("agent_type") != EXPECTED_AGENT:
        return 0

    ctx_path = os.environ.get("DRAGONFLY_CONTEXT_FILE")
    if not ctx_path or not os.path.isfile(ctx_path):
        return 0

    try:
        with open(ctx_path, "r", encoding="utf-8") as f:
            body = f.read()
    except OSError as e:
        # Log to stderr so the orchestrator notices, but don't block.
        print(f"review-context: failed to read {ctx_path}: {e}", file=sys.stderr)
        return 0

    if not body.strip():
        return 0

    sys.stdout.write("<dragonfly-context>\n")
    sys.stdout.write(body)
    if not body.endswith("\n"):
        sys.stdout.write("\n")
    sys.stdout.write("</dragonfly-context>\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
