use crate::maker_cfd::{FromTaker, NewTakerOnline};
use crate::model::cfd::{Order, OrderId};
use crate::model::{BitMexPriceEventId, TakerId};
use crate::{forward_only_ok, log_error, maker_cfd, send_to_socket, tokio_ext, wire};
use anyhow::{Context as AnyhowContext, Result};
use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use tokio::net::TcpStream;
use tokio_util::codec::FramedRead;
use xtra::prelude::*;
use xtra::spawn::TokioGlobalSpawnExt;
use xtra::{Actor as _, KeepRunning};

pub struct BroadcastOrder(pub Option<Order>);

#[allow(clippy::large_enum_variant)]
pub enum TakerCommand {
    SendOrder {
        order: Option<Order>,
    },
    NotifyInvalidOrderId {
        id: OrderId,
    },
    NotifyOrderAccepted {
        id: OrderId,
    },
    NotifyOrderRejected {
        id: OrderId,
    },
    NotifySettlementAccepted {
        id: OrderId,
    },
    NotifySettlementRejected {
        id: OrderId,
    },
    NotifyRollOverAccepted {
        id: OrderId,
        oracle_event_id: BitMexPriceEventId,
    },
    NotifyRollOverRejected {
        id: OrderId,
    },
    Protocol(wire::SetupMsg),
    RollOverProtocol(wire::RollOverMsg),
}

pub struct TakerMessage {
    pub taker_id: TakerId,
    pub command: TakerCommand,
}

pub enum ListenerMessage {
    NewConnection {
        stream: TcpStream,
        address: SocketAddr,
    },
    Error {
        source: io::Error,
    },
}

pub struct Actor {
    write_connections: HashMap<TakerId, Address<send_to_socket::Actor<wire::MakerToTaker>>>,
    new_taker_channel: Box<dyn MessageChannel<NewTakerOnline>>,
    taker_msg_channel: Box<dyn MessageChannel<FromTaker>>,
}

impl Actor {
    pub fn new(
        new_taker_channel: &impl MessageChannel<NewTakerOnline>,
        taker_msg_channel: &impl MessageChannel<FromTaker>,
    ) -> Self {
        Self {
            write_connections: HashMap::new(),
            new_taker_channel: new_taker_channel.clone_channel(),
            taker_msg_channel: taker_msg_channel.clone_channel(),
        }
    }

    async fn send_to_taker(&self, taker_id: TakerId, msg: wire::MakerToTaker) -> Result<()> {
        let conn = self
            .write_connections
            .get(&taker_id)
            .context("no connection to taker_id")?;

        // use `.send` here to ensure we only continue once the message has been sent
        conn.send(msg).await?;

        Ok(())
    }

    async fn handle_taker_message(&mut self, msg: TakerMessage) -> Result<()> {
        match msg.command {
            TakerCommand::SendOrder { order } => {
                self.send_to_taker(msg.taker_id, wire::MakerToTaker::CurrentOrder(order))
                    .await?;
            }
            TakerCommand::NotifyInvalidOrderId { id } => {
                self.send_to_taker(msg.taker_id, wire::MakerToTaker::InvalidOrderId(id))
                    .await?;
            }
            TakerCommand::NotifyOrderAccepted { id } => {
                self.send_to_taker(msg.taker_id, wire::MakerToTaker::ConfirmOrder(id))
                    .await?;
            }
            TakerCommand::NotifyOrderRejected { id } => {
                self.send_to_taker(msg.taker_id, wire::MakerToTaker::RejectOrder(id))
                    .await?;
            }
            TakerCommand::NotifySettlementAccepted { id } => {
                self.send_to_taker(msg.taker_id, wire::MakerToTaker::ConfirmSettlement(id))
                    .await?;
            }
            TakerCommand::NotifySettlementRejected { id } => {
                self.send_to_taker(msg.taker_id, wire::MakerToTaker::RejectSettlement(id))
                    .await?;
            }
            TakerCommand::Protocol(setup_msg) => {
                self.send_to_taker(msg.taker_id, wire::MakerToTaker::Protocol(setup_msg))
                    .await?;
            }
            TakerCommand::NotifyRollOverAccepted {
                id,
                oracle_event_id,
            } => {
                self.send_to_taker(
                    msg.taker_id,
                    wire::MakerToTaker::ConfirmRollOver {
                        order_id: id,
                        oracle_event_id,
                    },
                )
                .await?;
            }
            TakerCommand::NotifyRollOverRejected { id } => {
                self.send_to_taker(msg.taker_id, wire::MakerToTaker::RejectRollOver(id))
                    .await?;
            }
            TakerCommand::RollOverProtocol(roll_over_msg) => {
                self.send_to_taker(
                    msg.taker_id,
                    wire::MakerToTaker::RollOverProtocol(roll_over_msg),
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn handle_new_connection(
        &mut self,
        stream: TcpStream,
        address: SocketAddr,
        _: &mut Context<Self>,
    ) {
        let taker_id = TakerId::default();

        tracing::info!("New taker {} connected on {}", taker_id, address);

        let (read, write) = stream.into_split();
        let read = FramedRead::new(read, wire::JsonCodec::default())
            .map_ok(move |msg| FromTaker { taker_id, msg })
            .map(forward_only_ok::Message);

        let (out_msg_actor_address, mut out_msg_actor_context) = xtra::Context::new(None);

        let forward_to_cfd = forward_only_ok::Actor::new(self.taker_msg_channel.clone_channel())
            .create(None)
            .spawn_global();

        // only allow outgoing messages while we are successfully reading incoming ones
        tokio::spawn(async move {
            let mut actor = send_to_socket::Actor::new(write);

            out_msg_actor_context
                .handle_while(&mut actor, forward_to_cfd.attach_stream(read))
                .await;

            tracing::error!("Closing connection to taker {}", taker_id);

            actor.shutdown().await;
        });

        self.write_connections
            .insert(taker_id, out_msg_actor_address);

        let _ = self
            .new_taker_channel
            .send(maker_cfd::NewTakerOnline { id: taker_id })
            .await;
    }
}

macro_rules! log_error {
    ($future:expr) => {
        if let Err(e) = $future.await {
            tracing::error!(%e);
        }
    };
}

#[async_trait]
impl Handler<BroadcastOrder> for Actor {
    async fn handle(&mut self, msg: BroadcastOrder, _ctx: &mut Context<Self>) {
        let order = msg.0;

        for conn in self.write_connections.values() {
            tokio_ext::spawn_fallible(conn.send(wire::MakerToTaker::CurrentOrder(order.clone())));
        }
    }
}

#[async_trait]
impl Handler<TakerMessage> for Actor {
    async fn handle(&mut self, msg: TakerMessage, _ctx: &mut Context<Self>) {
        log_error!(self.handle_taker_message(msg));
    }
}

#[async_trait]
impl Handler<ListenerMessage> for Actor {
    async fn handle(&mut self, msg: ListenerMessage, ctx: &mut Context<Self>) -> KeepRunning {
        match msg {
            ListenerMessage::NewConnection { stream, address } => {
                self.handle_new_connection(stream, address, ctx).await;

                KeepRunning::Yes
            }
            ListenerMessage::Error { source } => {
                tracing::warn!("TCP listener produced an error: {}", source);

                // Maybe we should move the actual listening on the socket into here and restart the
                // actor upon an error?
                KeepRunning::Yes
            }
        }
    }
}

impl Message for BroadcastOrder {
    type Result = ();
}

impl Message for TakerMessage {
    type Result = ();
}

impl Message for ListenerMessage {
    type Result = KeepRunning;
}

impl xtra::Actor for Actor {}
