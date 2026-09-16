//! `fleets` — a fast, mostly compatible replacement for `ansible-inventory`.
//!
//! The supported product is the command-line application. The small Rust API exported here
//! is experimental during the `0.1.x` series and may change in `0.2.0`.

mod cli;
mod constructed;
mod enabled;
mod error;
mod expr;
mod hostrange;
mod ini;
mod literal_eval;
mod model;
mod output;
mod parse;
mod pattern;
mod sanitize;
mod script;
mod serialize;
mod telemetry;
mod vars;
mod vars_files;

pub use cli::{Args, run};
pub use error::{Error, Result};
pub use telemetry::{TelemetryGuard, init_telemetry};
