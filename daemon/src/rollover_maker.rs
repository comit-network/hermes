use crate::address_map::ActorName;
use crate::maker_inc_connections;
use crate::maker_inc_connections::TakerMessage;
use crate::model::cfd::Dlc;
use crate::model::cfd::OrderId;
use crate::model::cfd::Role;
use crate::model::cfd::RolloverProposal;
use crate::model::cfd::SettlementKind;
use crate::model::cfd::UpdateCfdProposal;
use crate::model::Identity;
use crate::oracle;
use crate::oracle::GetAnnouncement;
use crate::projection;
use crate::projection::try_into_update_rollover_proposal;
use crate::projection::UpdateRollOverProposal;
use crate::schnorrsig;
use crate::setup_contract;
use crate::setup_contract::RolloverParams;
use crate::tokio_ext::spawn_fallible;
use crate::wire;
use crate::wire::MakerToTaker;
use crate::wire::RollOverMsg;
use crate::Cfd;
use crate::Stopping;
use anyhow::Context as _;
use anyhow::Result;
use futures::channel::mpsc;
use futures::channel::mpsc::UnboundedSender;
use futures::future;
use futures::SinkExt;
use xtra::prelude::MessageChannel;
use xtra::Context;
use xtra::KeepRunning;
use xtra_productivity::xtra_productivity;

pub struct AcceptRollOver;

pub struct RejectRollOver;

pub struct ProtocolMsg(pub wire::RollOverMsg);

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
pub struct Completed {
    pub order_id: OrderId,
    pub dlc: Dlc,
}

pub struct Actor {
    send_to_taker_actor: Box<dyn MessageChannel<TakerMessage>>,
    cfd: Cfd,
    taker_id: Identity,
    n_payouts: usize,
    oracle_pk: schnorrsig::PublicKey,
    sent_from_taker: Option<UnboundedSender<RollOverMsg>>,
    maker_cfd_actor: Box<dyn MessageChannel<Completed>>,
    oracle_actor: Box<dyn MessageChannel<GetAnnouncement>>,
    on_stopping: Vec<Box<dyn MessageChannel<Stopping<Self>>>>,
    projection_actor: xtra::Address<projection::Actor>,
    proposal: RolloverProposal,
}

#[async_trait::async_trait]
impl xtra::Actor for Actor {
    async fn stopping(&mut self, ctx: &mut Context<Self>) -> KeepRunning {
        let address = ctx.address().expect("acquired own actor address");

        for channel in self.on_stopping.iter() {
            let _ = channel
                .send(Stopping {
                    me: address.clone(),
                })
                .await;
        }

        KeepRunning::StopAll
    }

    async fn started(&mut self, _ctx: &mut Context<Self>) {
        let new_proposal = UpdateCfdProposal::RollOverProposal {
            proposal: self.proposal.clone(),
            direction: SettlementKind::Incoming,
        };

        self.projection_actor
            .send(
                try_into_update_rollover_proposal(new_proposal)
                    .expect("update cfd proposal is rollover proposal"),
            )
            .await
            .expect("projection actor is running");
    }
}

impl Actor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        send_to_taker_actor: &(impl MessageChannel<TakerMessage> + 'static),
        cfd: Cfd,
        taker_id: Identity,
        oracle_pk: schnorrsig::PublicKey,
        maker_cfd_actor: &(impl MessageChannel<Completed> + 'static),
        oracle_actor: &(impl MessageChannel<GetAnnouncement> + 'static),
        (on_stopping0, on_stopping1): (
            &(impl MessageChannel<Stopping<Self>> + 'static),
            &(impl MessageChannel<Stopping<Self>> + 'static),
        ),
        projection_actor: xtra::Address<projection::Actor>,
        proposal: RolloverProposal,
        n_payouts: usize,
    ) -> Self {
        Self {
            send_to_taker_actor: send_to_taker_actor.clone_channel(),
            cfd,
            taker_id,
            n_payouts,
            oracle_pk,
            sent_from_taker: None,
            maker_cfd_actor: maker_cfd_actor.clone_channel(),
            oracle_actor: oracle_actor.clone_channel(),
            on_stopping: vec![on_stopping0.clone_channel(), on_stopping1.clone_channel()],
            projection_actor,
            proposal,
        }
    }

    async fn update_contract(&mut self, dlc: Dlc, ctx: &mut xtra::Context<Self>) -> Result<()> {
        let msg = Completed {
            order_id: self.cfd.id,
            dlc,
        };
        self.maker_cfd_actor.send(msg).await?;
        ctx.stop();
        Ok(())
    }

    async fn fail(&mut self, ctx: &mut xtra::Context<Self>, error: anyhow::Error) {
        tracing::info!(%self.cfd.id, %error, "Rollover failed");
        if let Err(err) = self
            .projection_actor
            .send(projection::UpdateRollOverProposal {
                order: self.cfd.id,
                proposal: None,
            })
            .await
        {
            tracing::error!(%err, "projection actor unreachable when attempting to fail rollover");
        }
        ctx.stop();
    }

    async fn accept(&mut self, ctx: &mut xtra::Context<Self>) -> Result<()> {
        let order_id = self.cfd.id;

        let (sender, receiver) = mpsc::unbounded();

        self.sent_from_taker = Some(sender);

        tracing::debug!(%order_id, "Maker accepts a roll_over proposal" );

        let cfd = self.cfd.clone();

        let dlc = cfd.open_dlc().expect("CFD was in wrong state");

        let oracle_event_id = oracle::next_announcement_after(
            time::OffsetDateTime::now_utc() + cfd.settlement_interval,
        )?;

        let taker_id = self.taker_id;

        self.send_to_taker_actor
            .send(maker_inc_connections::TakerMessage {
                taker_id,
                msg: wire::MakerToTaker::ConfirmRollOver {
                    order_id,
                    oracle_event_id,
                },
            })
            .await??;

        self.projection_actor
            .send(UpdateRollOverProposal {
                order: order_id,
                proposal: None,
            })
            .await?;

        let announcement = self
            .oracle_actor
            .send(oracle::GetAnnouncement(oracle_event_id))
            .await?
            .with_context(|| format!("Announcement {} not found", oracle_event_id))?;

        let rollover_fut = setup_contract::roll_over(
            self.send_to_taker_actor.sink().with(move |msg| {
                future::ok(maker_inc_connections::TakerMessage {
                    taker_id,
                    msg: wire::MakerToTaker::RollOverProtocol { order_id, msg },
                })
            }),
            receiver,
            (self.oracle_pk, announcement),
            RolloverParams::new(
                cfd.price,
                cfd.quantity_usd,
                cfd.leverage,
                cfd.refund_timelock_in_blocks(),
                cfd.fee_rate,
            ),
            Role::Maker,
            dlc,
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

    async fn reject(&mut self, ctx: &mut xtra::Context<Self>) -> Result<()> {
        tracing::info!(%self.cfd.id, "Maker rejects a roll_over proposal" );

        self.send_to_taker_actor
            .send(TakerMessage {
                taker_id: self.taker_id,
                msg: MakerToTaker::RejectRollOver(self.cfd.id),
            })
            .await??;
        self.projection_actor
            .send(UpdateRollOverProposal {
                order: self.cfd.id,
                proposal: None,
            })
            .await?;
        ctx.stop();

        Ok(())
    }

    pub async fn forward_protocol_msg(&mut self, msg: ProtocolMsg) -> Result<()> {
        let sender = self
            .sent_from_taker
            .as_mut()
            .context("cannot forward message to rollover task")?;
        sender.send(msg.0).await?;
        Ok(())
    }
}

#[xtra_productivity]
impl Actor {
    async fn handle_accept_rollover(
        &mut self,
        _msg: AcceptRollOver,
        ctx: &mut xtra::Context<Self>,
    ) {
        if let Err(err) = self.accept(ctx).await {
            self.fail(ctx, err).await;
        };
    }

    async fn handle_reject_rollover(
        &mut self,
        _msg: RejectRollOver,
        ctx: &mut xtra::Context<Self>,
    ) {
        if let Err(err) = self.reject(ctx).await {
            self.fail(ctx, err).await;
        };
    }

    async fn handle_protocol_msg(&mut self, msg: ProtocolMsg, ctx: &mut xtra::Context<Self>) {
        if let Err(err) = self.forward_protocol_msg(msg).await {
            self.fail(ctx, err).await;
        };
    }

    async fn handle_rollover_failed(&mut self, msg: RolloverFailed, ctx: &mut xtra::Context<Self>) {
        self.fail(ctx, msg.error).await;
    }

    async fn handle_rollover_succeeded(
        &mut self,
        msg: RolloverSucceeded,
        ctx: &mut xtra::Context<Self>,
    ) {
        if let Err(err) = self.update_contract(msg.dlc.clone(), ctx).await {
            self.fail(ctx, err).await;
        }
    }
}

impl ActorName for Actor {
    fn actor_name() -> String {
        "Maker rollover".to_string()
    }
}
