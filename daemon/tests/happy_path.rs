use anyhow::Result;
use async_trait::async_trait;
use bdk::bitcoin::util::psbt::PartiallySignedTransaction;
use bdk::bitcoin::Txid;
use cfd_protocol::secp256k1_zkp::schnorrsig;
use cfd_protocol::PartyParams;
use daemon::model::cfd::Order;
use daemon::model::{Usd, WalletInfo};
use daemon::{connection, db, maker_cfd, maker_inc_connections, monitor, oracle, wallet};
use rust_decimal_macros::dec;
use sqlx::SqlitePool;
use std::net::SocketAddr;
use std::str::FromStr;
use std::task::Poll;
use tokio::sync::watch;
use xtra::spawn::TokioGlobalSpawnExt;
use xtra::Actor;
use xtra_productivity::xtra_productivity;

#[tokio::test]
async fn taker_receives_order_from_maker_on_publication() {
    let (mut maker, mut taker) = start_both().await;

    let (published, received) =
        tokio::join!(maker.publish_order(new_dummy_order()), taker.next_order());

    assert_eq!(published, received)
}

fn new_dummy_order() -> maker_cfd::NewOrder {
    maker_cfd::NewOrder {
        price: Usd::new(dec!(50_000)),
        min_quantity: Usd::new(dec!(10)),
        max_quantity: Usd::new(dec!(100)),
    }
}

// Mocks the network layer between the taker and the maker ("the wire")
struct ActorConnection {}
impl xtra::Actor for ActorConnection {}

#[xtra_productivity(message_impl = false)]
impl ActorConnection {}

/// Test Stub simulating the Oracle actor
struct Oracle;
impl xtra::Actor for Oracle {}

#[xtra_productivity(message_impl = false)]
impl Oracle {
    async fn handle_fetch_announcement(&mut self, _msg: oracle::FetchAnnouncement) {}

    async fn handle_get_announcement(
        &mut self,
        _msg: oracle::GetAnnouncement,
    ) -> Option<oracle::Announcement> {
        todo!("stub this if needed")
    }

    async fn handle(&mut self, _msg: oracle::MonitorAttestation) {
        todo!("stub this if needed")
    }

    async fn handle(&mut self, _msg: oracle::Sync) {}
}

/// Test Stub simulating the Monitor actor
struct Monitor;
impl xtra::Actor for Monitor {}

#[xtra_productivity(message_impl = false)]
impl Monitor {
    async fn handle(&mut self, _msg: monitor::Sync) {}

    async fn handle(&mut self, _msg: monitor::StartMonitoring) {
        todo!("stub this if needed")
    }

    async fn handle(&mut self, _msg: monitor::CollaborativeSettlement) {
        todo!("stub this if needed")
    }

    async fn handle(&mut self, _msg: oracle::Attestation) {
        todo!("stub this if needed")
    }
}

/// Test Stub simulating the Wallet actor
struct Wallet;
impl xtra::Actor for Wallet {}

#[xtra_productivity(message_impl = false)]
impl Wallet {
    async fn handle(&mut self, _msg: wallet::BuildPartyParams) -> Result<PartyParams> {
        todo!("stub this if needed")
    }
    async fn handle(&mut self, _msg: wallet::Sync) -> Result<WalletInfo> {
        todo!("stub this if needed")
    }
    async fn handle(&mut self, _msg: wallet::Sign) -> Result<PartiallySignedTransaction> {
        todo!("stub this if needed")
    }
    async fn handle(&mut self, _msg: wallet::TryBroadcastTransaction) -> Result<Txid> {
        todo!("stub this if needed")
    }
}

/// Maker Test Setup
struct Maker {
    cfd_actor_addr:
        xtra::Address<maker_cfd::Actor<Oracle, Monitor, maker_inc_connections::Actor, Wallet>>,
    order_feed_receiver: watch::Receiver<Option<Order>>,
    inc_conn_addr: xtra::Address<maker_inc_connections::Actor>,
    address: SocketAddr,
}

impl Maker {
    async fn start(oracle_pk: schnorrsig::PublicKey) -> Self {
        let db = in_memory_db().await;

        let wallet_addr = Wallet {}.create(None).spawn_global();

        let maker = daemon::MakerActorSystem::new(
            db,
            wallet_addr,
            oracle_pk,
            |_, _| Oracle,
            |_, _| async { Ok(Monitor) },
            |channel0, channel1| maker_inc_connections::Actor::new(channel0, channel1),
        )
        .await
        .unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();

        let address = listener.local_addr().unwrap();

        let listener_stream = futures::stream::poll_fn(move |ctx| {
            let message = match futures::ready!(listener.poll_accept(ctx)) {
                Ok((stream, address)) => {
                    maker_inc_connections::ListenerMessage::NewConnection { stream, address }
                }
                Err(e) => maker_inc_connections::ListenerMessage::Error { source: e },
            };

            Poll::Ready(Some(message))
        });

        tokio::spawn(maker.inc_conn_addr.clone().attach_stream(listener_stream));

        Self {
            cfd_actor_addr: maker.cfd_actor_addr,
            order_feed_receiver: maker.order_feed_receiver,
            inc_conn_addr: maker.inc_conn_addr,
            address,
        }
    }

    async fn publish_order(&mut self, new_order_params: maker_cfd::NewOrder) -> Order {
        self.cfd_actor_addr.send(new_order_params).await.unwrap();
        let next_order = self.order_feed_receiver.borrow().clone().unwrap();

        next_order
    }
}

/// Taker Test Setup
struct Taker {
    order_feed: watch::Receiver<Option<Order>>,
}

impl Taker {
    async fn start(oracle_pk: schnorrsig::PublicKey, maker_address: SocketAddr) -> Self {
        let connection::Actor {
            send_to_maker,
            read_from_maker,
        } = connection::Actor::new(maker_address).await;

        let db = in_memory_db().await;

        let wallet_addr = Wallet {}.create(None).spawn_global();

        let taker = daemon::TakerActorSystem::new(
            db,
            wallet_addr,
            oracle_pk,
            send_to_maker,
            read_from_maker,
            |_, _| Oracle,
            |_, _| async { Ok(Monitor) },
        )
        .await
        .unwrap();

        Self {
            order_feed: taker.order_feed_receiver,
        }
    }

    async fn next_order(&mut self) -> Order {
        self.order_feed.changed().await.unwrap();
        let next_order = self.order_feed.borrow().clone().unwrap();
        next_order
    }
}

async fn start_both() -> (Maker, Taker) {
    let oracle_pk: schnorrsig::PublicKey = schnorrsig::PublicKey::from_str(
        "ddd4636845a90185991826be5a494cde9f4a6947b1727217afedc6292fa4caf7",
    )
    .unwrap();

    let maker = Maker::start(oracle_pk).await;
    let taker = Taker::start(oracle_pk, maker.address).await;
    (maker, taker)
}

async fn in_memory_db() -> SqlitePool {
    // Note: Every :memory: database is distinct from every other. So, opening two database
    // connections each with the filename ":memory:" will create two independent in-memory
    // databases. see: https://www.sqlite.org/inmemorydb.html
    let pool = SqlitePool::connect(":memory:").await.unwrap();

    db::run_migrations(&pool).await.unwrap();

    pool
}
