use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;

pub fn init() {
    let filter = EnvFilter::try_from_env("OXIEDRAW_LOG")
        .or_else(|_| EnvFilter::try_new("info,oxiedraw=debug"))
        .expect("static log filter is valid");

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(true))
        .init();
}
