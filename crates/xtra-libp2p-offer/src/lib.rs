mod current;
pub mod deprecated;

pub use current::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::taker::LatestOffers;
    use async_trait::async_trait;
    use futures::Future;
    use model::olivia::BitMexPriceEventId;
    use model::ContractSymbol;
    use model::Contracts;
    use model::FundingRate;
    use model::Leverage;
    use model::LotSize;
    use model::Position;
    use model::Price;
    use model::Timestamp;
    use model::TxFeeRate;
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;
    use std::collections::HashSet;
    use std::sync::Arc;
    use std::time::Duration;
    use time::macros::datetime;
    use tracing_subscriber::util::SubscriberInitExt;
    use xtra::spawn::TokioGlobalSpawnExt;
    use xtra::Actor as _;
    use xtra::Address;
    use xtra::Context;
    use xtra_libp2p::endpoint::Subscribers;
    use xtra_libp2p::libp2p::identity::Keypair;
    use xtra_libp2p::libp2p::multiaddr::Protocol;
    use xtra_libp2p::libp2p::transport::MemoryTransport;
    use xtra_libp2p::libp2p::Multiaddr;
    use xtra_libp2p::libp2p::PeerId;
    use xtra_libp2p::Connect;
    use xtra_libp2p::Endpoint;
    use xtra_libp2p::ListenOn;
    use xtra_productivity::xtra_productivity;

    #[tokio::test]
    async fn given_new_offers_then_received_offers_match_originals() {
        let _g = tracing_subscriber::fmt()
            .with_env_filter("xtra_libp2p_offer=trace")
            .with_test_writer()
            .set_default();

        let (maker_peer_id, maker_offer_addr, maker_endpoint_addr) =
            create_endpoint_with_offer_maker();
        let (offer_receiver_addr, taker_endpoint_addr) = create_endpoint_with_offer_taker();

        maker_endpoint_addr
            .send(ListenOn(Multiaddr::empty().with(Protocol::Memory(1000))))
            .await
            .unwrap();
        taker_endpoint_addr
            .send(Connect(
                Multiaddr::empty()
                    .with(Protocol::Memory(1000))
                    .with(Protocol::P2p(maker_peer_id.into())),
            ))
            .await
            .unwrap()
            .unwrap();

        let new_offers = dummy_offers();

        // maker keeps sending the offers until the taker establishes
        // a connection
        #[allow(clippy::disallowed_methods)]
        tokio::spawn({
            let new_offers = new_offers.clone();
            async move {
                loop {
                    maker_offer_addr
                        .send(crate::maker::NewOffers::new(new_offers.clone()))
                        .await
                        .unwrap();

                    tokio_extras::time::sleep(Duration::from_millis(200)).await;
                }
            }
        });

        // taker retries until the connection is established and we
        // get the maker's latest offers
        let received_offers = retry_until_some(|| {
            let offer_receiver_addr = offer_receiver_addr.clone();
            async move { offer_receiver_addr.send(GetLatestOffers).await.unwrap() }
        })
        .await;

        assert_eq!(new_offers, received_offers)
    }

    #[tokio::test]
    async fn given_taker_connects_then_taker_receives_all_current_offers() {
        let _g = tracing_subscriber::fmt()
            .with_env_filter("xtra_libp2p_offer=trace")
            .with_test_writer()
            .set_default();

        let (maker_peer_id, maker_offer_addr, maker_endpoint_addr) =
            create_endpoint_with_offer_maker();
        let (offer_receiver_addr, taker_endpoint_addr) = create_endpoint_with_offer_taker();

        maker_endpoint_addr
            .send(ListenOn(Multiaddr::empty().with(Protocol::Memory(1000))))
            .await
            .unwrap();

        let offer_btc_usd_long = dummy_offer(ContractSymbol::BtcUsd, Position::Long);
        maker_offer_addr
            .send(crate::maker::NewOffers::new(vec![
                offer_btc_usd_long.clone()
            ]))
            .await
            .unwrap();

        let offer_eth_usd_short = dummy_offer(ContractSymbol::EthUsd, Position::Short);
        maker_offer_addr
            .send(crate::maker::NewOffers::new(vec![
                offer_eth_usd_short.clone()
            ]))
            .await
            .unwrap();

        taker_endpoint_addr
            .send(Connect(
                Multiaddr::empty()
                    .with(Protocol::Memory(1000))
                    .with(Protocol::P2p(maker_peer_id.into())),
            ))
            .await
            .unwrap()
            .unwrap();

        // taker retries until the connection is established and we
        // get the maker's latest offers
        let received_offers = retry_until_some(|| {
            let offer_receiver_addr = offer_receiver_addr.clone();
            async move { offer_receiver_addr.send(GetLatestOffers).await.unwrap() }
        })
        .await;

        assert_eq!(received_offers.len(), 2);
        assert!(received_offers.contains(&offer_btc_usd_long));
        assert!(received_offers.contains(&offer_eth_usd_short));
    }

    fn create_endpoint_with_offer_maker(
    ) -> (PeerId, Address<crate::maker::Actor>, Address<Endpoint>) {
        let (endpoint_addr, endpoint_context) = Context::new(None);

        let id = Keypair::generate_ed25519();
        let offer_maker_addr = crate::maker::Actor::new(endpoint_addr.clone())
            .create(None)
            .spawn_global();

        let endpoint = Endpoint::new(
            Box::new(MemoryTransport::default),
            id.clone(),
            Duration::from_secs(10),
            [],
            Subscribers::new(
                vec![offer_maker_addr.clone().into()],
                vec![offer_maker_addr.clone().into()],
                vec![],
                vec![],
            ),
            Arc::new(HashSet::default()),
        );

        #[allow(clippy::disallowed_methods)]
        tokio::spawn(endpoint_context.run(endpoint));

        (id.public().to_peer_id(), offer_maker_addr, endpoint_addr)
    }

    fn create_endpoint_with_offer_taker() -> (Address<OffersReceiver>, Address<Endpoint>) {
        let offers_receiver_addr = OffersReceiver::new().create(None).spawn_global();

        let offer_taker_addr = crate::taker::Actor::new(offers_receiver_addr.clone().into())
            .create(None)
            .spawn_global();

        let endpoint_addr = Endpoint::new(
            Box::new(MemoryTransport::default),
            Keypair::generate_ed25519(),
            Duration::from_secs(10),
            [(PROTOCOL, offer_taker_addr.into())],
            Subscribers::default(),
            Arc::new(HashSet::default()),
        )
        .create(None)
        .spawn_global();

        (offers_receiver_addr, endpoint_addr)
    }

    struct OffersReceiver {
        offers: Vec<model::Offer>,
    }

    impl OffersReceiver {
        fn new() -> Self {
            Self { offers: Vec::new() }
        }
    }

    #[async_trait]
    impl xtra::Actor for OffersReceiver {
        type Stop = ();

        async fn stopped(self) -> Self::Stop {}
    }

    #[xtra_productivity]
    impl OffersReceiver {
        async fn handle(&mut self, msg: LatestOffers) {
            self.offers = msg.0;
        }
    }

    struct GetLatestOffers;

    #[xtra_productivity]
    impl OffersReceiver {
        async fn handle(&mut self, _: GetLatestOffers) -> Vec<model::Offer> {
            self.offers.clone()
        }
    }

    async fn retry_until_some<F, FUT>(mut fut: F) -> Vec<model::Offer>
    where
        F: FnMut() -> FUT,
        FUT: Future<Output = Vec<model::Offer>>,
    {
        loop {
            let offers = fut().await;

            if offers.is_empty() {
                tokio_extras::time::sleep(Duration::from_millis(200)).await;
            } else {
                return offers;
            }
        }
    }

    pub fn dummy_offers() -> Vec<model::Offer> {
        vec![
            dummy_offer(ContractSymbol::BtcUsd, Position::Long),
            dummy_offer(ContractSymbol::BtcUsd, Position::Short),
        ]
    }

    fn dummy_offer(contract_symbol: ContractSymbol, position_maker: Position) -> model::Offer {
        model::Offer {
            id: Default::default(),
            contract_symbol,
            position_maker,
            price: Price::new(dec!(1000)).unwrap(),
            min_quantity: Contracts::new(100),
            max_quantity: Contracts::new(1000),
            leverage_choices: vec![Leverage::TWO],
            creation_timestamp_maker: Timestamp::now(),
            settlement_interval: time::Duration::hours(24),
            oracle_event_id: BitMexPriceEventId::with_20_digits(
                datetime!(2021-10-04 22:00:00).assume_utc(),
                contract_symbol,
            ),
            tx_fee_rate: TxFeeRate::default(),
            funding_rate: FundingRate::new(Decimal::ONE).unwrap(),
            opening_fee: Default::default(),
            lot_size: LotSize::new(100),
        }
    }
}
