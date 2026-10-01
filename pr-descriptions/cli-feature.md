PR #38640 — feat: lov user ff is now faster and supports filtering

# Summary

`lov user ff` was slow and gave no way to search for a flag. It is now about twice as fast and takes an optional filter.

`lov user ff` shows which feature flags are enabled for a user. It took ~2.7s, because it loaded far more secrets than it needed and ran its slow lookups one after another. Finding one specific flag in the full list was also tedious.

# Changes

- `lov user ff` now runs in ~1.3s, down from ~2.7s.
- An optional filter argument matches flag names and sub-field names in the resolved value, case-insensitively.

# Example

```sh
# Enabled feature flags for the current user
lov user ff

# Enabled feature flags for the given user
lov user ff user@example.com

# Enabled feature flags for the current user containing "trajectory" in its name
lov user ff trajectory

# Enabled feature flags for the given user containing "trajectory" in its name
lov user ff user@example.com trajectory

# Enabled feature flags for the given user id
lov user ff aBcDeFgHiJkLmNoPqRsTuVwXyZ12

# All feature flags (including disabled) for the current user
lov user ff --all
```

# Low level

The command called `clisecrets.LoadAndInject(secrets.ServiceGoAPI)`, pulling all 409 go-api secrets just to read three Confidence credentials. The Firebase lookups, WASM provider build and flag listing then ran serially.

## Changes

- Fetch only the three Confidence secrets actually needed (`CONFIDENCE_CLIENT_SECRET`, `CONFIDENCE_ADMIN_CLIENT_ID`, `CONFIDENCE_ADMIN_CLIENT_SECRET`).
- Run prod/dev Firebase lookups, WASM provider setup, and flag-name listing concurrently via `errgroup`.
- Add optional `[filter]` positional arg; filtered flags display regardless of enabled state.
- Disambiguate single-arg form: treat it as a user if it parses as an email or 28-char alphanumeric Firebase UID, otherwise as a filter.
