use crate::address_map::ActorName;
use crate::address_map::Stopping;
use crate::connection;
use crate::model::cfd::CannotRollover;
use crate::model::cfd::Cfd;
use crate::model::cfd::Dlc;
use crate::model::cfd::OrderId;
use crate::model::cfd::Role;
use crate::model::cfd::RolloverProposal;
use crate::model::cfd::SettlementKind;
use crate::model::BitMexPriceEventId;
use crate::model::Timestamp;
use crate::oracle;
use crate::oracle::GetAnnouncement;
use crate::projection;
use crate::projection::UpdateRollOverProposal;
use crate::setup_contract;
use crate::setup_contract::RolloverParams;
use crate::tokio_ext::spawn_fallible;
use crate::wire;
use crate::wire::RollOverMsg;
use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use futures::channel::mpsc;
use futures::channel::mpsc::UnboundedSender;
use futures::future;
use futures::SinkExt;
use maia::secp256k1_zkp::schnorrsig;
use time::OffsetDateTime;
use xtra::prelude::MessageChannel;
use xtra_productivity::xtra_productivity;

pub struct Actor {
    cfd: Cfd,
    n_payouts: usize,
    oracle_pk: schnorrsig::PublicKey,
    timestamp: Timestamp,
    maker: xtra::Address<connection::Actor>,
    get_announcement: Box<dyn MessageChannel<GetAnnouncement>>,
    projection: xtra::Address<projection::Actor>,
    on_completed: Box<dyn MessageChannel<Completed>>,
    on_stopping: Vec<Box<dyn MessageChannel<Stopping<Self>>>>,
    rollover_msg_sender: Option<UnboundedSender<RollOverMsg>>,
}

impl Actor {
    pub fn new(
        (cfd, n_payouts): (Cfd, usize),
        oracle_pk: schnorrsig::PublicKey,
        maker: xtra::Address<connection::Actor>,
        get_announcement: &(impl MessageChannel<GetAnnouncement> + 'static),
        projection: xtra::Address<projection::Actor>,
        on_completed: &(impl MessageChannel<Completed> + 'static),
        (on_stopping0, on_stopping1): (
            &(impl MessageChannel<Stopping<Self>> + 'static),
            &(impl MessageChannel<Stopping<Self>> + 'static),
        ),
    ) -> Self {
        Self {
            cfd,
            n_payouts,
            oracle_pk,
            timestamp: Timestamp::now(),
            maker,
            get_announcement: get_announcement.clone_channel(),
            projection,
            on_completed: on_completed.clone_channel(),
            on_stopping: vec![on_stopping0.clone_channel(), on_stopping1.clone_channel()],
            rollover_msg_sender: None,
        }
    }

    async fn propose(&self, this: xtra::Address<Self>) -> Result<()> {
        self.maker
            .send(connection::ProposeRollOver {
                order_id: self.cfd.id,
                timestamp: self.timestamp,
                address: this,
            })
            .await??;

        self.update_proposal(Some((
            RolloverProposal {
                order_id: self.cfd.id,
                timestamp: self.timestamp,
            },
            SettlementKind::Outgoing,
        )))
        .await?;

        Ok(())
    }

    async fn handle_confirmed(
        &mut self,
        msg: RollOverAccepted,
        ctx: &mut xtra::Context<Self>,
    ) -> Result<()> {
        let RollOverAccepted { oracle_event_id } = msg;
        let announcement = self
            .get_announcement
            .send(oracle::GetAnnouncement(oracle_event_id))
            .await?
            .with_context(|| format!("Announcement {} not found", oracle_event_id))?;

        let order_id = self.cfd.id;
        tracing::info!(%order_id, "Rollover proposal got accepted");

        self.update_proposal(None).await?;

        let (sender, receiver) = mpsc::unbounded::<RollOverMsg>();
        // store the writing end to forward messages from the maker to
        // the spawned rollover task
        self.rollover_msg_sender = Some(sender);

        let rollover_fut = setup_contract::roll_over(
            xtra::message_channel::MessageChannel::sink(&self.maker).with(move |msg| {
                future::ok(wire::TakerToMaker::RollOverProtocol { order_id, msg })
            }),
            receiver,
            (self.oracle_pk, announcement),
            RolloverParams::new(
                self.cfd.price,
                self.cfd.quantity_usd,
                self.cfd.leverage,
                self.cfd.refund_timelock_in_blocks(),
                self.cfd.fee_rate,
            ),
            Role::Taker,
            self.cfd.dlc().context("No DLC in CFD")?,
            self.n_payouts,
        );

        let this = ctx.address().expect("self to be alive");
        spawn_fallible::<_, anyhow::Error>(async move {
            let _ = match rollover_fut.await {
                Ok(dlc) => this.send(RolloverSucceeded { dlc }).await?,
                Err(error) => this.send(RolloverFailed { error }).await?,
            };

            Ok(())
        });

        Ok(())
    }

    async fn handle_rejected(&self) -> Result<()> {
        let order_id = self.cfd.id;
        tracing::info!(%order_id, "Rollover proposal got rejected");

        self.update_proposal(None).await?;

        Ok(())
    }

    pub async fn forward_protocol_msg(&mut self, msg: wire::RollOverMsg) -> Result<()> {
        let sender = self
            .rollover_msg_sender
            .as_mut()
            .context("Cannot forward message to rollover task")?;
        sender.send(msg).await?;

        Ok(())
    }

    async fn update_proposal(
        &self,
        proposal: Option<(RolloverProposal, SettlementKind)>,
    ) -> Result<()> {
        self.projection
            .send(UpdateRollOverProposal {
                order: self.cfd.id,
                proposal,
            })
            .await?;

        Ok(())
    }

    async fn complete(&mut self, completed: Completed, ctx: &mut xtra::Context<Self>) {
        let _ = self.on_completed.send(completed).await;

        ctx.stop();
    }
}

#[async_trait]
impl xtra::Actor for Actor {
    async fn started(&mut self, ctx: &mut xtra::Context<Self>) {
        if let Err(e) = self.cfd.can_roll_over(OffsetDateTime::now_utc()) {
            self.complete(
                Completed::CannotRollover {
                    order_id: self.cfd.id,
                    reason: e,
                },
                ctx,
            )
            .await;
        }

        let this = ctx.address().expect("self to be alive");

        if let Err(e) = self.propose(this).await {
            self.complete(
                Completed::Failed {
                    order_id: self.cfd.id,
                    error: e,
                },
                ctx,
            )
            .await;
        }
    }

    async fn stopping(&mut self, ctx: &mut xtra::Context<Self>) -> xtra::KeepRunning {
        // inform other actors that we are stopping so that our
        // address can be GCd from their AddressMaps
        let me = ctx.address().expect("we are still alive");

        for channel in self.on_stopping.iter() {
            let _ = channel.send(Stopping { me: me.clone() }).await;
        }

        xtra::KeepRunning::StopAll
    }
}

#[xtra_productivity]
impl Actor {
    pub async fn handle_confirm_rollover(
        &mut self,
        msg: RollOverAccepted,
        ctx: &mut xtra::Context<Self>,
    ) {
        if let Err(error) = self.handle_confirmed(msg, ctx).await {
            self.complete(
                Completed::Failed {
                    order_id: self.cfd.id,
                    error,
                },
                ctx,
            )
            .await;
        }
    }

    pub async fn reject_rollover(&mut self, _: RollOverRejected, ctx: &mut xtra::Context<Self>) {
        let order_id = self.cfd.id;
        let completed = if let Err(error) = self.handle_rejected().await {
            Completed::Failed { order_id, error }
        } else {
            Completed::Rejected { order_id }
        };

        self.complete(completed, ctx).await;
    }

    pub async fn handle_rollover_succeeded(
        &mut self,
        msg: RolloverSucceeded,
        ctx: &mut xtra::Context<Self>,
    ) {
        self.complete(
            Completed::UpdatedContract {
                order_id: self.cfd.id,
                dlc: msg.dlc,
            },
            ctx,
        )
        .await;
    }

    pub async fn handle_rollover_failed(
        &mut self,
        msg: RolloverFailed,
        ctx: &mut xtra::Context<Self>,
    ) {
        self.complete(
            Completed::Failed {
                order_id: self.cfd.id,
                error: msg.error,
            },
            ctx,
        )
        .await;
    }

    pub async fn handle_protocol_msg(
        &mut self,
        msg: wire::RollOverMsg,
        ctx: &mut xtra::Context<Self>,
    ) {
        if let Err(error) = self.forward_protocol_msg(msg).await {
            self.complete(
                Completed::Failed {
                    order_id: self.cfd.id,
                    error,
                },
                ctx,
            )
            .await;
        }
    }
}

/// Message sent from the `connection::Actor` to the
/// `rollover_taker::Actor` to notify that the maker has accepted the
/// rollover proposal.
pub struct RollOverAccepted {
    pub oracle_event_id: BitMexPriceEventId,
}

/// Message sent from the `connection::Actor` to the
/// `rollover_taker::Actor` to notify that the maker has rejected the
/// rollover proposal.
pub struct RollOverRejected;

/// Message sent from the spawned task to `rollover_taker::Actor` to
/// notify that rollover has finished successfully.
pub struct RolloverSucceeded {
    dlc: Dlc,
}

/// Message sent from the spawned task to `rollover_taker::Actor` to
/// notify that rollover has failed.
pub struct RolloverFailed {
    error: anyhow::Error,
}

#[allow(clippy::large_enum_variant)]
pub enum Completed {
    UpdatedContract {
        order_id: OrderId,
        dlc: Dlc,
    },
    Rejected {
        order_id: OrderId,
    },
    Failed {
        order_id: OrderId,
        error: anyhow::Error,
    },
    CannotRollover {
        order_id: OrderId,
        reason: CannotRollover,
    },
}

impl xtra::Message for Completed {
    type Result = Result<()>;
}

impl ActorName for Actor {
    fn actor_name() -> String {
        "Taker rollover".to_string()
    }
}
