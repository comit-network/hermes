use anyhow::anyhow;
use anyhow::Result;
use time::macros::format_description;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::EnvFilter;

pub use tracing_subscriber::filter::LevelFilter;

pub fn init(level: LevelFilter, json_format: bool) -> Result<()> {
    if level == LevelFilter::OFF {
        return Ok(());
    }

    let is_terminal = atty::is(atty::Stream::Stderr);

    let filter = EnvFilter::from_default_env()
        .add_directive("info".parse()?)
        .add_directive("sqlx=warn".parse()?) // sqlx logs all queries on INFO
        .add_directive("bdk::blockchain::script_sync=off".parse()?) // bdk logs duration of sync on INFO
        .add_directive("bdk::wallet=off".parse()?) // bdk logs derivation of addresses on INFO
        .add_directive("_=off".parse()?) // rocket logs headers on INFO and uses `_` as the log target for it?
        .add_directive("rocket::launch=off".parse()?) // disable rocket startup logs
        .add_directive("rocket::launch_=off".parse()?) // disable rocket startup logs
        .add_directive(format!("taker={level}").parse()?)
        .add_directive(format!("maker={level}").parse()?)
        .add_directive(format!("daemon={level}").parse()?)
        .add_directive(format!("rocket={level}").parse()?);

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(is_terminal);

    let result = if json_format {
        builder.json().with_timer(UtcTime::rfc_3339()).try_init()
    } else {
        builder
            .compact()
            .with_timer(UtcTime::new(format_description!(
                "[year]-[month]-[day] [hour]:[minute]:[second]"
            )))
            .try_init()
    };

    result.map_err(|e| anyhow!("Failed to init logger: {e}"))?;

    tracing::info!("Initialized logger");

    Ok(())
}
