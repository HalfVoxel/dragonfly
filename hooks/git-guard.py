#!/usr/bin/env python3
"""Git guard hook for Claude Code PreToolUse and PermissionRequest.

Reads tool input from stdin (JSON with .tool_input.command),
checks the command against git rules, and outputs a permission decision.

Registered on both events because PreToolUse fires after the permission
dialog: a deny there makes the user approve the command only to watch the
hook reject it. PermissionRequest fires while the dialog is still pending,
so a deny cancels the prompt before the user sees it. PreToolUse stays
registered to catch commands that are allowlisted (e.g. Bash(git:*)) and
therefore never raise a dialog.
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


def check_command(cmd: str) -> tuple[str, str] | None:
    for pattern, exclude, decision, reason in RULES:
        if re.search(pattern, cmd):
            if exclude and re.search(exclude, cmd):
                continue
            return decision, reason
    return None


def build_output(event: str, decision: str, reason: str) -> dict | None:
    if event == "PermissionRequest":
        # "ask" has no PermissionRequest form: staying silent lets the pending
        # dialog show, which is the same outcome.
        if decision != "deny":
            return None
        return {
            "hookSpecificOutput": {
                "hookEventName": "PermissionRequest",
                "decision": {"behavior": "deny", "message": reason},
            }
        }
    return {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": decision,
            "permissionDecisionReason": reason,
        }
    }


def main():
    data = json.load(sys.stdin)
    cmd = data.get("tool_input", {}).get("command", "")
    result = check_command(cmd)
    if result:
        output = build_output(data.get("hook_event_name", "PreToolUse"), *result)
        if output:
            json.dump(output, sys.stdout)


if __name__ == "__main__":
    main()
