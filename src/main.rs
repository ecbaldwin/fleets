use clap::Parser;
use std::io::{self, Write};

use fleets::{Args, init_telemetry, run};

fn main() {
    let args = Args::parse();
    // Tracing is a no-op unless enabled in ~/.config/fleets/config.toml.
    let trace_guard = init_telemetry();

    let code = match run(&args) {
        Ok(out) => match writeln!(io::stdout().lock(), "{out}") {
            Ok(()) => 0,
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => 0,
            Err(e) => {
                eprintln!("fleets: writing output: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("fleets: {e:#}");
            1
        }
    };

    // Flush spans before exiting; process::exit does not run destructors.
    trace_guard.shutdown();
    std::process::exit(code);
}
