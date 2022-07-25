use daemon::projection::CfdState;
use daemon_tests::confirm;
use daemon_tests::expire;
use daemon_tests::flow::next_with;
use daemon_tests::flow::one_cfd_with_state;
use daemon_tests::maia::OliviaData;
use daemon_tests::mocks::oracle::dummy_wrong_attestation;
use daemon_tests::simulate_attestation;
use daemon_tests::start_from_open_cfd_state;
use daemon_tests::wait_next_state;
use model::Position;
use otel_tests::otel_test;
use std::time::Duration;
use tokio_extras::time::sleep;

#[otel_test]
async fn force_close_an_open_cfd_maker_going_short() {
    force_close_open_cfd(Position::Short).await;
}

#[otel_test]
async fn force_close_an_open_cfd_maker_going_long() {
    force_close_open_cfd(Position::Long).await;
}

async fn force_close_open_cfd(maker_position: Position) {
    let oracle_data = OliviaData::example_0();
    let (mut maker, mut taker, order_id, _) =
        start_from_open_cfd_state(oracle_data.announcement(), maker_position).await;
    // Taker initiates force-closing
    taker.system.commit(order_id).await.unwrap();

    confirm!(commit transaction, order_id, maker, taker);
    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::OpenCommitted);

    // After CetTimelockExpired, we're only waiting for attestation
    expire!(cet timelock, order_id, maker, taker);

    // Delivering the wrong attestation does not move state to `PendingCet`
    simulate_attestation!(taker, maker, order_id, dummy_wrong_attestation());

    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::OpenCommitted);

    // Delivering correct attestation moves the state `PendingCet`
    simulate_attestation!(taker, maker, order_id, oracle_data.attestation());

    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::PendingCet);

    confirm!(cet, order_id, maker, taker);
    sleep(Duration::from_secs(5)).await; // need to wait a bit until both transition
    wait_next_state!(order_id, maker, taker, CfdState::Closed);
}
