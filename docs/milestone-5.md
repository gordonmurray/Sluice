# Milestone 5 — observability

## What was built

Configuration only, as CLAUDE.md predicted — zero new Rust code.

- `otel-collector` (contrib image): receives OTLP from the facilitator. The
  **spanmetrics connector** turns its traces into Prometheus histograms, so
  settle latency needs no metrics code anywhere: traces in, histograms out.
- `prometheus`: scrapes the collector's exporter.
- `grafana` (:3001, anonymous viewer enabled, admin/admin): provisioned
  Prometheus + Postgres datasources and a `Sluice — payments` dashboard:
  - Revenue (USDC) and paid-requests stats, revenue-by-caller table, and
    paid-requests-over-time — straight from the milestone-4 `payments` table.
  - Settle latency p50/p95 — `histogram_quantile` over
    `traces_span_metrics_duration_milliseconds_bucket{span_name="POST /settle"}`.
  - Facilitator settle/verify request rate.
- Facilitator env: `OTEL_EXPORTER_OTLP_ENDPOINT` + `OTEL_SERVICE_NAME`.

## How to run

```sh
docker compose up -d --build
docker compose run --rm client         # generate paid traffic
open http://localhost:3001/d/sluice-payments
```

## Verified output

Queried through Grafana's own `/api/ds/query` (same path the panels use):

```
Postgres  -> revenue_usdc: 0.28, paid_requests: 8
Prometheus -> settle p95: 9750 ms
```

The dashboard uid `sluice-payments` is provisioned at startup; traffic from
`docker compose run --rm client` appears within one scrape interval.

## Review

Codex found no blockers; applied its two solid catches: the PromQL now pins
`service_name="facilitator"` + `span_kind="SPAN_KIND_SERVER"` (span names
alone would mix client/server spans or future services), and every Postgres
panel respects the dashboard time range (the stats were silently all-time).
Noted for later: the `spanmetrics` connector key is deprecated upstream in
favor of `span_metrics` (the pinned collector version accepts both), and
first-boot panels can error briefly until Postgres/Prometheus are ready.

## What surprised me

- **The facilitator exports OTLP over HTTP, not gRPC.** Pointing
  `OTEL_EXPORTER_OTLP_ENDPOINT` at the collector's 4317 (gRPC) port produced
  only `BatchSpanProcessor.ExportError: network error` — the fix is `:4318`.
- **The spanmetrics connector made "settle latency" free.** The facilitator
  emits spans, not metrics; rather than instrumenting anything, the collector
  derives duration histograms from the spans it already receives. The default
  flush interval (60s) makes it look broken on first try — set
  `metrics_flush_interval` low for local dev.
- **Settle p95 of ~9.7s is an artifact of the fork.** Anvil mines on a 5s
  interval (`--block-time 5`, needed for chain-time tracking), and the
  facilitator waits for the receipt. On real Base (2s blocks) this should be
  a few seconds; the panel exists precisely to watch that number.
