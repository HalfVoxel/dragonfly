PR #38640 — feat: lov user ff is now faster and supports filtering

# Summary

- `lov user ff` now runs in ~1.3s (down from ~2.7s) and accepts an optional filter argument.
- Filter does a case-insensitive substring match against flag names and sub-field names in the resolved value.

# Why

- Previous version called `clisecrets.LoadAndInject(secrets.ServiceGoAPI)`, pulling all 409 go-api secrets just to read three Confidence credentials.
- Slow steps (Firebase lookups, WASM provider build, flag listing) ran serially.
- Browsing the full flag list was painful when looking for a specific flag.

# What changed

- Fetch only the three Confidence secrets actually needed (`CONFIDENCE_CLIENT_SECRET`, `CONFIDENCE_ADMIN_CLIENT_ID`, `CONFIDENCE_ADMIN_CLIENT_SECRET`).
- Run prod/dev Firebase lookups, WASM provider setup, and flag-name listing concurrently via `errgroup`.
- Add optional `[filter]` positional arg; filtered flags display regardless of enabled state.
- Disambiguate single-arg form: treat it as a user if it parses as an email or 28-char alphanumeric Firebase UID, otherwise as a filter.

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
