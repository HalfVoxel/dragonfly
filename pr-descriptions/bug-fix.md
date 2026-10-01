PR #38228 — fix(agent): downgrade missing ToolApprovalRequired to stale decision

# Summary

Stopping the agent while answering a tool approval prompt could fail the whole agent run. The backend now treats that answer as stale and carries on.

When a tool call needs the user's approval, the agent records that it is waiting, and the user's answer is recorded as a decision on that call. The legacy frontend shows the approval prompt as soon as it sees the tool call, before the agent records that it is waiting. If the user stops the agent and answers the prompt at the same time, the backend receives a decision for a call that never asked for one, and it treated that as a fatal error that failed the agent run. The newer UI is unaffected because it waits for the agent before showing the prompt.

Agent runs started failing this way in production around 15:00–16:00 today.

# Changes

- A tool decision that arrives without an approval request is treated as stale instead of fatal, so the agent run continues.

# Links

- <a href="https://lovable.grafana.net/explore?....">Grafana: MAIN_LOOP_FAILED + TRAJECTORY_EMISSION_FAILED errors today"</a>
- <a href="https://lovable.grafana.net/d/arwvh2v/agent-tool-trace-list?orgId=1&from=2026-04-24T22:00:00.000Z&to=2026-04-25T21:59:59.000Z&timezone=browser&var-tool_name=$__all&var-status_type=error&var-routing_decision=$__all&var-message_filter=&var-project_filter=">Grafana: Tool errors today</a>

# Low level

Stopping the agent appends a `ToolRejection`, while the parallel answer reaches the API as a tool decision. `emitToolDecisionOnAgentHead` then finds the tool call but no `ToolApprovalRequired`, and raises an unexpected error that surfaces as `MAIN_LOOP_FAILED` / `TRAJECTORY_EMISSION_FAILED`. UI2 waits for `ToolApprovalRequired` before rendering the prompt.

## Changes

- Missing `ToolApprovalRequired` is now reported as a `staleToolDecisionError` instead of an unexpected fatal error.
- `LockAgentTrajectoryHead` already swallows `staleToolDecisionError`, so the agent loop continues against existing trajectory state.
