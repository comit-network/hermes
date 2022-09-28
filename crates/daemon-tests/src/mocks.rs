use crate::dummy_latest_quotes;
use crate::maia::olivia::btc_example_0;
use crate::maia::olivia::eth_example_0;
use crate::mocks::monitor::MockMonitor;
use crate::mocks::oracle::MockOracle;
use crate::mocks::price_feed::MockPriceFeed;
use crate::mocks::wallet::MockWallet;
use model::olivia;
use model::ContractSymbol;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::MutexGuard;
pub mod monitor;
pub mod oracle;
pub mod price_feed;
pub mod wallet;

#[derive(Clone)]
pub struct Mocks {
    wallet: Arc<Mutex<MockWallet>>,
    monitor: Arc<Mutex<MockMonitor>>,
    oracle: Arc<Mutex<MockOracle>>,
    price_feed: Arc<Mutex<MockPriceFeed>>,
}

impl Mocks {
    pub fn new(
        wallet: Arc<Mutex<MockWallet>>,
        price_feed: Arc<Mutex<MockPriceFeed>>,
        monitor: Arc<Mutex<MockMonitor>>,
        oracle: Arc<Mutex<MockOracle>>,
    ) -> Mocks {
        Self {
            wallet,
            monitor,
            oracle,
            price_feed,
        }
    }

    pub async fn wallet(&mut self) -> MutexGuard<'_, MockWallet> {
        self.wallet.lock().await
    }

    pub async fn oracle(&mut self) -> MutexGuard<'_, MockOracle> {
        self.oracle.lock().await
    }

    pub async fn price_feed(&mut self) -> MutexGuard<'_, MockPriceFeed> {
        self.price_feed.lock().await
    }

    pub async fn monitor(&mut self) -> MutexGuard<'_, MockMonitor> {
        self.monitor.lock().await
    }

    // Helper function setting up a "happy path" wallet mock
    pub async fn mock_wallet_sign_and_broadcast(&mut self) {
        self.wallet()
            .await
            .expect_sign()
            .returning(|sign_msg| Ok(sign_msg.psbt));
    }

    pub async fn mock_oracle_announcement(&mut self, symbol: ContractSymbol) {
        let oracle_data = match symbol {
            ContractSymbol::BtcUsd => btc_example_0(),
            ContractSymbol::EthUsd => eth_example_0(),
        };
        self.mock_oracle_announcement_with(oracle_data.announcements())
            .await;
    }

    pub async fn mock_oracle_announcement_with(
        &mut self,
        announcements: Vec<olivia::Announcement>,
    ) {
        self.oracle().await.set_announcements(announcements);
    }

    pub async fn mock_party_params(&mut self) {
        self.wallet()
            .await
            .expect_build_party_params()
            .returning(wallet::build_party_params);
    }

    pub async fn mock_latest_quotes(&mut self) {
        self.price_feed()
            .await
            .set_latest_quotes(dummy_latest_quotes());
    }
}
