# Environment variables

Every environment variable Odin reads. All are optional; each has a sensible default. CLI flags,
where they exist, override the variable.

| Variable | Read by | Default | Meaning |
|----------|---------|---------|---------|
| `ODIN_LOG` | `odin`, `odind` | `info` | Log level / [`EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) directive, e.g. `debug` or `odin_core::engine=debug,info`. See [observability](observability.md#levels). |
| `RUST_LOG` | `odin`, `odind` | — | Fallback for `ODIN_LOG` when it's unset (so a standard `RUST_LOG` works too). |
| `ODIN_WEBHOOK_SECRET` | `odind` | — | HMAC secret for verifying webhook signatures. Overridden by `--webhook-secret`. Required when a workflow declares a webhook trigger or an approval gate, unless `--webhook-allow-unsigned`. See [daemon](daemon.md#webhook-triggers). |
| `ODIN_RECIPES_DIR` | `odin`, `odind` | platform data-local dir (`directories::ProjectDirs`) `/recipes` | The recipe-catalog directory `odin run <name>` / `odind --recipes` resolve. Overridden by `--recipes-dir`. See [getting started](getting-started.md). |
| `ODIN_SHELL` | engine (`run:`/gate steps) | platform default (`sh` on POSIX, Git-Bash `sh` on Windows) | Override the POSIX shell used to run `run:` and gate commands. A blank value is treated as unset. |
| `ODIN_SQLITE_SYNCHRONOUS` | engine (SQLite store) | `NORMAL` | Set to `FULL` for the most conservative SQLite durability (slower writes); anything else is `NORMAL` (WAL + normal sync, the default). |

For OpenTelemetry/OTLP export, use the `odind --otlp-endpoint <url>` flag (built with `--features
otlp`), not an environment variable — see [observability](observability.md#opentelemetry--otlp-export).

> Test-only variables (`ODIN_LIVE_PROVIDER_TESTS`, etc.) are not part of the runtime surface and are
> documented in the relevant test modules.
