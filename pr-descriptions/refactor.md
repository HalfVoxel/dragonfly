PR #38514 — feat: make plan tool into a native tool (stage 2)

# Summary

- Stage 2 of converting the plan tool from a prompt-baked "soft" tool into a true native tool.
- PR was split into multiple stages to avoid problems when deploying, since data flow could go `old client -> new backend -> old client -> old backend`, which the old backend could not handle.

# What changed

- Remove plan-related logic from `primary_llm_prompt`, agent reminders, and static/dynamic critical instructions. The native tool handles these now.
- Drop legacy plan handling from the system chat prompt and simplify the chat reviewer config to match.
- Extend the trajectory parser and legacy converter to convert legacy formats to a native tool call.
- Update eval tasks and associated evaluator.
