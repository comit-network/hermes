use crate::collab_settlement_taker;
use crate::future_ext::FutureExt;
use crate::noise;
use crate::rollover_taker;
use crate::setup_taker;
use crate::taker_cfd::CurrentMakerOffers;
use crate::version;
use crate::wire;
use crate::wire::EncryptedJsonCodec;
use crate::wire::Version;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use bdk::bitcoin::Amount;
use futures::SinkExt;
use futures::StreamExt;
use futures::TryStreamExt;
use model::Identity;
use model::OrderId;
use model::Price;
use model::Timestamp;
use model::Usd;
use rand::thread_rng;
use rand::Rng;
use std::net::SocketAddr;
use std::time::Duration;
use std::time::SystemTime;
use time::OffsetDateTime;
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_tasks::Tasks;
use tokio_util::codec::Framed;
use xtra::prelude::MessageChannel;
use xtra::KeepRunning;
use xtra_productivity::xtra_productivity;
use xtras::address_map::NotConnected;
use xtras::AddressMap;
use xtras::LogFailure;
use xtras::SendInterval;

/// Time between reconnection attempts
pub const MAX_RECONNECT_INTERVAL_SECONDS: u64 = 60;

const TCP_TIMEOUT: Duration = Duration::from_secs(10);

/// The "Connected" state of our connection with the maker.
#[allow(clippy::large_enum_variant)]
enum State {
    Connected {
        last_heartbeat: SystemTime,
        /// Last pulse measurement time. Used for checking whether measuring
        /// task is not lagging too much.
        last_pulse: SystemTime,
        write: wire::Write<wire::MakerToTaker, wire::TakerToMaker>,
        _tasks: Tasks,
    },
    Disconnected,
}

impl State {
    async fn send(&mut self, msg: wire::TakerToMaker) -> Result<()> {
        let msg_str = msg.name();

        let write = match self {
            State::Connected { write, .. } => write,
            State::Disconnected => {
                bail!("Cannot send {msg_str}, not connected to maker");
            }
        };

        tracing::trace!(target: "wire", msg_name = msg_str, "Sending");

        write
            .send(msg)
            .await
            .with_context(|| format!("Failed to send message {msg_str} to maker"))?;

        Ok(())
    }

    fn handle_incoming_heartbeat(&mut self) {
        match self {
            State::Connected { last_heartbeat, .. } => {
                *last_heartbeat = SystemTime::now();
            }
            State::Disconnected => {
                debug_assert!(false, "Received heartbeat in disconnected state")
            }
        }
    }

    /// Record the time of the last pulse measurement.
    /// Returns the time difference between the last two pulses.
    fn update_last_pulse_time(&mut self) -> Result<Duration> {
        match self {
            State::Connected { last_pulse, .. } => {
                let new_pulse = SystemTime::now();
                let time_delta = new_pulse
                    .duration_since(*last_pulse)
                    .expect("clock is monotonic");
                *last_pulse = new_pulse;
                Ok(time_delta)
            }
            State::Disconnected => {
                bail!("Measuring pulse in disconnected state");
            }
        }
    }

    fn disconnect_if_last_heartbeat_older_than(&mut self, timeout: Duration) -> bool {
        let duration_since_last_heartbeat = match self {
            State::Connected { last_heartbeat, .. } => SystemTime::now()
                .duration_since(*last_heartbeat)
                .expect("clock is monotonic"),
            State::Disconnected => return false,
        };

        if duration_since_last_heartbeat < timeout {
            return false;
        }

        let heartbeat_timestamp = self
            .last_heartbeat()
            .map(|heartbeat| heartbeat.to_string())
            .unwrap_or_else(|| "None".to_owned());
        let seconds_since_heartbeat = duration_since_last_heartbeat.as_secs();
        tracing::warn!(%seconds_since_heartbeat,
            %heartbeat_timestamp,
            "Disconnecting due to lack of heartbeat",
        );

        *self = State::Disconnected;

        true
    }

    fn last_heartbeat(&self) -> Option<OffsetDateTime> {
        match self {
            State::Connected { last_heartbeat, .. } => Some((*last_heartbeat).into()),
            State::Disconnected => None,
        }
    }
}

pub struct Actor {
    status_sender: watch::Sender<ConnectionStatus>,
    identity_sk: x25519_dalek::StaticSecret,
    current_order: Box<dyn MessageChannel<CurrentMakerOffers>>,
    /// How often we check ("measure pulse") for heartbeat
    /// It should not be greater than maker's `heartbeat interval`
    heartbeat_measuring_rate: Duration,
    /// The interval of heartbeats from the maker
    maker_heartbeat_interval: Duration,
    /// Max duration since the last heartbeat until we die.
    heartbeat_timeout: Duration,
    /// TCP connection timeout
    connect_timeout: Duration,
    state: State,
    setup_actors: AddressMap<OrderId, setup_taker::Actor>,
    collab_settlement_actors: AddressMap<OrderId, collab_settlement_taker::Actor>,
    rollover_actors: AddressMap<OrderId, rollover_taker::Actor>,
}

#[derive(Clone, Copy)]
pub struct Connect {
    pub maker_identity: Identity,
    pub maker_addr: SocketAddr,
}

pub struct MakerStreamMessage {
    pub item: Result<wire::MakerToTaker>,
}

/// Private message to measure the current pulse (i.e. check when we received the last heartbeat).
struct MeasurePulse;

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionStatus {
    Online,
    Offline {
        reason: Option<ConnectionCloseReason>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConnectionCloseReason {
    VersionNegotiationFailed {
        proposed_version: Version,
        actual_version: Version,
    },
}

/// Message sent from the `setup_taker::Actor` to the
/// `connection::Actor` so that it can forward it to the maker.
///
/// Additionally, the address of this instance of the
/// `setup_taker::Actor` is included so that the `connection::Actor`
/// knows where to forward the contract setup messages from the maker
/// about this particular order.
pub struct TakeOrder {
    pub order_id: OrderId,
    pub quantity: Usd,
    pub address: xtra::Address<setup_taker::Actor>,
}

pub struct ProposeSettlement {
    pub order_id: OrderId,
    pub timestamp: Timestamp,
    pub taker: Amount,
    pub maker: Amount,
    pub price: Price,
    pub address: xtra::Address<collab_settlement_taker::Actor>,
}

pub struct ProposeRollover {
    pub order_id: OrderId,
    pub timestamp: Timestamp,
    pub address: xtra::Address<rollover_taker::Actor>,
}

impl Actor {
    pub fn new(
        status_sender: watch::Sender<ConnectionStatus>,
        current_order: &(impl MessageChannel<CurrentMakerOffers> + 'static),
        identity_sk: x25519_dalek::StaticSecret,
        maker_heartbeat_interval: Duration,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            status_sender,
            identity_sk,
            current_order: current_order.clone_channel(),
            heartbeat_measuring_rate: maker_heartbeat_interval.checked_div(2).expect("to divide"),
            maker_heartbeat_interval,
            heartbeat_timeout: maker_heartbeat_interval
                .checked_mul(2)
                .expect("to not overflow"),
            state: State::Disconnected,
            setup_actors: AddressMap::default(),
            connect_timeout,
            collab_settlement_actors: AddressMap::default(),
            rollover_actors: AddressMap::default(),
        }
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle_taker_to_maker(&mut self, message: wire::TakerToMaker) {
        if let Err(e) = self.state.send(message).await {
            tracing::warn!("{:#}", e);
        }
    }

    async fn handle_take_order(&mut self, msg: TakeOrder) -> Result<()> {
        self.state
            .send(wire::TakerToMaker::TakeOrder {
                order_id: msg.order_id,
                quantity: msg.quantity,
            })
            .await?;

        self.setup_actors.insert(msg.order_id, msg.address);

        Ok(())
    }

    async fn handle_propose_settlement(&mut self, msg: ProposeSettlement) -> Result<()> {
        let ProposeSettlement {
            order_id,
            timestamp,
            taker,
            maker,
            price,
            address,
        } = msg;

        self.state
            .send(wire::TakerToMaker::Settlement {
                order_id,
                msg: wire::taker_to_maker::Settlement::Propose {
                    timestamp,
                    taker,
                    maker,
                    price,
                },
            })
            .await?;

        self.collab_settlement_actors.insert(order_id, address);

        Ok(())
    }

    async fn handle_propose_rollover(&mut self, msg: ProposeRollover) -> Result<()> {
        let ProposeRollover {
            order_id,
            timestamp,
            address,
        } = msg;

        self.state
            .send(wire::TakerToMaker::ProposeRolloverV2 {
                order_id,
                timestamp,
            })
            .await?;

        self.rollover_actors.insert(order_id, address);

        Ok(())
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle_connect(
        &mut self,
        Connect {
            maker_addr,
            maker_identity,
        }: Connect,
        ctx: &mut xtra::Context<Self>,
    ) -> Result<()> {
        tracing::debug!(address = %maker_addr, "Connecting to maker");

        let (mut write, mut read) = {
            let mut connection = TcpStream::connect(&maker_addr)
                .timeout(self.connect_timeout)
                .await
                .with_context(|| {
                    let seconds = self.connect_timeout.as_secs();

                    format!("Connection attempt to {maker_addr} timed out after {seconds}s",)
                })?
                .with_context(|| format!("Failed to connect to {maker_addr}"))?;
            let noise = noise::initiator_handshake(
                &mut connection,
                &self.identity_sk,
                &maker_identity.pk(),
            )
            .timeout(TCP_TIMEOUT)
            .await??;

            Framed::new(connection, EncryptedJsonCodec::new(noise)).split()
        };

        let proposed_version = Version::LATEST;
        write
            .send(wire::TakerToMaker::HelloV2 {
                proposed_wire_version: proposed_version.clone(),
                daemon_version: version::version().to_string(),
            })
            .timeout(TCP_TIMEOUT)
            .await??;

        match read
            .try_next()
            .timeout(TCP_TIMEOUT)
            .await
            .with_context(|| {
                format!(
                    "Maker {maker_identity} did not send Hello within 10 seconds, dropping connection"
                )
            })?
            .with_context(|| format!("Failed to read first message from maker {maker_identity}"))? {
            Some(wire::MakerToTaker::Hello(actual_version)) => {
                tracing::info!(%maker_identity, %actual_version, "Received Hello message from maker");
                if proposed_version != actual_version {
                    self.status_sender
                        .send(ConnectionStatus::Offline {
                            reason: Some(ConnectionCloseReason::VersionNegotiationFailed {
                                proposed_version: proposed_version.clone(),
                                actual_version: actual_version.clone(),
                            }),
                        })
                        .expect("receiver to outlive the actor");

                    bail!(
                        "Network version mismatch, we proposed {proposed_version} but maker wants to use {actual_version}"
                    )
                }
            }
            Some(unexpected_message) => {
                bail!(
                    "Unexpected message {} from maker {maker_identity}", unexpected_message.name()
                )
            }
            None => {
                bail!(
                    "Connection to maker {maker_identity} closed before receiving first message"
                )
            }
        }

        tracing::info!(address = %maker_addr, "Established connection to maker");

        let this = ctx.address().expect("self to be alive");

        let mut tasks = Tasks::default();
        tasks.add(
            this.clone()
                .attach_stream(read.map(move |item| MakerStreamMessage { item })),
        );
        tasks.add(this.send_interval(self.heartbeat_measuring_rate, || MeasurePulse));

        self.state = State::Connected {
            last_heartbeat: SystemTime::now(),
            last_pulse: SystemTime::now(),
            write,
            _tasks: tasks,
        };
        self.status_sender
            .send(ConnectionStatus::Online)
            .expect("receiver to outlive the actor");

        Ok(())
    }

    async fn handle_wire_message(&mut self, message: MakerStreamMessage) -> KeepRunning {
        let msg = match message.item {
            Ok(msg) => msg,
            Err(e) => {
                tracing::warn!("Error while receiving message from maker: {:#}", e);
                return KeepRunning::Yes;
            }
        };

        let msg_name = msg.name();

        tracing::trace!(target: "wire", msg_name, "Received");

        match msg {
            wire::MakerToTaker::Heartbeat => {
                self.state.handle_incoming_heartbeat();
            }
            wire::MakerToTaker::ConfirmOrder(order_id) => {
                if let Err(NotConnected(_)) = self
                    .setup_actors
                    .send_async(&order_id, setup_taker::Accepted)
                    .await
                {
                    tracing::warn!(%order_id, "No active setup actor");
                }
            }
            wire::MakerToTaker::RejectOrder(order_id) => {
                if let Err(NotConnected(_)) = self
                    .setup_actors
                    .send_async(&order_id, setup_taker::Rejected::without_reason())
                    .await
                {
                    tracing::warn!(%order_id, "No active setup actor");
                }
            }
            wire::MakerToTaker::Protocol { order_id, msg } => {
                if let Err(NotConnected(_)) = self.setup_actors.send_async(&order_id, msg).await {
                    tracing::warn!(%order_id, "No active setup actor");
                }
            }
            wire::MakerToTaker::InvalidOrderId(order_id) => {
                if let Err(NotConnected(_)) = self
                    .setup_actors
                    .send_async(&order_id, setup_taker::Rejected::invalid_order_id())
                    .await
                {
                    tracing::warn!(%order_id, "No active setup actor");
                }
            }
            wire::MakerToTaker::Settlement { order_id, msg } => {
                if let Err(NotConnected(_)) = self
                    .collab_settlement_actors
                    .send_async(&order_id, msg)
                    .await
                {
                    tracing::warn!(%order_id, "No active collaborative settlement")
                }
            }
            wire::MakerToTaker::ConfirmRollover {
                order_id,
                oracle_event_id,
                tx_fee_rate,
                funding_rate,
                complete_fee,
            } => {
                if let Err(NotConnected(_)) = self
                    .rollover_actors
                    .send_async(
                        &order_id,
                        rollover_taker::RolloverAccepted {
                            oracle_event_id,
                            tx_fee_rate,
                            funding_rate,
                            complete_fee: complete_fee.into(),
                        },
                    )
                    .await
                {
                    tracing::warn!(%order_id, "No active rollover");
                }
            }
            wire::MakerToTaker::RejectRollover(order_id) => {
                if let Err(NotConnected(_)) = self
                    .rollover_actors
                    .send_async(&order_id, rollover_taker::RolloverRejected)
                    .await
                {
                    tracing::warn!(%order_id, "No active rollover");
                }
            }
            wire::MakerToTaker::RolloverProtocol { order_id, msg } => {
                if let Err(NotConnected(_)) = self.rollover_actors.send_async(&order_id, msg).await
                {
                    tracing::warn!(%order_id, "No active rollover");
                }
            }
            wire::MakerToTaker::CurrentOffers(maker_offers) => {
                let _ = self
                    .current_order
                    .send(CurrentMakerOffers(maker_offers))
                    .log_failure("Failed to forward current order from maker")
                    .await;
            }
            wire::MakerToTaker::CurrentOrder(_) => {
                // no-op, we support `CurrentOffers` message and can ignore this one.
            }
            wire::MakerToTaker::Hello(_) => {
                tracing::warn!("Ignoring unexpected Hello message from maker. Hello is only expected when opening a new connection.")
            }
            wire::MakerToTaker::Unknown => {
                // Ignore unknown message to be forwards-compatible. We are logging it above on
                // `trace` level already.
            }
        }
        KeepRunning::Yes
    }

    fn handle_measure_pulse(&mut self, _: MeasurePulse) {
        tracing::trace!(target: "wire", "measuring heartbeat pulse");

        match self.state.update_last_pulse_time() {
            Ok(duration) => {
                if duration >= self.maker_heartbeat_interval {
                    let seconds = self.maker_heartbeat_interval.as_secs();
                    let pulse_delta_seconds = duration.as_secs();

                    tracing::warn!(
                        "Heartbeat pulse measurements fell behind more than heartbeat interval ({seconds}), likely missing a heartbeat from the maker. Diff between pulses: {pulse_delta_seconds}"
                    );
                    return; // Don't try to disconnect if the measurements fell behind
                }
            }
            Err(e) => {
                tracing::debug!("{e}");
            }
        }

        if self
            .state
            .disconnect_if_last_heartbeat_older_than(self.heartbeat_timeout)
        {
            self.status_sender
                .send(ConnectionStatus::Offline { reason: None })
                .expect("watch receiver to outlive the actor");
        }
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    type Stop = ();

    async fn stopped(self) -> Self::Stop {}
}

// TODO: Move the reconnection logic inside the connection::Actor instead of
// depending on a watch channel
pub async fn connect(
    mut maker_online_status_feed_receiver: watch::Receiver<ConnectionStatus>,
    connection_actor_addr: xtra::Address<Actor>,
    maker_identity: Identity,
    maker_addresses: Vec<SocketAddr>,
) {
    loop {
        let connection_status = maker_online_status_feed_receiver.borrow().clone();
        if matches!(connection_status, ConnectionStatus::Offline { .. }) {
            tracing::debug!("No connection to the maker");
            'connect: loop {
                for address in &maker_addresses {
                    let connect_msg = Connect {
                        maker_identity,
                        maker_addr: *address,
                    };

                    if let Err(e) = connection_actor_addr
                        .send(connect_msg)
                        .await
                        .expect("Taker actor to be present")
                    {
                        tracing::warn!(%address, "Failed to establish connection: {:#}", e);
                        continue;
                    }
                    break 'connect;
                }

                let num_addresses = maker_addresses.len();

                // Apply a random number of seconds between the reconnection
                // attempts so that all takers don't try to reconnect at the same time
                let seconds = thread_rng().gen_range(5, MAX_RECONNECT_INTERVAL_SECONDS);

                tracing::warn!(
                    "Tried connecting to {num_addresses} addresses without success, retrying in {seconds} seconds",
                );

                tokio::time::sleep(Duration::from_secs(seconds)).await;
            }
        }
        maker_online_status_feed_receiver
            .changed()
            .await
            .expect("watch channel should outlive the future");
    }
}
