# Observability

Odin is instrumented with [`tracing`](https://docs.rs/tracing). The **library** (`odin-core`)
only *emits* spans and events; a **binary** installs the subscriber. The `odin` CLI and the
`odind` daemon install one for you; an embedder of `odin-core` installs its own (the library
never sets a global subscriber).

Logs go to **stderr**; a binary's **stdout** stays a clean data channel (the CLI prints run
summaries / `--json` there, so you can pipe it without log lines corrupting the output).

## Levels

The level filter is read from `$ODIN_LOG`, then `$RUST_LOG`, defaulting to `info`. It is a
standard [`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
directive, so you get per-target control:

```sh
ODIN_LOG=debug odind --workflows ./wf          # everything at debug
ODIN_LOG=odin_core::engine=debug,info odin run wf.yaml --repo .   # engine at debug, rest info
```

At `info` you see run lifecycle + per-step outcomes; at `debug` you also see each loaded
workflow, provider dispatch, and gate/judge detail.

## Format

`--log-format text` (default, human-readable) or `--log-format json` (one JSON object per line,
for an aggregator):

```sh
odind --workflows ./wf --log-format json | fluent-bit ...
```

## What's emitted

Spans nest, so every event carries its run/step context:

| Span | Fields | Where |
|------|--------|-------|
| `run` | `run_id`, `workflow`, `durable` | one per engine run; wraps all step work |
| `step` | `step`, `scratch` | one per step execution (duration) |
| `dispatch` | `source`, `workflow` | the daemon, one per trigger event (wraps the `run` span) |

Key events (all within the spans above): `run started` / `run finished` (`status`, `steps`,
`cost_micros`, `elapsed_ms`), `step finished` (`status`, `exit_code`, `attempts`), webhook
`delivery accepted` / `duplicate delivery ignored` / 503 retries, cron-trigger skips, resume
counts.

```text
2026-06-03T18:22:04Z  INFO run{run_id=7f3c… workflow=issue-to-pr durable=true}: run started
2026-06-03T18:22:09Z  INFO run{run_id=7f3c… …}: step finished step=plan status=Passed exit_code=0 attempts=1
2026-06-03T18:22:31Z  INFO run{run_id=7f3c… …}: run finished status=Succeeded steps=6 cost_micros=123400 elapsed_ms=27210
```

## OpenTelemetry / OTLP export

Build with the `otlp` feature, then point `--otlp-endpoint` at an OTLP collector (Jaeger,
Tempo, Grafana Agent, the OpenTelemetry Collector — anything speaking OTLP/gRPC on `:4317`):

```sh
cargo build -p odin-daemon --features otlp
odind --workflows ./wf --otlp-endpoint http://localhost:4317
```

Spans then appear as run/step trees with durations, so you can see where time goes, which
provider is slow, and which gate failed — alongside the console logs. The exporter is **opt-in
at compile time** (it pulls the OTel SDK + tonic/gRPC), so the default binary stays lean;
`--otlp-endpoint` without the feature is ignored with a warning. The daemon flushes in-flight
spans on shutdown.

> The `tracing` facade means the OTLP layer is additive — the console layer is unaffected, and
> an `odin-core` embedder can attach any other `tracing` layer (e.g. a custom metrics or
> sampling layer) the same way.

## Embedding

If you embed `odin-core`, install your own subscriber once at startup. The crate offers a
convenience under the `telemetry` feature (what the binaries use):

```rust
let _guard = odin_core::telemetry::init(&odin_core::telemetry::Options {
    format: odin_core::telemetry::LogFormat::Text,
    otlp_endpoint: None, // Some("http://localhost:4317") with the `otlp` feature
});
```

…or wire `tracing-subscriber` yourself — Odin's spans/events flow into whatever global
subscriber is installed.
