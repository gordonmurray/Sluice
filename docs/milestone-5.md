# Milestone 5 — observability

## What I built

Configuration only, no new Rust code.

- `otel-collector` (contrib image): receives OTLP from the facilitator. The
  spanmetrics connector turns its traces into Prometheus histograms, so
  settle latency needs no metrics code anywhere: traces in, histograms out.
- `prometheus`: scrapes the collector's exporter.
- `grafana` (:3001, anonymous viewer enabled, admin/admin): provisioned
  Prometheus and Postgres datasources and a `Sluice — payments` dashboard:
  - Revenue (USDC) and paid-requests stats, a revenue-by-caller table, and
    paid requests over time, straight from the `payments` table.
  - Settle latency p50/p95 via `histogram_quantile` over
    `traces_span_metrics_duration_milliseconds_bucket{span_name="POST /settle"}`.
  - Facilitator settle/verify request rate.
- Facilitator env: `OTEL_EXPORTER_OTLP_ENDPOINT` plus `OTEL_SERVICE_NAME`.

## How to run

```sh
docker compose up -d --build
docker compose run --rm client         # generate paid traffic
open http://localhost:3001/d/sluice-payments
```

## Verified output

Queried through Grafana's own `/api/ds/query`, the same path the panels use:

```
Postgres   -> revenue_usdc: 0.28, paid_requests: 8
Prometheus -> settle p95: 9750 ms
```

The dashboard uid `sluice-payments` is provisioned at startup; traffic from
`docker compose run --rm client` appears within one scrape interval.

## Review

Codex found no blockers and two solid catches, both applied: the PromQL now
pins `service_name="facilitator"` plus `span_kind="SPAN_KIND_SERVER"` (span
names alone would mix client and server spans, or future services), and
every Postgres panel respects the dashboard time range (the stats were
silently all-time). Noted for later: the `spanmetrics` connector key is
deprecated upstream in favour of `span_metrics` (the pinned collector
version accepts both), and first-boot panels can error briefly until
Postgres and Prometheus are ready.

## What surprised me

- **The facilitator exports OTLP over HTTP, not gRPC.** Pointing
  `OTEL_EXPORTER_OTLP_ENDPOINT` at the collector's 4317 (gRPC) port
  produced only `BatchSpanProcessor.ExportError: network error`; the fix is
  `:4318`.
- **The spanmetrics connector made settle latency free.** The facilitator
  emits spans, not metrics; rather than instrumenting anything, the
  collector derives duration histograms from the spans it already receives.
  The default flush interval (60 seconds) makes it look broken on first
  try, so set `metrics_flush_interval` low for local dev.
- **A settle p95 of 9.7 seconds is an artefact of the fork.** Anvil mines
  on a 5-second interval (`--block-time 5`, needed for chain-time
  tracking) and the facilitator waits for the receipt. On real Base
  (2-second blocks) this should be a few seconds; the panel exists
  precisely to watch that number.
