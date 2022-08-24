use daemon::projection::CfdOffer;
use daemon::projection::MakerOffers;
use daemon_tests::flow::ensure_null_next_offers;
use daemon_tests::flow::next_maker_offers;
use daemon_tests::start_both;
use daemon_tests::OfferParamsBuilder;
use model::ContractSymbol;
use model::Leverage;
use otel_tests::otel_test;

#[otel_test]
async fn taker_receives_offer_from_maker_on_publication() {
    let (mut maker, mut taker) = start_both().await;

    ensure_null_next_offers(taker.offers_feed()).await.unwrap();

    let leverage = Leverage::TWO;
    maker
        .set_offer_params(
            OfferParamsBuilder::new()
                .leverage_choices(vec![leverage])
                .build(),
        )
        .await;

    let (published, received) = next_maker_offers(
        maker.offers_feed(),
        taker.offers_feed(),
        &ContractSymbol::BtcUsd,
    )
    .await
    .unwrap();

    assert_eq_offers(published, received);
}

fn assert_eq_offers(published: MakerOffers, received: MakerOffers) {
    assert_eq_offer(published.btcusd_long, received.btcusd_long);
    assert_eq_offer(published.btcusd_short, received.btcusd_short);
    assert_eq_offer(published.ethusd_long, received.ethusd_long);
    assert_eq_offer(published.ethusd_short, received.ethusd_short);
}

/// Helper function to compare a maker's `CfdOffer` against the taker's corresponding `CfdOffer`.
///
/// Unfortunately, we cannot simply use `assert_eq!` because part of the `CfdOffer` is
/// position-depedent.
fn assert_eq_offer(published: Option<CfdOffer>, received: Option<CfdOffer>) {
    let (mut published, mut received) = match (published, received) {
        (None, None) => return,
        (Some(published), Some(received)) => (published, received),
        (published, received) => {
            panic!("Offer mismatch. Maker published {published:?}, taker received {received:?}")
        }
    };

    // Comparing `LeverageDetails` straight up will fail because the values in them depend on each
    // party's position. Therefore, we need to assert against things carefully
    {
        for (leverage_details_published_i, leverage_details_received_i) in published
            .leverage_details
            .iter()
            .zip(received.leverage_details.iter())
        {
            // We can expect the absolute values of the initial funding fee per lot to be the same
            // per leverage for both parties
            let initial_funding_fee_per_lot_maker =
                leverage_details_published_i.initial_funding_fee_per_lot;
            let initial_funding_fee_per_lot_taker =
                leverage_details_received_i.initial_funding_fee_per_lot;
            assert_eq!(
                initial_funding_fee_per_lot_maker.abs(),
                initial_funding_fee_per_lot_taker.abs()
            );
        }

        // As a last step, we delete the data from `leverage_details` for both parties so that the
        // final assertion on the entire `CfdOffer` has a chance of succeeding
        published.leverage_details = Vec::new();
        received.leverage_details = Vec::new();
    }

    assert_eq!(published, received);
}
