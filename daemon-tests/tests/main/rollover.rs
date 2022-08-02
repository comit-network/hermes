use daemon::bdk::bitcoin::SignedAmount;
use daemon::bdk::bitcoin::Txid;
use daemon::projection::CfdState;
use daemon_tests::dummy_offer_params;
use daemon_tests::flow::next_with;
use daemon_tests::flow::one_cfd_with_state;
use daemon_tests::maia::OliviaData;
use daemon_tests::mock_oracle_announcements;
use daemon_tests::start_from_open_cfd_state;
use daemon_tests::wait_next_state;
use daemon_tests::FeeCalculator;
use daemon_tests::Maker;
use daemon_tests::Taker;
use model::olivia::BitMexPriceEventId;
use model::OrderId;
use model::Position;
use otel_tests::otel_test;

#[otel_test]
async fn rollover_an_open_cfd_maker_going_short() {
    let (mut maker, mut taker, order_id, fee_calculator) =
        prepare_rollover(Position::Short, OliviaData::example_0()).await;

    // We charge 24 hours for the rollover because that is the fallback strategy if the timestamp of
    // the settlement-event is already expired
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_0(),
        None,
        fee_calculator.complete_fee_for_rollover_hours(24),
    )
    .await;
}

#[otel_test]
async fn rollover_an_open_cfd_maker_going_long() {
    let (mut maker, mut taker, order_id, fee_calculator) =
        prepare_rollover(Position::Long, OliviaData::example_0()).await;

    // We charge 24 hours for the rollover because that is the fallback strategy if the timestamp of
    // the settlement-event is already expired
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_0(),
        None,
        fee_calculator.complete_fee_for_rollover_hours(24),
    )
    .await;
}

#[otel_test]
async fn double_rollover_an_open_cfd() {
    // double rollover ensures that both parties properly succeeded and can do another rollover

    let (mut maker, mut taker, order_id, fee_calculator) =
        prepare_rollover(Position::Short, OliviaData::example_0()).await;

    // We charge 24 hours for the rollover because that is the fallback strategy if the timestamp of
    // the settlement-event is already expired
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_0(),
        None,
        fee_calculator.complete_fee_for_rollover_hours(24),
    )
    .await;

    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_0(),
        None,
        fee_calculator.complete_fee_for_rollover_hours(48),
    )
    .await;
}

#[otel_test]
async fn maker_rejects_rollover_of_open_cfd() {
    let oracle_data = OliviaData::example_0();
    let (mut maker, mut taker, order_id, _) =
        start_from_open_cfd_state(oracle_data.announcements(), Position::Short).await;

    let is_accepting_rollovers = false;
    maker
        .system
        .update_rollover_configuration(is_accepting_rollovers)
        .await
        .unwrap();

    taker
        .trigger_rollover_with_latest_dlc_params(order_id)
        .await;

    wait_next_state!(order_id, maker, taker, CfdState::Open);
}

#[otel_test]
async fn given_rollover_completed_when_taker_fails_rollover_can_retry() {
    let (mut maker, mut taker, order_id, fee_calculator) =
        prepare_rollover(Position::Short, OliviaData::example_0()).await;

    // 1. Do two rollovers
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_1(),
        None,
        fee_calculator.complete_fee_for_rollover_hours(24),
    )
    .await;

    let taker_commit_txid_after_first_rollover = taker.latest_commit_txid();
    let taker_settlement_event_id_after_first_rollover = taker.latest_settlement_event_id();
    let taker_dlc_after_first_rollover = taker.latest_dlc();
    let taker_complete_fee_after_first_rollover = taker.latest_fees();

    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_1(),
        None,
        // The second rollover increases the complete fees to 48h
        fee_calculator.complete_fee_for_rollover_hours(48),
    )
    .await;

    // We simulate the taker being one rollover behind by setting the
    // latest DLC to the one of the first rollover
    taker
        .append_rollover_event(
            order_id,
            taker_dlc_after_first_rollover,
            taker_complete_fee_after_first_rollover,
        )
        .await;

    // 2. Retry the rollover from the first rollover DLC
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_1(),
        Some((
            taker_commit_txid_after_first_rollover,
            taker_settlement_event_id_after_first_rollover,
        )),
        // We expect that the rollover retry won't add additional costs, since we retry from the
        // previous rollover we expect 48h
        fee_calculator.complete_fee_for_rollover_hours(48),
    )
    .await;

    // 3. Ensure that we can do another rollover after the retry
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_1(),
        None,
        // Additional rollover increases the complete fees to 72h
        fee_calculator.complete_fee_for_rollover_hours(72),
    )
    .await;
}

#[otel_test]
async fn given_contract_setup_completed_when_taker_fails_first_rollover_can_retry() {
    let (mut maker, mut taker, order_id, fee_calculator) =
        prepare_rollover(Position::Short, OliviaData::example_0()).await;

    let taker_commit_txid_after_contract_setup = taker.latest_commit_txid();
    let taker_settlement_event_id_after_contract_setup = taker.latest_settlement_event_id();
    let taker_dlc_after_contract_setup = taker.latest_dlc();
    let taker_complete_fee_after_contract_setup = taker.latest_fees();

    // 1. Do a rollover
    // For the first rollover we expect to be charged 24h
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_1(),
        None,
        fee_calculator.complete_fee_for_rollover_hours(24),
    )
    .await;

    // 2. Retry the rollover
    // We simulate the taker being one rollover behind by setting the
    // latest DLC to the one generated by contract setup
    taker
        .append_rollover_event(
            order_id,
            taker_dlc_after_contract_setup,
            taker_complete_fee_after_contract_setup,
        )
        .await;

    // When retrying the rollover we expect to be charged the same amount (i.e. 24h, no fee
    // increase)
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_1(),
        Some((
            taker_commit_txid_after_contract_setup,
            taker_settlement_event_id_after_contract_setup,
        )),
        // Only one term of 24h is charged, so the expected fees are for 24h.
        // This is due to the rollover falling back to charging one full term if the event is
        // already past expiry.
        fee_calculator.complete_fee_for_rollover_hours(24),
    )
    .await;

    // 3. Ensure that we can do another rollover after the retry
    // After another rollover we expect to be charged for 48h
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_1(),
        None,
        fee_calculator.complete_fee_for_rollover_hours(48),
    )
    .await;
}

#[otel_test]
async fn given_contract_setup_completed_when_taker_fails_two_rollovers_can_retry() {
    let (mut maker, mut taker, order_id, fee_calculator) =
        prepare_rollover(Position::Short, OliviaData::example_0()).await;

    let taker_commit_txid_after_contract_setup = taker.latest_commit_txid();
    let taker_settlement_event_id_after_contract_setup = taker.latest_settlement_event_id();
    let taker_dlc_after_contract_setup = taker.latest_dlc();
    let taker_complete_fee_after_contract_setup = taker.latest_fees();

    // 1. Do two rollovers
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_1(),
        None,
        fee_calculator.complete_fee_for_rollover_hours(24),
    )
    .await;

    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_1(),
        None,
        // The second rollover increases the complete fees to 48h
        fee_calculator.complete_fee_for_rollover_hours(48),
    )
    .await;

    // 2. Retry the rollover from contract setup, i.e. both rollovers are discarded, we go back to
    // the initial DLC state We simulate the taker being two rollover behind by setting the
    // latest DLC to the one generated by contract setup
    taker
        .append_rollover_event(
            order_id,
            taker_dlc_after_contract_setup,
            taker_complete_fee_after_contract_setup,
        )
        .await;

    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_1(),
        Some((
            taker_commit_txid_after_contract_setup,
            taker_settlement_event_id_after_contract_setup,
        )),
        // The expected to be charged for 24h because we only charge one full term
        // This is due to the rollover falling back to charging one full term if the event is
        // already past expiry.
        fee_calculator.complete_fee_for_rollover_hours(24),
    )
    .await;

    // 3. Ensure that we can do another rollover after the retry
    rollover(
        &mut maker,
        &mut taker,
        order_id,
        OliviaData::example_1(),
        None,
        // After another rollover we expect to be charged for 48h
        fee_calculator.complete_fee_for_rollover_hours(48),
    )
    .await;
}

/// Set up a CFD that can be rolled over
///
/// Starts maker and taker with an open CFD.
/// Sets offer on the maker side because in order to roll over the maker needs an active offer for
/// up-to-date fee calculation.
/// Asserts that initially both parties don't have funding costs.
async fn prepare_rollover(
    maker_position: Position,
    oracle_data: OliviaData,
) -> (Maker, Taker, OrderId, FeeCalculator) {
    let (mut maker, mut taker, order_id, fee_calculator) =
        start_from_open_cfd_state(oracle_data.announcements(), maker_position).await;

    // Maker needs to have an active offer in order to accept rollover
    maker
        .set_offer_params(dummy_offer_params(maker_position))
        .await;

    let maker_cfd = maker.first_cfd();
    let taker_cfd = taker.first_cfd();

    let (expected_maker_fee, expected_taker_fee) =
        fee_calculator.complete_fee_for_rollover_hours(0);
    assert_eq!(expected_maker_fee, maker_cfd.accumulated_fees);
    assert_eq!(expected_taker_fee, taker_cfd.accumulated_fees);

    (maker, taker, order_id, fee_calculator)
}

async fn rollover(
    maker: &mut Maker,
    taker: &mut Taker,
    order_id: OrderId,
    oracle_data: OliviaData,
    from_params_taker: Option<(Txid, BitMexPriceEventId)>,
    (expected_fees_after_rollover_maker, expected_fees_after_rollover_taker): (
        SignedAmount,
        SignedAmount,
    ),
) {
    // make sure the expected oracle data is mocked
    mock_oracle_announcements(maker, taker, oracle_data.announcements()).await;

    let commit_tx_id_before_rollover_maker = maker.latest_commit_txid();
    let commit_tx_id_before_rollover_taker = taker.latest_commit_txid();

    match from_params_taker {
        None => {
            taker
                .trigger_rollover_with_latest_dlc_params(order_id)
                .await;
        }
        Some((from_commit_txid, from_settlement_event_id)) => {
            taker
                .trigger_rollover_with_specific_params(
                    order_id,
                    from_commit_txid,
                    from_settlement_event_id,
                )
                .await;
        }
    }

    wait_next_state!(order_id, maker, taker, CfdState::RolloverSetup);
    wait_next_state!(order_id, maker, taker, CfdState::Open);

    let maker_cfd = maker.first_cfd();
    let taker_cfd = taker.first_cfd();

    assert_eq!(
        expected_fees_after_rollover_maker, maker_cfd.accumulated_fees,
        "Maker's fees don't match predicted fees after rollover"
    );
    assert_eq!(
        expected_fees_after_rollover_taker, taker_cfd.accumulated_fees,
        "Taker's fees don't match predicted fees after rollover"
    );

    // Ensure that the event ID of the latest dlc is the event ID used for rollover
    assert_eq!(
        oracle_data.settlement_announcement().id,
        maker_cfd
            .aggregated()
            .latest_dlc()
            .as_ref()
            .unwrap()
            .settlement_event_id,
        "Taker's latest event-id does not match given event-id after rollover"
    );
    assert_eq!(
        oracle_data.settlement_announcement().id,
        taker_cfd
            .aggregated()
            .latest_dlc()
            .as_ref()
            .unwrap()
            .settlement_event_id,
        "Taker's latest event-id does not match given event-id after rollover"
    );

    assert_ne!(
        commit_tx_id_before_rollover_maker,
        maker.latest_commit_txid(),
        "The commit_txid of the taker should have changed after the rollover"
    );

    assert_ne!(
        commit_tx_id_before_rollover_taker,
        taker.latest_commit_txid(),
        "The commit_txid of the maker should have changed after the rollover"
    );

    assert_eq!(
        taker.latest_commit_txid(),
        maker.latest_commit_txid(),
        "The maker and taker should have the same commit_txid after the rollover"
    );
}
