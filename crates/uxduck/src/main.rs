//! The native executable: composition root only.

mod account;
mod adapters;
mod analytics;
mod api;
mod app_update;
mod build;
mod cli;
mod daemon;
mod desktop_app;
mod dev_api;
mod diagnostics;
mod edge;
mod errors;
mod feedback;
mod find;
mod firmware;
mod grants;
mod inspect;
mod interface;
mod live_video;
mod logs;
mod marker_sources;
mod mcp;
mod network;
mod online;
mod params;
mod parent;
mod paths;
mod places;
mod plugin_bundle;
mod ports;
mod receiver_stick;
mod remote_users;
mod remote_view;
mod service;
mod shipped;
mod sim;
mod spool;
mod startup;
mod store;
mod sync;
mod terrain;
mod tlog_index;
mod tray;
mod update;
mod uploader;
mod vehicle_files;
mod video;
mod wakes;
mod watch;

use std::process::ExitCode;

use clap::Parser;

fn main() -> ExitCode {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    // The copied updater is a short-lived installer worker, not another daemon.
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__install-update")) {
        let Some(dir) = std::env::args_os().nth(2) else {
            eprintln!("UXDuck update: missing staged update directory");
            return ExitCode::from(2);
        };
        return match app_update::worker(std::path::Path::new(&dir)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("UXDuck update: {error}");
                ExitCode::from(1)
            }
        };
    }
    let (console, _console_guard) = match logs::console_writer().and_then(logs::Tee::new) {
        Ok(console) => console,
        Err(error) => {
            startup::record(&format!("cannot start the console writer: {error}"));
            eprintln!("uxduck: cannot start the console writer: {error}");
            return ExitCode::from(2);
        }
    };
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with(tracing_subscriber::fmt::layer().with_writer(console))
        .with(errors::ErrorLayer)
        .init();
    errors::install_panic_hook();
    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let failed = error.use_stderr();
            if failed {
                startup::record(&error.to_string());
            }
            let _ = error.print();
            return if failed {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            };
        }
    };
    match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime.block_on(cli::run(cli)),
        Err(error) => {
            startup::record(&format!("cannot start the runtime: {error}"));
            eprintln!("uxduck: cannot start the runtime: {error}");
            ExitCode::from(2)
        }
    }
}
