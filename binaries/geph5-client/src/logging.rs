use arc_writer::ArcWriter;
use once_cell::sync::Lazy;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// In-memory buffer for JSON formatted logs
static JSON_LOGS: Lazy<ArcWriter<Vec<u8>>> = Lazy::new(|| ArcWriter::new(Vec::new()));

/// Initialize the tracing subscribers for logging
pub fn init_logging() -> anyhow::Result<()> {
    // Create a JSON writer that writes to our in-memory buffer
    let json_writer = JSON_LOGS.clone();

    tracing_subscriber::registry()
        // Standard logs to stderr (for console display)
        .with(fmt::layer().compact().with_writer(std::io::stderr))
        // Text logs to the LOGS global buffer
        .with(fmt::layer().json().with_writer(move || json_writer.clone()))
        // Set filtering based on environment or defaults
        .with(
            EnvFilter::builder()
                .with_default_directive("geph=debug".parse()?)
                .from_env_lossy(),
        )
        .init();

    Ok(())
}

/// Get the current JSON logs as a String
pub fn get_json_logs() -> String {
    let logs = JSON_LOGS.lock();

    // Convert the byte buffer to a string
    let log_string = String::from_utf8_lossy(&logs);

    log_string.to_string()
}
