# Grafana Dashboard Reference (lovable.grafana.net)

Reference for rendering panels from selected production dashboards as PNGs.

## When to use this

Use this when you want to embed a chart in a PR description, a comment, or
include one in your own analysis. Common cases:

- Bug fix → render the panel that *shows* the bug (error rate, latency spike, panic count) before the fix lands.
- Performance / metric improvement → render the baseline panel.
- Investigating a hot path or anomaly while reviewing → grab the relevant timeseries.

`GRAFANA_TOKEN` and `GRAFANA_HOST` are already exported in this environment by
push-and-check — you do **not** need to set them yourself, just `curl` directly.

## Rendering a panel as PNG

### Convert a panel URL to a render URL

Given a panel URL like:

```
https://lovable.grafana.net/d/arnnnzn/agent-tool-invocations
  ?orgId=1
  &from=2026-04-24T10:09:47.782Z&to=2026-04-25T10:09:47.782Z
  &var-routing_decision=$__all
  &viewPanel=panel-2
```

Replace `/d/` with `/render/d-solo/`, drop `viewPanel=`, and add `panelId`, `width`, `height`, `tz`:

```
https://lovable.grafana.net/render/d-solo/arnnnzn/agent-tool-invocations
  ?orgId=1
  &from=2026-04-24T10:09:47.782Z&to=2026-04-25T10:09:47.782Z
  &panelId=2
  &width=1600&height=800&tz=UTC
  &var-routing_decision=$__all
```

> Pass `panelId=<numeric>`, not the `panel-2` form from `viewPanel`.

### curl example

```bash
curl -sS -H "Authorization: Bearer $GRAFANA_TOKEN" \
  -o /tmp/panel.png -w "HTTP %{http_code} bytes=%{size_download}\n" \
  -G "$GRAFANA_HOST/render/d-solo/arnnnzn/agent-tool-invocations" \
  --data-urlencode "orgId=1" \
  --data-urlencode "from=now-24h" \
  --data-urlencode "to=now" \
  --data-urlencode "panelId=2" \
  --data-urlencode "width=1600" \
  --data-urlencode "height=800" \
  --data-urlencode "tz=UTC" \
  --data-urlencode "var-routing_decision=\$__all" \
  --data-urlencode "var-abnormal_tool_name=\$__all" \
  --data-urlencode "var-Filters=tool_name|=|plan--create"
```

The response is `image/png`. Save to `/tmp/panel.png` (or any path under `/tmp`).

### Embedding the rendered PNG in a PR

Use the `gh image` extension to upload the PNG directly and get back a markdown
reference you can paste into the PR body or comment:

```bash
gh image /tmp/panel.png
# prints something like: ![panel.png](https://github.com/user-attachments/assets/...)
```

It pulls the session token out of your browser cookies automatically — no
flags or env vars needed. The repo is inferred from the git remote of the
current directory (override with `--repo owner/repo` if you're not in the
target repo).

You can pass multiple paths in one call to upload them in order. Capture the
output and embed the markdown line in the PR body via
`gh pr edit <number> --body-file ...`.

Do **not** put the raw `/render/d-solo/...` URL in a PR body — it requires the
service-account token to load and won't render for anyone else.

### Sizing rules of thumb

| Panel kind | Suggested size |
|---|---|
| timeseries, default `w=12` | `width=1600 height=800` |
| timeseries, `w=24` | `width=1800 height=800` |
| stat | `width=400 height=200` |
| table (h≥10) | `width=1800 height=1000` |
| logs panel | `width=1800 height=900` |

### Time params

ISO-8601 with `Z` is safest (`2026-04-25T10:00:00Z`). Relative (`now-24h`, `now-6h`) works too. Always pin `tz=UTC` so axes don't drift with viewer timezone.

### Variables

- Single-value: `--data-urlencode "var-name=value"`.
- Multi-value: repeat the flag (`--data-urlencode "var-tool_name=foo" --data-urlencode "var-tool_name=bar"`).
- "All": `--data-urlencode "var-name=\$__all"` (escape `$` in shells).
- Adhoc filters (Prometheus): `var-Filters=<label>|<op>|<value>` — operators `=`, `!=`, `=~`, `!~`. Adhoc filters apply to every panel on the dashboard targeting the same datasource as the adhoc variable, so this is often the cleanest way to slice.

### Permissions

The service-account token has **Viewer + Renderer** scopes — enough to render
existing panels via `/render/d-solo`. It cannot create dashboards or snapshots,
so ad-hoc PromQL not present in any saved panel cannot be rendered through
Grafana itself — query the datasource directly
(`/api/datasources/proxy/uid/<uid>/api/v1/query_range`) and plot the JSON
locally if you need a custom chart.

---

## Dashboards

### 1. `arwvh2v` — Agent Tool Trace List
*"Trace list for tool calls."* Default time: `now-6h → now`.

**Variables**

| Name | Type | Format / Values |
|---|---|---|
| `tool_name` | query, multi, includeAll | label values of `tool_invocation_total.tool_name`. Use exact strings (`code--view`, `plan--create`). |
| `status_type` | custom, single | One of `all`, `success`, `hard_error`, `soft_error`. |
| `routing_decision` | query, single, includeAll | label values of `tool_invocation_total.routing_decision`. |
| `message_filter` | textbox | Free text matched against the trace list. |
| `project_filter` | textbox | UUID / free text. |

**Panels**

| ID | Title | DS | Notes |
|---|---|---|---|
| 16 | Tool Name (stat) | mixed | Echo of selected `tool_name`. |
| 15 | Team (stat) | prom | Owning team for `tool_name`. |
| 10 | Tool Invocations (timeseries) | prom | `sum(rate(tool_invocation_total{tool_name=~…}[25m]))*3600`. |
| 11 | Tool Invocation Errors `${tool_name}` (timeseries) | prom | Hard + soft error % for selected tool. |
| 12 | Tool Execution Duration (timeseries) | prom | p50/p90/p99 of `tool_invocation_duration_bucket` (success only). |
| 13 | Top 5 Tool Warnings (table) | clickhouse-observability | Top `tool.warning_id` from `traces`. |
| 14 | Agent Tool Traces (table, h=50) | clickhouse-observability | Detailed span list. Render at `width=1800 height=1200`. |

---

### 2. `arnnnzn` — Agent Tool Invocations
No description. Default time: `now-6h → now`.

**Variables**

| Name | Type | Format |
|---|---|---|
| `Filters` | adhoc (Prometheus) | `var-Filters=tool_name|=|plan--create`. Applies to every Prom panel — the cleanest filter. |
| `routing_decision` | query, single, includeAll | label values of `tool_invocation_total.routing_decision`. |
| `abnormal_tool_name` | query, multi, includeAll | Auto-computed list of "abnormally erroring" tools; only meaningful for panel 14. |

**Panels**

| ID | Title | DS | Notes |
|---|---|---|---|
| 2 | Tool Invocations per Hour (timeseries, h=13 w=24) | prom | `3600 * sum by (tool_name) rate(tool_invocation_total{routing_decision=~…}[$__rate_interval])`. |
| 14 | `$abnormal_tool_name` (timeseries, h=16 w=24) | prom | Soft error rate vs historical baseline. Needs single tool via `var-abnormal_tool_name=…`. |
| 6 | Tool Invocation Errors (timeseries) | prom | Hard error % per tool. |
| 10 | Top Tools by Hard Error Rate (table) | prom | Ranked over `$__range`. |
| 3 | Tool Invocation Soft Errors (timeseries) | prom | Soft error % per tool. |
| 9 | Top Tools by Soft Error Rate (table) | prom | Ranked over `$__range`. |
| 15 | Tool Invocation Soft Errors (total) (timeseries) | prom | Soft error % aggregated. |
| 8 | Top Missing Tools (table) | tempo `de12wyhkjtx4wb` | TraceQL: `agent.tool.prepare && warning_id="no_such_tool"`. |
| 7 | Missing Tool Invocations per Hour (timeseries) | prom | `3600 * sum(rate(tool_missing_total{routing_decision=~…}[$__rate_interval]))`. |

> Row/text panels (5, 16, 17, 18) skipped — no data.

---

### 3. `advalsr2uklj4b` — Errors
No description. Default time: `now-1h → now`. Refresh 1m.

**Variables**

| Name | Type | Format |
|---|---|---|
| `Filters` | adhoc (datasource `besp5zmppatc0d` — googlecloud-logging) | Applies to GCP-logging panels only; most panels here use ClickHouse and ignore it. |

**Panels** (rows 56, 57, 58, 59, 60, 61 omitted)

| ID | Title | DS | Notes |
|---|---|---|---|
| 34 | Unexpected errors (timeseries, h=13 w=19) | clickhouse-observability | Expensive query — keep window ≤1h. |
| 30 | % successful /chat requests (stat) | prom | `100 - failed/(succ+failed)*100` over `$__range`. |
| 41 | % of chat requests failing (timeseries) | prom | 2m / 5m / 15m rolling fail-rates of `user_chat_message_*`. |
| 53 | Provider's share of errors (table) | clickhouse-observability | Aggregates over `otel.logs`. |
| 39 | Panics in Go over time (timeseries) | clickhouse-observability | Count from `otel.logs` panic logs. |
| 27 | Panics in Go (logs panel) | clickhouse-observability | Render `width=1800 height=600`. |
| 28 | Unexpected errors (table, h=13 w=24) | clickhouse-observability | Wide aggregated error table. |
| 54 | Chat errors and teams (timeseries) | prom | `api_unexpected_errors_total` per team / `agent.MessageSentV1` rate. |
| 44 | Possible graph deadlocks (timeseries) | prom | `graph_hard_timeout_total` by node. |
| 55 | 🔍 Chat Errors by Type (sampled) (timeseries) | clickhouse-observability | Body breakdown of `chat.go` errors. |
| 43 | % builds that fail (timeseries) | prom | `build_result_success_total{success="false"}` / total. |
| 37 | Other issues (table, h=13 w=24) | clickhouse-observability | Aggregated severity report. |
| 38 | Go status 5xx+panic per endpoint (table) | prom | `api_durations_endpoint_seconds_count{status=~"5..|0"}`. |
| 40 | 5xx+panic per endpoint (table) | clickhouse-observability | ClickHouse complement to 38. |
| 46 | Panic per endpoint (table) | clickhouse-observability | Status=0 in `canonical_api_requests`. |
| 25 | Go 5xx + Panics (logs) | clickhouse-observability | Render `width=1800 height=900`. |
| 49 | Go status 400 per endpoint (timeseries) | prom | `api_durations_endpoint_seconds_count{status=~"4.."}`. |

---

### 4. `be9vjer5ubu9sf` — Node latencies
Single panel. Default time: `now-1h → now`.

**Variables**

| Name | Type | Format |
|---|---|---|
| `node` | query, multi, includeAll | label values of `api_durations_node_seconds_bucket.node`. Multi via repeated `var-node=` params. |

**Panels**

| ID | Title | DS | Notes |
|---|---|---|---|
| 1 | `$node` (timeseries, h=8 w=24) | prom | p50/p90/p95/p99 from `histogram_quantile(... rate(api_durations_node_seconds_bucket{node=~"[[node]]"}[$__rate_interval]))`. Legacy `[[node]]` syntax; multi-select still works through `var-node=`. |

---

### 5. `ar2sgxl` — Backend Performance
No description. Default time: `now-6h → now`. **No variables.**

**Panels**

| ID | Title | DS | Notes |
|---|---|---|---|
| 3 | Total API CPU Usage (timeseries) | grafanacloud-profiles (Pyroscope) | Renders, but may be sparse without a saved Pyroscope query. |
| 1 | Middleware Duration (timeseries) | prom | Avg duration from request received → handler entry. |
| 6 | Total API CPU Usage (timeseries) | prom | `sum(rate(container_cpu_usage_seconds_total{pod=~"go-api.*"}[5m]))`. |
| 2 | Sandbox Pod Memory Usage (timeseries) | prom | min/avg/max `system_memory_utilization_percent{service_name="sandbox"}`. |
| 4 | Average Mutex Delay (timeseries) | grafanacloud-profiles | Pyroscope. |
| 5 | API Pod Count (timeseries) | prom | HPA desired/current/min/max replicas for `go-api-hpa`. |
| 7 | Top Span Producers (timeseries, h=14 w=24) | tempo `de12wyhkjtx4wb` | TraceQL `{} | rate() by(name) | topk(15)`. |

---

## Datasource UID cheatsheet

| UID | Type | Display name |
|---|---|---|
| `grafanacloud-prom` | Prometheus | grafanacloud-lovable-prom |
| `cf1sydisy6b5sb` | ClickHouse | clickhouse-observability |
| `de12wyhkjtx4wb` | Tempo | (traces) |
| `besp5zmppatc0d` | GCP Logging | googlecloud-logging-datasource |
| `grafanacloud-profiles` | Pyroscope | grafanacloud-lovable-profiles |
