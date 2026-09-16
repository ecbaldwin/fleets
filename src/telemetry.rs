//! Optional OpenTelemetry tracing, exported to an OTLP/HTTP collector (e.g. Jaeger).
//!
//! Tracing is **off** unless a config file enables it, so the normal fast path pays nothing
//! (with no subscriber installed, `tracing` spans compile down to near no-ops). Config lives
//! at `$XDG_CONFIG_HOME/fleets/config.toml` (default `~/.config/fleets/config.toml`):
//!
//! ```toml
//! [tracing]
//! enabled = true
//! endpoint = "http://localhost:4318"   # OTLP/HTTP base URL of the collector
//! service_name = "fleets"
//! ```
//!
//! A missing or malformed config disables tracing (a malformed one warns on stderr). When
//! enabled but the collector is unreachable, spans are simply dropped — fleets never blocks
//! on or fails because of telemetry.

#[cfg(feature = "telemetry")]
use std::path::PathBuf;

#[cfg(feature = "telemetry")]
use opentelemetry::trace::TracerProvider as _;
#[cfg(feature = "telemetry")]
use opentelemetry_otlp::WithExportConfig as _;
#[cfg(feature = "telemetry")]
use opentelemetry_sdk::Resource;
#[cfg(feature = "telemetry")]
use opentelemetry_sdk::trace::SdkTracerProvider;
#[cfg(feature = "telemetry")]
use serde::Deserialize;
#[cfg(feature = "telemetry")]
use tracing_subscriber::layer::SubscriberExt;
#[cfg(feature = "telemetry")]
use tracing_subscriber::util::SubscriberInitExt;

#[cfg(feature = "telemetry")]
#[derive(Debug, Default, Deserialize)]
struct FileConfig {
    #[serde(default)]
    tracing: TracingConfig,
}

#[cfg(feature = "telemetry")]
#[derive(Debug, Deserialize)]
struct TracingConfig {
    #[serde(default)]
    enabled: bool,
    #[serde(default = "default_endpoint")]
    endpoint: String,
    #[serde(default = "default_service_name")]
    service_name: String,
}

#[cfg(feature = "telemetry")]
impl Default for TracingConfig {
    fn default() -> Self {
        TracingConfig {
            enabled: false,
            endpoint: default_endpoint(),
            service_name: default_service_name(),
        }
    }
}

#[cfg(feature = "telemetry")]
fn default_endpoint() -> String {
    "http://localhost:4318".to_string()
}

#[cfg(feature = "telemetry")]
fn default_service_name() -> String {
    "fleets".to_string()
}

/// Held for the lifetime of the program; flushes pending spans on
/// [`TelemetryGuard::shutdown`]. This is a no-op when the `telemetry` feature is disabled.
#[cfg(feature = "telemetry")]
pub struct TelemetryGuard {
    provider: Option<SdkTracerProvider>,
}

#[cfg(feature = "telemetry")]
impl TelemetryGuard {
    /// Flush and tear down the exporter. Call this explicitly before `process::exit`, which
    /// does not run destructors.
    pub fn shutdown(self) {
        if let Some(provider) = self.provider {
            let _ = provider.shutdown();
        }
    }
}

#[cfg(feature = "telemetry")]
fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("fleets").join("config.toml"))
}

/// Initialize tracing when the feature and configuration enable it.
#[cfg(feature = "telemetry")]
pub fn init_telemetry() -> TelemetryGuard {
    let Some(path) = config_path() else {
        return TelemetryGuard::disabled();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return TelemetryGuard::disabled();
    };
    let cfg: FileConfig = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("fleets: ignoring tracing config {}: {e}", path.display());
            return TelemetryGuard::disabled();
        }
    };
    if !cfg.tracing.enabled {
        return TelemetryGuard::disabled();
    }

    // OTLP/HTTP wants the per-signal URL; accept a base endpoint for convenience.
    let endpoint = {
        let base = cfg.tracing.endpoint.trim_end_matches('/');
        if base.ends_with("/v1/traces") {
            base.to_string()
        } else {
            format!("{base}/v1/traces")
        }
    };

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .build()
    {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fleets: tracing disabled (exporter init failed): {e}");
            return TelemetryGuard::disabled();
        }
    };

    let resource = Resource::builder()
        .with_service_name(cfg.tracing.service_name)
        .build();

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("fleets");
    let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

    if tracing_subscriber::registry()
        .with(otel_layer)
        .try_init()
        .is_err()
    {
        // A subscriber is already installed; don't fight it.
        let _ = provider.shutdown();
        return TelemetryGuard::disabled();
    }

    TelemetryGuard {
        provider: Some(provider),
    }
}

#[cfg(feature = "telemetry")]
impl TelemetryGuard {
    fn disabled() -> Self {
        Self { provider: None }
    }
}

#[cfg(not(feature = "telemetry"))]
/// No-op tracing guard used by builds without the `telemetry` feature.
pub struct TelemetryGuard;

#[cfg(not(feature = "telemetry"))]
impl TelemetryGuard {
    /// No-op counterpart to the telemetry-enabled shutdown operation.
    pub fn shutdown(self) {}
}

#[cfg(not(feature = "telemetry"))]
/// Return a no-op guard when telemetry support is not compiled in.
pub fn init_telemetry() -> TelemetryGuard {
    TelemetryGuard
}
