use cw_core::config::DataPaths;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The returned guard flushes the non-blocking writer when it drops, so the caller has to hold it
/// for as long as it wants file logs — `let _ = init(..)` would drop it at once and lose them.
#[must_use]
pub fn init(paths: &DataPaths) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let level = std::env::var("CONTEXTWITNESS_LOG")
        .ok()
        .and_then(|value| {
            value
                .parse::<tracing_subscriber::filter::LevelFilter>()
                .ok()
        })
        .unwrap_or(tracing_subscriber::filter::LevelFilter::INFO);
    let (file_layer, guard) = match appender(&paths.logs()) {
        Ok(appender) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            (
                Some(
                    tracing_subscriber::fmt::layer()
                        .with_ansi(false)
                        .with_writer(writer),
                ),
                Some(guard),
            )
        }
        Err(error) => {
            eprintln!("file logging is disabled: {error}");
            (None, None)
        }
    };

    tracing_subscriber::registry()
        .with(level)
        .with(file_layer)
        .with(tracing_subscriber::fmt::layer())
        .init();

    guard
}

fn appender(
    directory: &std::path::Path,
) -> Result<tracing_appender::rolling::RollingFileAppender, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(directory)?;

    // The builder rather than `rolling::daily`, which panics on a directory it cannot open.
    Ok(tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("contextwitness")
        .filename_suffix("log")
        .build(directory)?)
}
