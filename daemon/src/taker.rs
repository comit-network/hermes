use anyhow::{Context, Result};
use bdk::bitcoin;
use bdk::bitcoin::secp256k1::schnorrsig;
use clap::Clap;
use daemon::db::{self};

use daemon::model::WalletInfo;

use daemon::seed::Seed;
use daemon::{
    bitmex_price_feed, connection, housekeeping, logger, monitor, oracle, taker_cfd, wallet,
    wallet_sync, TakerActorSystem,
};

use sqlx::sqlite::SqliteConnectOptions;
use sqlx::SqlitePool;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;

use tokio::sync::watch;
use tracing_subscriber::filter::LevelFilter;
use xtra::prelude::MessageChannel;
use xtra::spawn::TokioGlobalSpawnExt;
use xtra::Actor;

mod routes_taker;

pub const TERM: time::Duration = time::Duration::hours(24);

#[derive(Clap)]
struct Opts {
    /// The IP address of the other party (i.e. the maker).
    #[clap(long, default_value = "127.0.0.1:9999")]
    maker: SocketAddr,

    /// The IP address to listen on for the HTTP API.
    #[clap(long, default_value = "127.0.0.1:8000")]
    http_address: SocketAddr,

    /// Where to permanently store data, defaults to the current working directory.
    #[clap(long)]
    data_dir: Option<PathBuf>,

    /// Generate a seed file within the data directory.
    #[clap(long)]
    generate_seed: bool,

    /// If enabled logs will be in json format
    #[clap(short, long)]
    json: bool,

    #[clap(subcommand)]
    network: Network,
}

#[derive(Clap)]
enum Network {
    Mainnet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long, default_value = "ssl://electrum.blockstream.info:50002")]
        electrum: String,
    },
    Testnet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long, default_value = "ssl://electrum.blockstream.info:60002")]
        electrum: String,
    },
    /// Run on signet
    Signet {
        /// URL to the electrum backend to use for the wallet.
        #[clap(long)]
        electrum: String,
    },
}

impl Network {
    fn electrum(&self) -> &str {
        match self {
            Network::Mainnet { electrum } => electrum,
            Network::Testnet { electrum } => electrum,
            Network::Signet { electrum } => electrum,
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
}

#[rocket::main]
async fn main() -> Result<()> {
    let opts = Opts::parse();

    logger::init(LevelFilter::DEBUG, opts.json).context("initialize logger")?;
    tracing::info!("Running version: {}", env!("VERGEN_GIT_SEMVER_LIGHTWEIGHT"));

    let data_dir = opts
        .data_dir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().expect("unable to get cwd"));

    let data_dir = opts.network.data_dir(data_dir);

    if !data_dir.exists() {
        tokio::fs::create_dir_all(&data_dir).await?;
    }

    let seed = Seed::initialize(&data_dir.join("taker_seed"), opts.generate_seed).await?;

    let bitcoin_network = opts.network.bitcoin_network();
    let ext_priv_key = seed.derive_extended_priv_key(bitcoin_network)?;

    let wallet = wallet::Actor::new(
        opts.network.electrum(),
        &data_dir.join("taker_wallet.sqlite"),
        ext_priv_key,
    )
    .await?
    .create(None)
    .spawn_global();
    let wallet_info = wallet.send(wallet::Sync).await??;

    // TODO: Actually fetch it from Olivia
    let oracle = schnorrsig::PublicKey::from_str(
        "ddd4636845a90185991826be5a494cde9f4a6947b1727217afedc6292fa4caf7",
    )?;

    let (wallet_feed_sender, wallet_feed_receiver) = watch::channel::<WalletInfo>(wallet_info);

    let (task, quote_updates) = bitmex_price_feed::new().await?;
    tokio::spawn(task);

    let figment = rocket::Config::figment()
        .merge(("address", opts.http_address.ip()))
        .merge(("port", opts.http_address.port()));

    let db = SqlitePool::connect_with(
        SqliteConnectOptions::new()
            .create_if_missing(true)
            .filename(data_dir.join("taker.sqlite")),
    )
    .await?;

    db::run_migrations(&db)
        .await
        .context("Db migrations failed")?;

    // Create actors
    let mut conn = db.acquire().await?;

    housekeeping::transition_non_continue_cfds_to_setup_failed(&mut conn).await?;
    housekeeping::rebroadcast_transactions(&mut conn, &wallet).await?;

    let connection::Actor {
        send_to_maker,
        read_from_maker,
    } = connection::Actor::new(opts.maker).await?;

    let TakerActorSystem {
        cfd_actor_addr,
        cfd_feed_receiver,
        order_feed_receiver,
        update_cfd_feed_receiver,
    } = TakerActorSystem::new(
        db.clone(),
        wallet.clone(),
        oracle,
        send_to_maker,
        read_from_maker,
        |cfds, channel| oracle::Actor::new(cfds, channel, TERM),
        {
            |channel, cfds| {
                let electrum = opts.network.electrum().to_string();
                monitor::Actor::new(electrum, channel, cfds)
            }
        },
    )
    .await?;

    tokio::spawn(wallet_sync::new(wallet, wallet_feed_sender));
    let take_offer_channel = MessageChannel::<taker_cfd::TakeOffer>::clone_channel(&cfd_actor_addr);
    let cfd_action_channel = MessageChannel::<taker_cfd::CfdAction>::clone_channel(&cfd_actor_addr);

    rocket::custom(figment)
        .manage(order_feed_receiver)
        .manage(update_cfd_feed_receiver)
        .manage(take_offer_channel)
        .manage(cfd_action_channel)
        .manage(cfd_feed_receiver)
        .manage(wallet_feed_receiver)
        .manage(quote_updates)
        .manage(bitcoin_network)
        .mount(
            "/api",
            rocket::routes![
                routes_taker::feed,
                routes_taker::post_order_request,
                routes_taker::get_health_check,
                routes_taker::margin_calc,
                routes_taker::post_cfd_action,
            ],
        )
        .mount(
            "/",
            rocket::routes![routes_taker::dist, routes_taker::index],
        )
        .launch()
        .await?;

    db.close().await;

    Ok(())
}
