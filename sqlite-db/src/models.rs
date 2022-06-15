use bdk::bitcoin;
use maia_core::secp256k1_zkp;
use model::impl_sqlx_type_display_from_str;
use serde::de::Error;
use serde::Deserialize;
use serde::Serialize;
use sqlx::types::uuid::adapter::Hyphenated;
use sqlx::types::Uuid;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, sqlx::Type)]
#[sqlx(transparent)]
pub struct OrderId(Hyphenated);

impl Serialize for OrderId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for OrderId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let uuid = String::deserialize(deserializer)?;
        let uuid = uuid.parse::<Uuid>().map_err(D::Error::custom)?;

        Ok(Self(uuid.to_hyphenated()))
    }
}

impl Default for OrderId {
    fn default() -> Self {
        Self(Uuid::new_v4().to_hyphenated())
    }
}

impl fmt::Display for OrderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<model::OrderId> for OrderId {
    fn from(id: model::OrderId) -> Self {
        OrderId(Uuid::from(id).to_hyphenated())
    }
}

impl From<OrderId> for model::OrderId {
    fn from(id: OrderId) -> Self {
        let id = Uuid::from_str(id.0.to_string().as_str())
            .expect("Safe conversion from one uuid format to another");
        model::OrderId::from(id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SecretKey(secp256k1_zkp::key::SecretKey);

impl fmt::Display for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for SecretKey {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let sk = secp256k1_zkp::key::SecretKey::from_str(s)?;
        Ok(Self(sk))
    }
}

impl From<SecretKey> for secp256k1_zkp::key::SecretKey {
    fn from(sk: SecretKey) -> Self {
        sk.0
    }
}

impl From<secp256k1_zkp::key::SecretKey> for SecretKey {
    fn from(key: secp256k1_zkp::key::SecretKey) -> Self {
        Self(key)
    }
}

impl_sqlx_type_display_from_str!(SecretKey);

/// Role in the Cfd
#[derive(Debug, Copy, Clone, PartialEq, sqlx::Type)]
pub enum Role {
    Maker,
    Taker,
}

impl From<model::Role> for Role {
    fn from(role: model::Role) -> Self {
        match role {
            model::Role::Maker => Role::Maker,
            model::Role::Taker => Role::Taker,
        }
    }
}

impl From<Role> for model::Role {
    fn from(role: Role) -> Self {
        match role {
            Role::Maker => model::Role::Maker,
            Role::Taker => model::Role::Taker,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PublicKey(bitcoin::util::key::PublicKey);

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for PublicKey {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let pk = bitcoin::util::key::PublicKey::from_str(s)?;
        Ok(Self(pk))
    }
}

impl From<PublicKey> for bitcoin::util::key::PublicKey {
    fn from(pk: PublicKey) -> Self {
        pk.0
    }
}

impl From<bitcoin::util::key::PublicKey> for PublicKey {
    fn from(pk: bitcoin::util::key::PublicKey) -> Self {
        Self(pk)
    }
}

impl_sqlx_type_display_from_str!(PublicKey);
