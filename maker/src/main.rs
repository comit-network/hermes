use anyhow::Context;
use anyhow::Result;
use clap::Parser;
use clap::Subcommand;
use daemon::bdk;
use daemon::bdk::bitcoin;
use daemon::bdk::bitcoin::Amount;
use daemon::bdk::FeeRate;
use daemon::db;
use daemon::monitor;
use daemon::oracle;
use daemon::projection;
use daemon::seed::RandomSeed;
use daemon::seed::Seed;
use daemon::wallet;
use daemon::MakerActorSystem;
use daemon::HEARTBEAT_INTERVAL;
use daemon::N_PAYOUTS;
use model::cfd::Role;
use model::olivia;
use model::SETTLEMENT_INTERVAL;
use rocket::fairing::AdHoc;
use shared_bin::logger;
use shared_bin::logger::LevelFilter;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio_tasks::Tasks;
use xtra::Actor;
use xtras::supervisor;

mod routes;

#[derive(Parser)]
struct Opts {
    /// The port to listen on for p2p connections.
    #[clap(long, default_value = "9999")]
    p2p_port: u16,

    /// The IP address to listen on for the HTTP API.
    #[clap(long, default_value = "127.0.0.1:8001")]
    http_address: SocketAddr,

    /// Where to permanently store data, defaults to the current working directory.
    #[clap(long)]
    data_dir: Option<PathBuf>,

    /// If enabled logs will be in json format
    #[clap(short, long)]
    json: bool,

    /// Configure the log level, e.g.: one of Error, Warn, Info, Debug, Trace
    #[clap(short, long, default_value = "Debug")]
    log_level: LevelFilter,

    #[clap(subcommand)]
    network: Network,
}

#[derive(Parser)]
enum Network {
    /// Run on mainnet.
    Mainnet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long, default_value = "ssl://blockstream.info:700")]
        electrum: String,

        #[clap(subcommand)]
        withdraw: Option<Withdraw>,
    },
    /// Run on testnet.
    Testnet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long, default_value = "ssl://blockstream.info:993")]
        electrum: String,

        #[clap(subcommand)]
        withdraw: Option<Withdraw>,
    },
    /// Run on signet
    Signet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long)]
        electrum: String,

        #[clap(subcommand)]
        withdraw: Option<Withdraw>,
    },
}

#[derive(Subcommand)]
enum Withdraw {
    Withdraw {
        /// Optionally specify the amount of Bitcoin to be withdrawn. If not specified the wallet
        /// will be drained. Amount is to be specified with denomination, e.g. "0.1 BTC"
        #[clap(long)]
        amount: Option<Amount>,
        /// Optionally specify the fee-rate for the transaction. The fee-rate is specified as sats
        /// per vbyte, e.g. 5.0
        #[clap(long)]
        fee: Option<f32>,
        /// The address to receive the Bitcoin.
        #[clap(long)]
        address: bdk::bitcoin::Address,
    },
}

impl Network {
    fn electrum(&self) -> &str {
        match self {
            Network::Mainnet { electrum, .. } => electrum,
            Network::Testnet { electrum, .. } => electrum,
            Network::Signet { electrum, .. } => electrum,
        }
    }

    fn bitcoin_network(&self) -> bitcoin::Network {
        match self {
            Network::Mainnet { .. } => bitcoin::Network::Bitcoin,
            Network::Testnet { .. } => bitcoin::Network::Testnet,
            Network::Signet { .. } => bitcoin::Network::Signet,
        }
    }

    fn data_dir(&self, base: PathBuf) -> PathBuf {
        match self {
            Network::Mainnet { .. } => base.join("mainnet"),
            Network::Testnet { .. } => base.join("testnet"),
            Network::Signet { .. } => base.join("signet"),
        }
    }

    fn withdraw(&self) -> &Option<Withdraw> {
        match self {
            Network::Mainnet { withdraw, .. } => withdraw,
            Network::Testnet { withdraw, .. } => withdraw,
            Network::Signet { withdraw, .. } => withdraw,
        }
    }
}

#[rocket::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();

    logger::init(opts.log_level, opts.json).context("initialize logger")?;
    tracing::info!("Running version: {}", env!("VERGEN_GIT_SEMVER_LIGHTWEIGHT"));
    let settlement_interval_hours = SETTLEMENT_INTERVAL.whole_hours();

    tracing::info!(
        "CFDs created with this release will settle after {settlement_interval_hours} hours"
    );

    let data_dir = opts
        .data_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("unable to get cwd"));

    let data_dir = opts.network.data_dir(data_dir);

    if !data_dir.exists() {
        tokio::fs::create_dir_all(&data_dir).await?;
    }

    let seed = RandomSeed::initialize(&data_dir.join("maker_seed")).await?;

    let bitcoin_network = opts.network.bitcoin_network();
    let ext_priv_key = seed.derive_extended_priv_key(bitcoin_network)?;

    let mut tasks = Tasks::default();

    let (wallet, wallet_feed_receiver) = wallet::Actor::new(opts.network.electrum(), ext_priv_key)?;

    let (wallet, wallet_fut) = wallet.create(None).run();
    tasks.add(wallet_fut);

    if let Some(Withdraw::Withdraw {
        amount,
        address,
        fee,
    }) = opts.network.withdraw()
    {
        wallet
            .send(wallet::Withdraw {
                amount: *amount,
                address: address.clone(),
                fee: fee.map(FeeRate::from_sat_per_vb),
            })
            .await??;

        return Ok(());
    }

    let auth_username = rocket_basicauth::Username("itchysats");
    let auth_password = seed.derive_auth_password::<rocket_basicauth::Password>();

    let (identity_pk, identity_sk) = seed.derive_identity();

    let hex_pk = hex::encode(identity_pk.to_bytes());
    tracing::info!(
        "Authentication details: username='{auth_username}' password='{auth_password}', noise_public_key='{hex_pk}'",
    );

    let figment = rocket::Config::figment()
        .merge(("address", opts.http_address.ip()))
        .merge(("port", opts.http_address.port()))
        .merge(("cli_colors", false));

    let p2p_port = opts.p2p_port;
    let p2p_socket = format!("0.0.0.0:{p2p_port}").parse::<SocketAddr>().unwrap();

    let db = db::connect(data_dir.join("maker.sqlite")).await?;

    // Create actors

    let (projection_actor, projection_context) = xtra::Context::new(None);

    let maker = MakerActorSystem::new(
        db.clone(),
        wallet.clone(),
        *olivia::PUBLIC_KEY,
        |executor| oracle::Actor::new(db.clone(), executor, SETTLEMENT_INTERVAL),
        {
            |executor| {
                let electrum = opts.network.electrum().to_string();
                monitor::Actor::new(db.clone(), electrum, executor)
            }
        },
        SETTLEMENT_INTERVAL,
        N_PAYOUTS,
        projection_actor.clone(),
        identity_sk,
        HEARTBEAT_INTERVAL,
        p2p_socket,
    )?;

    let (supervisor, price_feed) =
        supervisor::Actor::with_policy(xtra_bitmex_price_feed::Actor::default, |e| match e {
            xtra_bitmex_price_feed::Error::FailedToParseQuote { .. }
            | xtra_bitmex_price_feed::Error::Failed { .. }
            | xtra_bitmex_price_feed::Error::FailedToConnect { .. }
            | xtra_bitmex_price_feed::Error::Unspecified
            | xtra_bitmex_price_feed::Error::StreamEnded => true, // always restart price feed actor
        });

    let (_supervisor_address, task) = supervisor.create(None).run();
    tasks.add(task);

    let (proj_actor, projection_feeds) =
        projection::Actor::new(db.clone(), Role::Maker, bitcoin_network, &price_feed);
    tasks.add(projection_context.run(proj_actor));

    rocket::custom(figment)
        .manage(projection_feeds)
        .manage(wallet_feed_receiver)
        .manage(maker)
        .manage(auth_username)
        .manage(auth_password)
        .manage(bitcoin_network)
        .mount(
            "/api",
            rocket::routes![
                routes::maker_feed,
                routes::post_sell_order,
                routes::post_cfd_action,
                routes::get_health_check,
                routes::post_withdraw_request,
                routes::get_cfds,
                routes::get_takers,
            ],
        )
        .register("/api", rocket::catchers![rocket_basicauth::unauthorized])
        .mount("/", rocket::routes![routes::dist, routes::index])
        .register("/", rocket::catchers![rocket_basicauth::unauthorized])
        .attach(AdHoc::on_liftoff("Log launch", |rocket| {
            Box::pin(async move {
                let http_endpoint = format!(
                    "http://{}:{}",
                    rocket.config().address,
                    rocket.config().port
                );

                tracing::info!(endpoint = %http_endpoint, "HTTP interface is ready");
            })
        }))
        .launch()
        .await?;

    db.close().await;

    Ok(())
}
