mod audio;
mod config;
mod daemon;
mod db;
mod paths;
mod server;
mod state;

fn main() {
    // Initialise logging first so subsequent startup steps are visible.
    let cfg = config::Config::load();
    env_logger::Builder::from_default_env()
        .filter_level(parse_level(&cfg.logging.level))
        .format_timestamp_secs()
        .init();

    log::info!("Loaded configuration");
    log::debug!("Config: {:?}", cfg);

    paths::ensure_dirs();
    daemon::run(cfg);
}

fn parse_level(s: &str) -> log::LevelFilter {
    match s.to_lowercase().as_str() {
        "trace" => log::LevelFilter::Trace,
        "debug" => log::LevelFilter::Debug,
        "info" => log::LevelFilter::Info,
        "warn" | "warning" => log::LevelFilter::Warn,
        "error" => log::LevelFilter::Error,
        _ => log::LevelFilter::Info,
    }
}
