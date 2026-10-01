PR #113365 — feat(agent): read the tool_disabled rejection reason

# Summary

Upcoming PRs record a new reason when the agent calls a tool that is unavailable in the current run. This PR teaches everything that reads rejection reasons to handle it first.

When the agent calls a tool that can't run, the trajectory records the call as rejected, with a reason. Prompt building, billing, the debug view and the chat UI all read that reason. The next PRs in this stack reject calls to tools that are unavailable in the current run (for example a code-editing tool in chat mode), instead of reporting them as unknown tools.

# Changes

- Adds a `tool_disabled` tool call rejection reason and teaches every reader of it to handle it. Nothing writes it yet.

# Low level

A reader that receives a rejection enum value it doesn't know renders the bare "Tool call was rejected." line. With this PR deployed before the producer (#113366), every reader already knows the value.

## Changes

- The trajectory proto gains `TOOL_REJECTION_REASON_TOOL_DISABLED` (14), with regenerated Go and TS code.
- The main agent, compaction and loop-side tool-result prompt builders render the executor's `SoftError` text, falling back to "Tool X is unavailable here, tool did not run."
- `IsFreeRejection` treats it as free, like `disabled_by_user`, because the call never ran.
- The eval data reducer, work-group descriptions and web `rejectionVisibility` hide it, like other pre-execution rejections.
- The debug view labels it "Tool disabled", and the warehouse `fct_tool_call.rejection_reason` doc lists it.
