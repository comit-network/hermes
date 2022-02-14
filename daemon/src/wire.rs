use crate::noise::NOISE_MAX_MSG_LEN;
use crate::noise::NOISE_TAG_LEN;
use crate::olivia::BitMexPriceEventId;
use anyhow::bail;
use anyhow::Result;
use bdk::bitcoin::secp256k1::Signature;
use bdk::bitcoin::util::psbt::PartiallySignedTransaction;
use bdk::bitcoin::Address;
use bdk::bitcoin::Amount;
use bdk::bitcoin::PublicKey;
use bytes::BytesMut;
use futures::stream::SplitSink;
use futures::stream::SplitStream;
use maia::secp256k1_zkp::EcdsaAdaptorSignature;
use maia::secp256k1_zkp::SecretKey;
use maia::CfdTransactions;
use maia::PartyParams;
use maia::PunishParams;
use model::cfd::Order;
use model::cfd::OrderId;
use model::FundingRate;
use model::Price;
use model::Timestamp;
use model::TxFeeRate;
use model::Usd;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use snow::TransportState;
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;
use std::ops::RangeInclusive;
use tokio::net::TcpStream;
use tokio_util::codec::Decoder;
use tokio_util::codec::Encoder;
use tokio_util::codec::Framed;
use tokio_util::codec::LengthDelimitedCodec;

pub type Read<D, E> = SplitStream<Framed<TcpStream, EncryptedJsonCodec<D, E>>>;
pub type Write<D, E> = SplitSink<Framed<TcpStream, EncryptedJsonCodec<D, E>>, E>;

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, PartialOrd)]
pub struct Version(semver::Version);

impl Version {
    pub fn current() -> Self {
        Self(semver::Version::new(2, 0, 0))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

pub mod taker_to_maker {
    use super::*;

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "type", content = "payload")]
    #[allow(clippy::large_enum_variant)]
    pub enum Settlement {
        Propose {
            timestamp: Timestamp,
            #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc")]
            taker: Amount,
            #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc")]
            maker: Amount,
            price: Price,
        },
        Initiate {
            sig_taker: Signature,
        },
    }

    impl fmt::Display for Settlement {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Settlement::Propose { .. } => write!(f, "Propose"),
                Settlement::Initiate { .. } => write!(f, "Initiate"),
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
#[allow(clippy::large_enum_variant)]
pub enum TakerToMaker {
    Hello(Version),
    TakeOrder {
        order_id: OrderId,
        quantity: Usd,
    },
    ProposeRollover {
        order_id: OrderId,
        timestamp: Timestamp,
    },
    Protocol {
        order_id: OrderId,
        msg: SetupMsg,
    },
    RolloverProtocol {
        order_id: OrderId,
        msg: RolloverMsg,
    },
    Settlement {
        order_id: OrderId,
        msg: taker_to_maker::Settlement,
    },
}

impl fmt::Display for TakerToMaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TakerToMaker::TakeOrder { .. } => write!(f, "TakeOrder"),
            TakerToMaker::Protocol { msg, .. } => write!(f, "Protocol::{msg}"),
            TakerToMaker::ProposeRollover { .. } => write!(f, "ProposeRollover"),
            TakerToMaker::RolloverProtocol { msg, .. } => write!(f, "RolloverProtocol::{msg}"),
            TakerToMaker::Settlement { msg, .. } => write!(f, "Settlement::{msg}"),
            TakerToMaker::Hello(_) => write!(f, "Hello"),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
#[allow(clippy::large_enum_variant)]
pub enum MakerToTaker {
    Hello(Version),
    /// Periodically broadcasted message, indicating maker's presence
    Heartbeat,
    CurrentOrder(Option<Order>),
    ConfirmOrder(OrderId),
    RejectOrder(OrderId),
    InvalidOrderId(OrderId),
    Protocol {
        order_id: OrderId,
        msg: SetupMsg,
    },
    RolloverProtocol {
        order_id: OrderId,
        msg: RolloverMsg,
    },
    ConfirmRollover {
        order_id: OrderId,
        oracle_event_id: BitMexPriceEventId,
        tx_fee_rate: TxFeeRate,
        funding_rate: FundingRate,
    },
    RejectRollover(OrderId),
    Settlement {
        order_id: OrderId,
        msg: maker_to_taker::Settlement,
    },
}

pub mod maker_to_taker {
    use super::*;

    #[derive(Debug, Serialize, Deserialize)]
    #[serde(tag = "type", content = "payload")]
    pub enum Settlement {
        Confirm,
        Reject,
    }

    impl fmt::Display for Settlement {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Settlement::Confirm => write!(f, "Confirm"),
                Settlement::Reject => write!(f, "Reject"),
            }
        }
    }
}

impl fmt::Display for MakerToTaker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MakerToTaker::Hello(_) => write!(f, "Hello"),
            MakerToTaker::Heartbeat { .. } => write!(f, "Heartbeat"),
            MakerToTaker::CurrentOrder(_) => write!(f, "CurrentOrder"),
            MakerToTaker::ConfirmOrder(_) => write!(f, "ConfirmOrder"),
            MakerToTaker::RejectOrder(_) => write!(f, "RejectOrder"),
            MakerToTaker::InvalidOrderId(_) => write!(f, "InvalidOrderId"),
            MakerToTaker::Protocol { msg, .. } => write!(f, "Protocol::{msg}"),
            MakerToTaker::ConfirmRollover { .. } => write!(f, "ConfirmRollover"),
            MakerToTaker::RejectRollover(_) => write!(f, "RejectRollover"),
            MakerToTaker::RolloverProtocol { msg, .. } => write!(f, "RolloverProtocol::{msg}"),
            MakerToTaker::Settlement { msg, .. } => write!(f, "Settlement::{msg}"),
        }
    }
}

/// A codec that can decode encrypted JSON into the type `D` and encode `E` to encrypted JSON.
pub struct EncryptedJsonCodec<D, E> {
    _type: PhantomData<(D, E)>,
    inner: LengthDelimitedCodec,
    transport_state: TransportState,
}

impl<D, E> EncryptedJsonCodec<D, E> {
    pub fn new(transport_state: TransportState) -> Self {
        Self {
            _type: PhantomData,
            inner: LengthDelimitedCodec::new(),
            transport_state,
        }
    }
}

impl<D, E> Decoder for EncryptedJsonCodec<D, E>
where
    D: DeserializeOwned,
{
    type Item = D;
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let bytes = match self.inner.decode(src)? {
            None => return Ok(None),
            Some(bytes) => bytes,
        };

        let decrypted = bytes
            .chunks(NOISE_MAX_MSG_LEN as usize)
            .map(|chunk| {
                let mut buf = vec![0u8; chunk.len() - NOISE_TAG_LEN as usize];
                self.transport_state.read_message(chunk, &mut *buf)?;
                Ok(buf)
            })
            .collect::<Result<Vec<Vec<u8>>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>();

        let item = serde_json::from_slice(&decrypted)?;

        Ok(Some(item))
    }
}

impl<D, E> Encoder<E> for EncryptedJsonCodec<D, E>
where
    E: Serialize,
{
    type Error = anyhow::Error;

    fn encode(&mut self, item: E, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let bytes = serde_json::to_vec(&item)?;

        let encrypted = bytes
            .chunks((NOISE_MAX_MSG_LEN - NOISE_TAG_LEN) as usize)
            .map(|chunk| {
                let mut buf = vec![0u8; chunk.len() + NOISE_TAG_LEN as usize];
                self.transport_state.write_message(chunk, &mut *buf)?;
                Ok(buf)
            })
            .collect::<Result<Vec<Vec<u8>>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<u8>>();

        self.inner.encode(encrypted.into(), dst)?;

        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum SetupMsg {
    /// Message enabling setting up lock and based on that commit, refund and cets
    ///
    /// Each party sends and receives this message.
    /// After receiving this message each party is able to construct the lock transaction.
    Msg0(Msg0),
    /// Message that ensures complete commit, cets and refund transactions
    ///
    /// Each party sends and receives this message.
    /// After receiving this message the commit, refund and cet transactions are complete.
    /// Once verified we can sign and send the lock PSBT.
    Msg1(Msg1),
    /// Message adding signature to the lock PSBT
    ///
    /// Each party sends and receives this message.
    /// Upon receiving this message from the other party we merge our signature and then the lock
    /// tx is fully signed and can be published on chain.
    Msg2(Msg2),
    /// Message acknowledging that we received everything
    ///
    /// Simple ACK message used at the end of the message exchange to ensure that both parties sent
    /// and received everything and we did not run into timeouts on the other side.
    /// This is used to avoid one party publishing the lock transaction while the other party ran
    /// into a timeout.
    Msg3(Msg3),
}

impl fmt::Display for SetupMsg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SetupMsg::Msg0(_) => write!(f, "Msg0"),
            SetupMsg::Msg1(_) => write!(f, "Msg1"),
            SetupMsg::Msg2(_) => write!(f, "Msg2"),
            SetupMsg::Msg3(_) => write!(f, "Msg3"),
        }
    }
}

impl SetupMsg {
    pub fn try_into_msg0(self) -> Result<Msg0> {
        if let Self::Msg0(v) = self {
            Ok(v)
        } else {
            bail!("Not Msg0")
        }
    }

    pub fn try_into_msg1(self) -> Result<Msg1> {
        if let Self::Msg1(v) = self {
            Ok(v)
        } else {
            bail!("Not Msg1")
        }
    }

    pub fn try_into_msg2(self) -> Result<Msg2> {
        if let Self::Msg2(v) = self {
            Ok(v)
        } else {
            bail!("Not Msg2")
        }
    }

    pub fn try_into_msg3(self) -> Result<Msg3> {
        if let Self::Msg3(v) = self {
            Ok(v)
        } else {
            bail!("Not Msg3")
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Msg0 {
    pub lock_psbt: PartiallySignedTransaction, // TODO: Use binary representation
    pub identity_pk: PublicKey,
    #[serde(with = "bdk::bitcoin::util::amount::serde::as_sat")]
    pub lock_amount: Amount,
    pub address: Address,
    pub revocation_pk: PublicKey,
    pub publish_pk: PublicKey,
}

impl From<(PartyParams, PunishParams)> for Msg0 {
    fn from((party, punish): (PartyParams, PunishParams)) -> Self {
        let PartyParams {
            lock_psbt,
            identity_pk,
            lock_amount,
            address,
        } = party;
        let PunishParams {
            revocation_pk,
            publish_pk,
        } = punish;

        Self {
            lock_psbt,
            identity_pk,
            lock_amount,
            address,
            revocation_pk,
            publish_pk,
        }
    }
}

impl From<Msg0> for (PartyParams, PunishParams) {
    fn from(msg0: Msg0) -> Self {
        let Msg0 {
            lock_psbt,
            identity_pk,
            lock_amount,
            address,
            revocation_pk,
            publish_pk,
        } = msg0;

        let party = PartyParams {
            lock_psbt,
            identity_pk,
            lock_amount,
            address,
        };
        let punish = PunishParams {
            revocation_pk,
            publish_pk,
        };

        (party, punish)
    }
}

#[derive(Serialize, Deserialize)]
pub struct Msg1 {
    pub commit: EcdsaAdaptorSignature,
    pub cets: HashMap<String, Vec<(RangeInclusive<u64>, EcdsaAdaptorSignature)>>,
    pub refund: Signature,
}

impl From<CfdTransactions> for Msg1 {
    fn from(txs: CfdTransactions) -> Self {
        let cets = txs
            .cets
            .into_iter()
            .map(|grouped_cets| {
                (
                    grouped_cets.event.id,
                    grouped_cets
                        .cets
                        .into_iter()
                        .map(|(_, encsig, digits)| (digits.range(), encsig))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();
        Self {
            commit: txs.commit.1,
            cets,
            refund: txs.refund.1,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Msg2 {
    pub signed_lock: PartiallySignedTransaction, // TODO: Use binary representation
}

#[derive(Serialize, Deserialize)]
pub struct Msg3;

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum RolloverMsg {
    Msg0(RolloverMsg0),
    Msg1(RolloverMsg1),
    Msg2(RolloverMsg2),
    Msg3(RolloverMsg3),
}

impl fmt::Display for RolloverMsg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RolloverMsg::Msg0(_) => write!(f, "Msg0"),
            RolloverMsg::Msg1(_) => write!(f, "Msg1"),
            RolloverMsg::Msg2(_) => write!(f, "Msg2"),
            RolloverMsg::Msg3(_) => write!(f, "Msg3"),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct RolloverMsg0 {
    pub revocation_pk: PublicKey,
    pub publish_pk: PublicKey,
}

impl RolloverMsg {
    pub fn try_into_msg0(self) -> Result<RolloverMsg0> {
        if let Self::Msg0(v) = self {
            Ok(v)
        } else {
            bail!("Not Msg0")
        }
    }

    pub fn try_into_msg1(self) -> Result<RolloverMsg1> {
        if let Self::Msg1(v) = self {
            Ok(v)
        } else {
            bail!("Not Msg1")
        }
    }

    pub fn try_into_msg2(self) -> Result<RolloverMsg2> {
        if let Self::Msg2(v) = self {
            Ok(v)
        } else {
            bail!("Not Msg2")
        }
    }

    pub fn try_into_msg3(self) -> Result<RolloverMsg3> {
        if let Self::Msg3(v) = self {
            Ok(v)
        } else {
            bail!("Not Msg3")
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct RolloverMsg1 {
    pub commit: EcdsaAdaptorSignature,
    pub cets: HashMap<String, Vec<(RangeInclusive<u64>, EcdsaAdaptorSignature)>>,
    pub refund: Signature,
}

#[derive(Serialize, Deserialize)]
pub struct RolloverMsg2 {
    pub revocation_sk: SecretKey,
}

#[derive(Serialize, Deserialize)]
pub struct RolloverMsg3;

impl From<CfdTransactions> for RolloverMsg1 {
    fn from(txs: CfdTransactions) -> Self {
        let cets = txs
            .cets
            .into_iter()
            .map(|grouped_cets| {
                (
                    grouped_cets.event.id,
                    grouped_cets
                        .cets
                        .into_iter()
                        .map(|(_, encsig, digits)| (digits.range(), encsig))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<HashMap<_, _>>();
        Self {
            commit: txs.commit.1,
            cets,
            refund: txs.refund.1,
        }
    }
}
