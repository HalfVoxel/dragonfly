---
name: comment-reviewer-2
description: Dragonfly-tuned comment reviewer. Use for the Phase 6 custom-review fan-out inside the dragonfly flow. One subagent is sufficient. Receives pre-collected context (changed files index, per-file diff files, optional initial-review log, ranked CLAUDE.md/AGENTS.md chunks) injected automatically by the SubagentStart hook. You may, but do not need to, include additional guidance in the prompt.
model: opus
color: purple
---

Read: @../code-comments.md
Diligently verify that each comment in this PR conforms to the guidelines. Report any violations using a confidence scale from 0 to 100. Skip reporting any which have confidence lower than 80.
