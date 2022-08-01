mod delete;
mod insert;
mod load;

pub use insert::insert;
pub use load::load;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory;
    use crate::models;
    use anyhow::bail;
    use anyhow::Context;
    use anyhow::Result;
    use bdk::bitcoin::Amount;
    use model::olivia::BitMexPriceEventId;
    use model::Cfd;
    use model::CfdEvent;
    use model::ContractSymbol;
    use model::EventKind;
    use model::FundingRate;
    use model::Leverage;
    use model::OfferId;
    use model::OpeningFee;
    use model::OrderId;
    use model::Position;
    use model::Price;
    use model::Role;
    use model::Timestamp;
    use model::TxFeeRate;
    use model::Usd;
    use rust_decimal_macros::dec;
    use sqlx::pool::PoolConnection;
    use sqlx::Connection;
    use sqlx::Sqlite;
    use time::macros::datetime;
    use time::Duration;
    use time::OffsetDateTime;

    pub fn dummy_cfd() -> Cfd {
        Cfd::new(
            OrderId::default(),
            OfferId::default(),
            Position::Long,
            Price::new(dec!(60_000)).unwrap(),
            Leverage::TWO,
            Duration::hours(24),
            Role::Taker,
            Usd::new(dec!(1_000)),
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"
                .parse()
                .unwrap(),
            None,
            OpeningFee::new(Amount::from_sat(2000)),
            FundingRate::default(),
            TxFeeRate::default(),
            ContractSymbol::BtcUsd,
        )
    }

    #[tokio::test]
    async fn test_insert_rollover_completed_event_data_should_not_error() {
        let db = memory().await.unwrap();
        let cfd = dummy_cfd();
        db.insert_cfd(&cfd).await.unwrap();
        let timestamp = Timestamp::now();
        let event = std::fs::read_to_string("./src/test_events/rollover_completed.json").unwrap();
        let event = serde_json::from_str::<EventKind>(&event).unwrap();
        let rollover_completed = CfdEvent {
            timestamp,
            id: cfd.id(),
            event,
        };

        let mut connection = db.inner.acquire().await.unwrap();
        db.append_event(rollover_completed.clone()).await.unwrap();
        insert(&mut connection, 1, rollover_completed.clone())
            .await
            .unwrap();

        let (rollovers, revokes, cets) = count_table_entries(connection).await;
        assert_eq!(rollovers, 1);
        assert_eq!(revokes, 2);
        assert_eq!(cets, 2);
    }

    #[tokio::test]
    async fn repeatedly_insert_rollover_completed_event_data_should_not_error() -> Result<()> {
        let db = memory().await?;

        let cfd = dummy_cfd();
        db.insert_cfd(&cfd).await?;

        let timestamp = Timestamp::now();

        let event = std::fs::read_to_string("./src/test_events/rollover_completed.json")?;
        let event = serde_json::from_str::<EventKind>(&event)?;

        let rollover_completed = CfdEvent {
            timestamp,
            id: cfd.id(),
            event: event.clone(),
        };

        // insert first rollovercompleted event
        let mut connection = db.inner.acquire().await?;
        db.append_event(rollover_completed.clone()).await?;
        insert(&mut connection, 1, rollover_completed.clone()).await?;

        // insert second rollovercompleted event with different event id
        let rollover_completed = update_event_id(
            timestamp,
            event,
            datetime!(2021-06-01 10:00:00).assume_utc(),
            cfd.id(),
        )?;
        let mut connection = db.inner.acquire().await?;
        db.append_event(rollover_completed.clone()).await?;
        insert(&mut connection, 2, rollover_completed).await?;

        let (rollovers, revokes, cets) = count_table_entries(connection).await;
        assert_eq!(rollovers, 1);
        assert_eq!(revokes, 2);
        assert_eq!(cets, 2);

        Ok(())
    }

    #[tokio::test]
    async fn given_one_rollover_event_load_should_not_error() -> Result<()> {
        let db = memory().await?;

        let cfd = dummy_cfd();
        db.insert_cfd(&cfd).await?;

        let timestamp = Timestamp::now();

        let event = std::fs::read_to_string("./src/test_events/rollover_completed.json")?;
        let event = serde_json::from_str::<EventKind>(&event)?;

        let rollover_completed = CfdEvent {
            timestamp,
            id: cfd.id(),
            event,
        };

        db.append_event(rollover_completed.clone()).await?;
        let mut connection = db.inner.acquire().await?;
        insert(&mut connection, 1, rollover_completed.clone()).await?;

        let order_id = models::OrderId::from(cfd.id());

        let cfd_row_id = sqlx::query!(r#"select id from cfds where order_id = $1"#, order_id)
            .fetch_one(&mut connection)
            .await?
            .id
            .unwrap();

        let mut transaction = connection.begin().await?;

        let (loaded_dlc, loaded_funding_fee, loaded_complete_fee) =
            load(&mut transaction, cfd_row_id, 1)
                .await?
                .context("Expect to find data")?;

        match rollover_completed.event {
            EventKind::RolloverCompleted {
                dlc: Some(dlc),
                funding_fee,
                complete_fee,
            } => {
                // dlc does not implement eq hence we only assert on the event id which is
                // sufficient because we only expect to have 1 item in the db
                assert_eq!(loaded_dlc.settlement_event_id, dlc.settlement_event_id);
                assert_eq!(loaded_funding_fee, funding_fee);
                assert_eq!(loaded_complete_fee, complete_fee)
            }
            _ => {
                bail!("We should always have a RolloverCompletedEvent")
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn when_having_two_rollovers_should_load_last() -> Result<()> {
        let db = memory().await?;
        let cfd = dummy_cfd();
        db.insert_cfd(&cfd).await?;
        let timestamp = Timestamp::now();
        let event = std::fs::read_to_string("./src/test_events/rollover_completed.json")?;
        let event = serde_json::from_str::<EventKind>(&event)?;
        let rollover_completed = CfdEvent {
            timestamp,
            id: cfd.id(),
            event: event.clone(),
        };

        // insert first RolloverCompleted event data
        let mut connection = db.inner.acquire().await?;
        db.append_event(rollover_completed.clone()).await?;
        insert(&mut connection, 1, rollover_completed.clone()).await?;

        // insert second RolloverCompleted event data
        let second_rollover_completed_event = update_event_id(
            timestamp,
            event,
            datetime!(2021-06-01 10:00:00).assume_utc(),
            cfd.id(),
        )?;
        let mut connection = db.inner.acquire().await.unwrap();
        db.append_event(second_rollover_completed_event.clone())
            .await
            .unwrap();
        insert(&mut connection, 2, second_rollover_completed_event.clone())
            .await
            .unwrap();

        let order_id = models::OrderId::from(cfd.id());

        let cfd_row_id = sqlx::query!(r#"select id from cfds where order_id = $1"#, order_id)
            .fetch_one(&mut connection)
            .await?
            .id
            .unwrap();

        let mut transaction = connection.begin().await?;
        let (loaded_dlc, loaded_funding_fee, loaded_complete_fee) =
            load(&mut transaction, cfd_row_id, 2)
                .await?
                .context("Expect to find data")?;

        match second_rollover_completed_event.event {
            EventKind::RolloverCompleted {
                dlc: Some(dlc),
                funding_fee,
                complete_fee,
            } => {
                // dlc does not implement eq hence we only assert on the event id which is
                // sufficient because we only expect to have 1 item in the db
                assert_eq!(loaded_dlc.settlement_event_id, dlc.settlement_event_id);
                assert_eq!(loaded_funding_fee, funding_fee);
                assert_eq!(loaded_complete_fee, complete_fee);
            }
            _ => {
                bail!("We should always have a RolloverCompletedEvent")
            }
        }

        Ok(())
    }

    async fn count_table_entries(mut connection: PoolConnection<Sqlite>) -> (i32, i32, i32) {
        let row = sqlx::query!(
            r#"
            SELECT
                COUNT(DISTINCT rollover_completed_event_data.id) as rollovers,
                COUNT(DISTINCT revoked_commit_transactions.id) as revokes,
                COUNT(DISTINCT open_cets.id) as cets
            FROM
                rollover_completed_event_data,
                revoked_commit_transactions,
                open_cets;
            "#
        )
        .fetch_one(&mut connection)
        .await
        .unwrap();
        (
            row.rollovers,
            // Not sure why these are options
            row.revokes.unwrap(),
            row.cets.unwrap(),
        )
    }

    fn update_event_id(
        timestamp: Timestamp,
        event: EventKind,
        settlement_event_timestamp: OffsetDateTime,
        id: OrderId,
    ) -> Result<CfdEvent> {
        match event {
            EventKind::RolloverCompleted {
                dlc: Some(mut dlc),
                funding_fee,
                complete_fee,
            } => {
                dlc.settlement_event_id =
                    BitMexPriceEventId::with_20_digits(settlement_event_timestamp);

                Ok(CfdEvent {
                    timestamp,
                    id,
                    event: EventKind::RolloverCompleted {
                        dlc: Some(dlc),
                        funding_fee,
                        complete_fee,
                    },
                })
            }
            _ => {
                bail!("We should always have a RolloverCompleted event")
            }
        }
    }
}
