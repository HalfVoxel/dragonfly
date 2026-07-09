# dragonfly-watcher

A Claude Code [channel](https://code.claude.com/docs/en/channels-reference)
that watches the current branch's PR and pushes events into the running
session, so the agent reacts to CI results and review comments without
polling:

- `ci_check_failed` — a check just failed (name, link; the agent is told to
  run `dragonfly ci failures`)
- `ci_settled` — all checks finished (tally, failed names)
- `review_comment` / `pr_comment` — a new comment by someone else

The MCP server is `dragonfly watch-mcp` (bundled in the dragonfly binary);
`bin/watcher.sh` only locates the binary. The first successful fetch is a
silent baseline — only changes observed while the session is live become
events. A fresh push re-anchors the watch, mirroring `dragonfly ci watch`.

## Install

```
claude plugin marketplace add HalfVoxel/dragonfly   # or the local checkout path
claude plugin install dragonfly-watcher@dragonfly
```

Channels are a research preview: custom channels are not on the Anthropic
allowlist, so the session must be started with

```
claude --dangerously-load-development-channels plugin:dragonfly-watcher@dragonfly
```

## Testing

- Protocol/demo: `dragonfly watch-mcp --demo` emits one synthetic event 3s
  after the MCP handshake.
- `DRAGONFLY_WATCH_INCLUDE_SELF=1` disables the own-comments filter so a
  single-account test can observe its own comments as events.
