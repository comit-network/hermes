use anyhow::Result;
use bdk::bitcoin;
use bdk::bitcoin::secp256k1;
use bdk::bitcoin::secp256k1::SecretKey;
use bdk::bitcoin::secp256k1::SECP256K1;
use bdk::bitcoin::util::bip32::ExtendedPrivKey;
use bdk::bitcoin::Amount;
use bdk::bitcoin::Network;
use rand::CryptoRng;
use rand::RngCore;

pub mod keypair;

pub fn new_test_wallet(
    rng: &mut (impl RngCore + CryptoRng),
    utxo_amount: Amount,
    num_utxos: u8,
) -> Result<bdk::Wallet<(), bdk::database::MemoryDatabase>> {
    use bdk::populate_test_db;
    use bdk::testutils;

    let mut seed = [0u8; 32];
    rng.fill_bytes(&mut seed);

    let key = ExtendedPrivKey::new_master(Network::Regtest, &seed)?;
    let descriptors = testutils!(@descriptors (&format!("wpkh({key}/*)")));

    let mut database = bdk::database::MemoryDatabase::new();

    for index in 0..num_utxos {
        populate_test_db!(
            &mut database,
            testutils! {
                @tx ( (@external descriptors, index as u32) => utxo_amount.as_sat() ) (@confirmations 1)
            },
            Some(100)
        );
    }

    let wallet = bdk::Wallet::new_offline(&descriptors.0, None, Network::Regtest, database)?;

    Ok(wallet)
}

pub trait AddressExt {
    fn random() -> Self;
}

impl AddressExt for bdk::bitcoin::Address {
    fn random() -> Self {
        let pk = {
            let sk = secp256k1::SecretKey::new(&mut rand::thread_rng());
            bitcoin::PublicKey::new(sk.to_public_key())
        };

        bdk::bitcoin::Address::p2wpkh(&pk, Network::Regtest).unwrap()
    }
}

pub trait SecretKeyExt {
    fn to_public_key(self) -> secp256k1::PublicKey;
}

impl SecretKeyExt for SecretKey {
    fn to_public_key(self) -> secp256k1::PublicKey {
        secp256k1::PublicKey::from_secret_key(SECP256K1, &self)
    }
}
