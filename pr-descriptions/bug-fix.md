PR #38228 — fix(agent): downgrade missing ToolApprovalRequired to stale decision

# Summary

- Treat a tool decision arriving without a preceding `ToolApprovalRequired` event as a stale decision rather than a fatal error.
- Fixes `MAIN_LOOP_FAILED` / `TRAJECTORY_EMISSION_FAILED` errors that started appearing in production around 15:00–16:00 today.

# Why

- Legacy frontend renders the approval prompt as soon as it sees the tool call, without waiting for `ToolApprovalRequired`.
- Race: user stops the agent (cancellation appends a `ToolRejection`) and submits a response in parallel, which the API receives as a tool decision.
- `emitToolDecisionOnAgentHead` then finds the tool call but no `ToolApprovalRequired`, raising a fatal unexpected error.
- UI2 is unaffected because it waits for `ToolApprovalRequired` before rendering.

# What changed

- Missing `ToolApprovalRequired` is now reported as a `staleToolDecisionError` instead of an unexpected fatal error.
- `LockAgentTrajectoryHead` already swallows `staleToolDecisionError`, so the agent loop continues against existing trajectory state.

# Links

- <a href="https://lovable.grafana.net/explore?....">Grafana: MAIN_LOOP_FAILED + TRAJECTORY_EMISSION_FAILED errors today"</a>
- <a href="https://lovable.grafana.net/d/arwvh2v/agent-tool-trace-list?orgId=1&from=2026-04-24T22:00:00.000Z&to=2026-04-25T21:59:59.000Z&timezone=browser&var-tool_name=$__all&var-status_type=error&var-routing_decision=$__all&var-message_filter=&var-project_filter=">Grafana: Tool errors today</a>
