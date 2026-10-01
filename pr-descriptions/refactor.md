PR #38514 — feat: make plan tool into a native tool (stage 2)

# Summary

The plan tool was still partly baked into the prompt instead of being a real tool. This second stage removes the prompt-baked version, leaving only the native tool.

The agent's plan tool started as a "soft" tool: its instructions and handling were baked into the prompt instead of being a real tool the agent calls. This is stage 2 of turning it into a native tool, and it removes the prompt-baked version.

# Changes

- Removes the prompt-baked plan tool, leaving only the native tool.
- Converts plans stored in the legacy format into native tool calls when reading old trajectories.

# Rollout order

Deploy only after stage 1 has been fully deployed. During a deploy, data can flow `old client -> new backend -> old client -> old backend`, and an old backend without stage 1 can't handle native plan tool calls.

# Low level

Plan behavior lived in `primary_llm_prompt`, agent reminders, the static/dynamic critical instructions and the system chat prompt, and older trajectories store plans in that prompt-baked format.

## Changes

- Remove plan-related logic from `primary_llm_prompt`, agent reminders, and static/dynamic critical instructions. The native tool handles these now.
- Drop legacy plan handling from the system chat prompt and simplify the chat reviewer config to match.
- Extend the trajectory parser and legacy converter to convert legacy formats to a native tool call.
- Update eval tasks and associated evaluator.
