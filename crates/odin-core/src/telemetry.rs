//! Binary-side telemetry setup. The `odin-core` **library** only *emits* `tracing` spans and
//! events; a **binary** (`odin`, `odind`) calls [`init`] once at startup to install a global
//! subscriber. This module is gated behind the `telemetry` feature so a library embedder pays
//! nothing for the subscriber stack and chooses its own.
//!
//! The console layer is text by default, JSON with [`LogFormat::Json`]. The level filter is
//! read from `$ODIN_LOG`, then `$RUST_LOG`, defaulting to `info` (e.g. `ODIN_LOG=debug`, or
//! `ODIN_LOG=odin_core=debug,info` for per-target control). With the `otlp` feature,
//! [`Options::otlp_endpoint`] additionally exports spans to an OpenTelemetry/OTLP collector.

use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, registry};

/// Console output format for the fmt layer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// Human-readable, one line per event (the default).
    #[default]
    Text,
    /// One JSON object per line, for log aggregation.
    Json,
}

impl std::str::FromStr for LogFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "text" | "human" | "pretty" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(format!(
                "unknown log format {other:?} (expected `text` or `json`)"
            )),
        }
    }
}

/// Telemetry options, typically derived from a binary's CLI flags.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Output format for the console layer.
    pub format: LogFormat,
    /// OTLP collector endpoint (e.g. `http://localhost:4317`). Honored only when this crate is
    /// built with the `otlp` feature; otherwise it is ignored with a warning.
    pub otlp_endpoint: Option<String>,
}

/// A guard whose `Drop` flushes buffered telemetry (the OTLP batch exporter, if any). Hold it
/// for the whole process lifetime; dropping it before exit flushes in-flight spans.
#[must_use = "dropping the Guard tears telemetry down immediately; bind it for the process lifetime"]
#[derive(Default)]
pub struct Guard {
    #[cfg(feature = "otlp")]
    otlp: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(feature = "otlp")]
        if let Some(provider) = self.otlp.take() {
            // Flush and shut the exporter down so the last spans reach the collector.
            let _ = provider.shutdown();
        }
    }
}

/// Builds the level filter from `$ODIN_LOG`, then `$RUST_LOG`, defaulting to `info`.
fn env_filter() -> EnvFilter {
    std::env::var("ODIN_LOG")
        .or_else(|_| std::env::var("RUST_LOG"))
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map_or_else(|| EnvFilter::new("info"), EnvFilter::new)
}

/// Installs the global `tracing` subscriber from `opts`. Call once at startup. A second call is
/// a no-op (the global subscriber can be set only once). Returns a [`Guard`] to hold until exit.
pub fn init(opts: &Options) -> Guard {
    // Logs go to STDERR so a binary's stdout stays a clean data channel (the CLI prints run
    // summaries / `--json` there; mixing log lines in would corrupt piped output).
    let fmt_layer = match opts.format {
        LogFormat::Text => tracing_subscriber::fmt::layer()
            .with_writer(std::io::stderr)
            .boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_writer(std::io::stderr)
            .boxed(),
    };

    #[cfg(feature = "otlp")]
    let (otlp_layer, guard) = otlp::build(opts.otlp_endpoint.as_deref());
    #[cfg(not(feature = "otlp"))]
    let guard = Guard::default();

    let subscriber = registry().with(env_filter()).with(fmt_layer);
    #[cfg(feature = "otlp")]
    let subscriber = subscriber.with(otlp_layer);

    // `try_init` errors only if a subscriber is already set — fine to ignore (idempotent).
    let _ = subscriber.try_init();

    #[cfg(not(feature = "otlp"))]
    if opts.otlp_endpoint.is_some() {
        tracing::warn!(
            "--otlp-endpoint was set but this binary was built without the `otlp` feature; \
             ignoring (rebuild with `--features otlp`)"
        );
    }

    guard
}

#[cfg(feature = "otlp")]
mod otlp {
    use super::Guard;
    use opentelemetry::trace::TracerProvider as _;
    use opentelemetry_otlp::WithExportConfig as _;
    use tracing_subscriber::Layer;
    use tracing_subscriber::registry::LookupSpan;

    /// Builds an optional OpenTelemetry OTLP span-export layer and the guard that owns its
    /// provider. When `endpoint` is `None`, no exporter is built (the layer is `None`).
    pub(super) fn build<S>(
        endpoint: Option<&str>,
    ) -> (Option<Box<dyn Layer<S> + Send + Sync>>, Guard)
    where
        S: tracing::Subscriber + for<'a> LookupSpan<'a> + Send + Sync + 'static,
    {
        let Some(endpoint) = endpoint else {
            return (None, Guard::default());
        };
        let exporter = match opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()
        {
            Ok(e) => e,
            Err(e) => {
                // Don't crash the binary on a bad endpoint — log once console-side and skip.
                eprintln!("odin: OTLP exporter init failed ({e}); continuing without OTLP export");
                return (None, Guard::default());
            }
        };
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_batch_exporter(exporter)
            .with_resource(
                opentelemetry_sdk::Resource::builder()
                    .with_service_name("odin")
                    .build(),
            )
            .build();
        let tracer = provider.tracer("odin");
        let layer = tracing_opentelemetry::layer().with_tracer(tracer).boxed();
        (
            Some(layer),
            Guard {
                otlp: Some(provider),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::LogFormat;

    #[test]
    fn log_format_parses_case_insensitively() {
        assert_eq!("text".parse::<LogFormat>().unwrap(), LogFormat::Text);
        assert_eq!("  Text ".parse::<LogFormat>().unwrap(), LogFormat::Text);
        assert_eq!("JSON".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert_eq!(LogFormat::default(), LogFormat::Text);
        let err = "xml".parse::<LogFormat>().unwrap_err();
        assert!(err.contains("xml"), "error names the bad value: {err}");
    }
}
