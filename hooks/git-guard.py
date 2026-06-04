#!/usr/bin/env python3
"""Git guard hook for Claude Code PreToolUse.

Reads tool input from stdin (JSON with .tool_input.command),
checks the command against git rules, and outputs a permission decision.
"""

import json
import re
import sys

RULES = [
    # (pattern, exclude_pattern, decision, reason)
    (r"\bgit\s+add\b[^|&;]*?(\s--all\b|\s-[a-z]*A[a-z]*\b)", None, "deny",
     "git add -A / --all is not allowed. Stage files explicitly by path (e.g. `git add path/to/file`) so unintended changes are never committed."),
    (r"\bgit\s+merge\s+", None, "deny", "git merge is not allowed. Rebase instead."),
    (r"\bgit\s+commit\b", None, "ask", "git commit requires confirmation"),
    (r"\bgit\s+(rebase|reset)\b", r"\bgit\s+rebase\s+--continue\b", "ask", "git rebase/reset requires confirmation"),
]


def check_command(cmd: str) -> dict | None:
    for pattern, exclude, decision, reason in RULES:
        if re.search(pattern, cmd):
            if exclude and re.search(exclude, cmd):
                continue
            return {
                "hookSpecificOutput": {
                    "hookEventName": "PreToolUse",
                    "permissionDecision": decision,
                    "permissionDecisionReason": reason,
                }
            }
    return None


def main():
    data = json.load(sys.stdin)
    cmd = data.get("tool_input", {}).get("command", "")
    result = check_command(cmd)
    if result:
        json.dump(result, sys.stdout)


if __name__ == "__main__":
    main()
