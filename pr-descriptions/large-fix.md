PR #99569 — fix(trajectory): bound memory of cold checkpointed reducer walks

# Summary

Rebuilding agent state from a long history with no saved checkpoint loaded the entire history into memory, which ran temporal workers out of memory. The rebuild now works through the history in small pieces, keeping one in memory at a time.

An agent's history on a project is an event-sourced trajectory, and most facts the agent needs (current git branch, message counts, credit cost) come from replaying that history through a reducer. Replaying a long history on every request is too slow, so some reducers checkpoint their state at regular anchor points and resume from the nearest one. When no checkpoint exists yet, for example right after a deploy that adds or changes a checkpointed reducer, the walk goes all the way back to the start of the history. On the largest projects that is millions of events, and until now the walk kept every one of them in memory.

On Sep 8, concurrent cold walks over multi-million-event trajectories killed 4 temporal-worker pods at 19.3 GiB; one 2.5M-event walk alone cost ~12 GiB.

# Changes

- A cold checkpointed reducer walk now holds one ~3000-event segment in memory at a time instead of buffering the whole chain, and writes each checkpoint cell as it passes so a killed walk keeps its progress.
- The per-trajectory event cache becomes a bucketed LRU capped at 64k events, and history scans are steered toward ~4000 events per fetch, so no single walk can grow the heap without bound.

# Low level

`reduceCheckpointed` seeds a `core.Checkpointable` reducer from the nearest checkpoint cell found by walking `History` newest-first, then advances forward and writes a cell at every anchor (roughly one per 1000 events). Before this PR it appended the entire backward walk to a slice, handed all cells to the async checkpoint worker only after the forward pass finished, and read through a `storageTrajectory` event cache that never evicted with a scan batch size that doubled without a cap. A walk that hit the warm timeout or an OOM-killed pod therefore wrote nothing and repeated on the next turn.

## Important behavioral changes

- Checkpoint cells are written synchronously inline as the forward pass crosses each anchor instead of being batched to the async worker after the walk; only deferred ancestry derivation still goes to the worker.
- `core.Checkpointable` now embeds `CanStartAtRoot`, so a reducer with only `CanStartAt` silently reduces cold (counted as `not_root_startable`) and the `ErrNoStartingPoint` path in the checkpointed walk is gone.
- Checkpointed reduces of one reducer key on one trajectory instance serialize on a new `Trajectory.CheckpointMutex`, so a concurrent second reduce waits and seeds from the first one's cells instead of repeating the walk.

## Changes

- `reduce_checkpoint.go`: the walk is split into a seek pass that streams history keeping only a boundary event ID every 1000 events, and a forward pass that loads each segment with `HistoryBetween`, advances the reducer, and writes anchor cells inline.
- `reduce_checkpoint_writer.go`: seeded verification reuses `walkCheckpointed` with `skipTipCell`, replacing the separate `reduceForVerification` walk that also buffered to root.
- `event_cache.go` (new) and `storage_impl.go`: the flat event map becomes a bucketed LRU keyed by `(label, seq bucket)` that evicts whole buckets past `maxEventCacheEvents`, with a new `trajectory.event_cache.evictions` counter.
- `storage_impl.go`: `nextBatchSize` steers the scan window toward `targetScanEvents` and `clampBatchSize` caps its seq span so a sparse stretch cannot grow the window into a dense one.
- `trajectory.go` and `storage_impl.go`: add `CheckpointMutex(key)` to the `Trajectory` interface, backed by a per-instance map of mutexes.
- `reducers/core/reduce.go`, `reduce_head.go`, `reducers/AGENTS.md`: embed `CanStartAtRoot` in `Checkpointable` and document the root-capable and one-reducer-per-deploy rules.
