use anyhow::Context;
use anyhow::Result;
use futures::future::BoxFuture;
use futures::FutureExt;
use model::CfdEvent;
use model::EventKind;
use model::FundingRate;
use model::Identity;
use model::Leverage;
use model::OpeningFee;
use model::OrderId;
use model::Position;
use model::Price;
use model::Role;
use model::TxFeeRate;
use model::Usd;
use sqlx::migrate::MigrateError;
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgConnectOptions;
use sqlx::PgPool;
use sqlx::Postgres;
use time::Duration;

pub fn connect() -> BoxFuture<'static, Result<PgPool>> {
    async move {
        let pg_connection_options = PgConnectOptions::new()
            .host("localhost")
            //.port(5432)
            //.password("")
            //.ssl_mode(PgSslMode::Require)
            .username("postgres");

        let pool = PgPool::connect_with(pg_connection_options).await?;

        // Attempt to migrate, early return if successful
        let error = match run_migrations_pg(&pool).await {
            Ok(()) => {
                tracing::info!("Opened database");

                return Ok(pool);
            }
            Err(e) => e,
        };

        // Attempt to recover from _some_ problems during migration.
        // These two can happen if someone tampered with the migrations or messed with the DB.
        if let Some(MigrateError::VersionMissing(_) | MigrateError::VersionMismatch(_)) =
            error.downcast_ref::<MigrateError>()
        {
            tracing::error!("{:#}", error);
        }

        Err(error)
    }
    .boxed()
}

pub async fn memory() -> Result<PgPool> {
    // Note: Every :memory: database is distinct from every other. So, opening two database
    // connections each with the filename ":memory:" will create two independent in-memory
    // databases. see: https://www.sqlite.org/inmemorydb.html
    let pool = PgPool::connect(":memory:").await?;

    run_migrations(&pool).await?;

    Ok(pool)
}

async fn run_migrations(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("Failed to run migrations")?;

    Ok(())
}

async fn run_migrations_pg(pool: &PgPool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("Failed to run migrations")?;

    Ok(())
}

pub async fn insert_cfd(cfd: &model::Cfd, conn: &mut PoolConnection<Postgres>) -> Result<()> {
    let query_result = sqlx::query(
        r#"
        insert into cfds (
            uuid,
            position,
            initial_price,
            leverage,
            settlement_time_interval_hours,
            quantity_usd,
            counterparty_network_identity,
            role,
            opening_fee,
            initial_funding_rate,
            initial_tx_fee_rate
        ) values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)"#,
    )
    .bind(&cfd.id())
    .bind(&cfd.position())
    .bind(&cfd.initial_price())
    .bind(&cfd.taker_leverage())
    .bind(&cfd.settlement_time_interval_hours().whole_hours())
    .bind(&cfd.quantity())
    .bind(&cfd.counterparty_network_identity())
    .bind(&cfd.role())
    .bind(&cfd.opening_fee())
    .bind(&cfd.initial_funding_rate())
    .bind(&cfd.initial_tx_fee_rate())
    .execute(conn)
    .await?;

    if query_result.rows_affected() != 1 {
        anyhow::bail!("failed to insert cfd");
    }

    Ok(())
}

/// Appends an event to the `events` table.
///
/// To make handling of `None` events more ergonomic, you can pass anything in here that implements
/// `Into<Option>` event.
pub async fn append_event(
    event: impl Into<Option<CfdEvent>>,
    conn: &mut PoolConnection<Postgres>,
) -> Result<()> {
    let event = match event.into() {
        Some(event) => event,
        None => return Ok(()),
    };

    let (event_name, event_data) = event.event.to_json();

    tracing::trace!(event = %event_name, order_id = %event.id, "Appending event to database");

    let query_result = sqlx::query(
        r##"
        insert into events (
            cfd_id,
            name,
            data,
            created_at
        ) values (
            (select id from cfds where cfds.uuid = $1),
            $2, $3, $4
        )"##,
    )
    .bind(&event.id)
    .bind(&event_name)
    .bind(&event_data)
    .bind(&event.timestamp)
    .execute(conn)
    .await?;

    if query_result.rows_affected() != 1 {
        anyhow::bail!("failed to insert event");
    }

    Ok(())
}

// TODO: Make sqlx directly instantiate this struct instead of mapping manually. Need to create
// newtype for `settlement_interval`.
#[derive(Clone, Copy)]
pub struct Cfd {
    pub id: OrderId,
    pub position: Position,
    pub initial_price: Price,
    pub taker_leverage: Leverage,
    pub settlement_interval: Duration,
    pub quantity_usd: Usd,
    pub counterparty_network_identity: Identity,
    pub role: Role,
    pub opening_fee: OpeningFee,
    pub initial_funding_rate: FundingRate,
    pub initial_tx_fee_rate: TxFeeRate,
}

pub async fn load_cfd(
    id: OrderId,
    conn: &mut PoolConnection<Postgres>,
) -> Result<(Cfd, Vec<CfdEvent>)> {
    let cfd_row = sqlx::query!(
        r#"
            select
                id as cfd_id,
                uuid as "uuid: model::OrderId",
                position as "position: model::Position",
                initial_price as "initial_price: model::Price",
                leverage as "leverage: model::Leverage",
                settlement_time_interval_hours,
                quantity_usd as "quantity_usd: model::Usd",
                counterparty_network_identity as "counterparty_network_identity: model::Identity",
                role as "role: model::Role",
                opening_fee as "opening_fee: model::OpeningFee",
                initial_funding_rate as "initial_funding_rate: model::FundingRate",
                initial_tx_fee_rate as "initial_tx_fee_rate: model::TxFeeRate"
            from
                cfds
            where
                cfds.uuid = $1
            "#,
        &id.to_string()
    )
    .fetch_one(&mut *conn)
    .await?;

    let cfd = Cfd {
        id: cfd_row.uuid,
        position: cfd_row.position,
        initial_price: cfd_row.initial_price,
        taker_leverage: cfd_row.leverage,
        settlement_interval: Duration::hours(cfd_row.settlement_time_interval_hours.into()),
        quantity_usd: cfd_row.quantity_usd,
        counterparty_network_identity: cfd_row.counterparty_network_identity,
        role: cfd_row.role,
        opening_fee: cfd_row.opening_fee,
        initial_funding_rate: cfd_row.initial_funding_rate,
        initial_tx_fee_rate: cfd_row.initial_tx_fee_rate,
    };

    let events = sqlx::query!(
        r#"

        select
            name,
            data,
            created_at as "created_at: model::Timestamp"
        from
            events
        where
            cfd_id = $1
            "#,
        cfd_row.cfd_id
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|row| {
        Ok(CfdEvent {
            timestamp: row.created_at,
            id,
            event: EventKind::from_json(row.name, row.data)?,
        })
    })
    .collect::<Result<Vec<_>>>()?;

    Ok((cfd, events))
}

pub async fn load_all_cfd_ids(conn: &mut PoolConnection<Postgres>) -> Result<Vec<OrderId>> {
    let ids = sqlx::query!(
        r#"
            select
                id as cfd_id,
                uuid as "uuid: model::OrderId"
            from
                cfds
            order by cfd_id desc
            "#
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| r.uuid)
    .collect();

    Ok(ids)
}

/// Loads all CFDs where we are still able to append events
///
/// This function is to be called when we only want to process CFDs where events can still be
/// appended, but ignore all other CFDs.
/// Open in this context means that the CFD is not final yet, i.e. we can still append events.
/// In this context a CFD is not open anymore if one of the following happened:
/// 1. Event of the confirmation of a payout (spend) transaction on the blockchain was recorded
///     Cases: Collaborative settlement, CET, Refund
/// 2. Event that fails the CFD early was recorded, meaning it becomes irrelevant for processing
///     Cases: Setup failed, Taker's take order rejected
pub async fn load_open_cfd_ids(conn: &mut PoolConnection<Postgres>) -> Result<Vec<OrderId>> {
    let ids = sqlx::query!(
        r#"
            select
                id as cfd_id,
                uuid as "uuid: model::OrderId"
            from
                cfds
            where not exists (
                select id from EVENTS as events
                where cfd_id = cfds.id and
                (
                    events.name = $1 or
                    events.name = $2 or
                    events.name= $3 or
                    events.name= $4 or
                    events.name= $5
                )
            )
            order by cfd_id desc
            "#,
        EventKind::COLLABORATIVE_SETTLEMENT_CONFIRMED,
        EventKind::CET_CONFIRMED,
        EventKind::REFUND_CONFIRMED,
        EventKind::CONTRACT_SETUP_FAILED,
        EventKind::OFFER_REJECTED
    )
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .map(|r| r.uuid)
    .collect();

    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bdk::bitcoin::Amount;
    use model::Cfd;
    use model::Leverage;
    use model::OpeningFee;
    use model::Position;
    use model::Price;
    use model::Role;
    use model::Timestamp;
    use model::TxFeeRate;
    use model::Usd;
    use pretty_assertions::assert_eq;
    use rust_decimal_macros::dec;

    #[tokio::test]
    async fn test_insert_and_load_cfd() {
        let mut conn = setup_test_db().await;

        let cfd = insert(dummy_cfd(), &mut conn).await;
        let (
            super::Cfd {
                id,
                position,
                initial_price,
                taker_leverage: leverage,
                settlement_interval,
                quantity_usd,
                counterparty_network_identity,
                role,
                opening_fee,
                initial_funding_rate,
                initial_tx_fee_rate,
            },
            _,
        ) = load_cfd(cfd.id(), &mut conn).await.unwrap();

        assert_eq!(cfd.id(), id);
        assert_eq!(cfd.position(), position);
        assert_eq!(cfd.initial_price(), initial_price);
        assert_eq!(cfd.taker_leverage(), leverage);
        assert_eq!(cfd.settlement_time_interval_hours(), settlement_interval);
        assert_eq!(cfd.quantity(), quantity_usd);
        assert_eq!(
            cfd.counterparty_network_identity(),
            counterparty_network_identity
        );
        assert_eq!(cfd.role(), role);
        assert_eq!(cfd.opening_fee(), opening_fee);
        assert_eq!(cfd.initial_funding_rate(), initial_funding_rate);
        assert_eq!(cfd.initial_tx_fee_rate(), initial_tx_fee_rate);
    }

    #[tokio::test]
    async fn test_insert_and_load_cfd_ids_order_desc() {
        let mut conn = setup_test_db().await;

        let cfd_1 = insert(dummy_cfd(), &mut conn).await;
        let cfd_2 = insert(dummy_cfd(), &mut conn).await;
        let cfd_3 = insert(dummy_cfd(), &mut conn).await;

        let ids = load_all_cfd_ids(&mut conn).await.unwrap();

        assert_eq!(vec![cfd_3.id(), cfd_2.id(), cfd_1.id()], ids)
    }

    #[tokio::test]
    async fn test_append_events() {
        let mut conn = setup_test_db().await;

        let cfd = insert(dummy_cfd(), &mut conn).await;

        let timestamp = Timestamp::now();

        let event1 = CfdEvent {
            timestamp,
            id: cfd.id(),
            event: EventKind::OfferRejected,
        };

        append_event(event1.clone(), &mut conn).await.unwrap();
        let (_, events) = load_cfd(cfd.id(), &mut conn).await.unwrap();
        assert_eq!(events, vec![event1.clone()]);

        let event2 = CfdEvent {
            timestamp,
            id: cfd.id(),
            event: EventKind::RevokeConfirmed,
        };

        append_event(event2.clone(), &mut conn).await.unwrap();
        let (_, events) = load_cfd(cfd.id(), &mut conn).await.unwrap();
        assert_eq!(events, vec![event1, event2])
    }

    #[tokio::test]
    async fn given_collaborative_close_confirmed_then_do_not_load_non_final_cfd() {
        let mut conn = setup_test_db().await;

        let cfd_final = insert(dummy_cfd(), &mut conn).await;
        append_event(lock_confirmed(&cfd_final), &mut conn)
            .await
            .unwrap();
        append_event(collab_settlement_confirmed(&cfd_final), &mut conn)
            .await
            .unwrap();

        let cfd_ids = load_open_cfd_ids(&mut conn).await.unwrap();

        assert!(cfd_ids.is_empty());
    }

    #[tokio::test]
    async fn given_cet_confirmed_then_do_not_load_non_final_cfd() {
        let mut conn = setup_test_db().await;

        let cfd_final = insert(dummy_cfd(), &mut conn).await;
        append_event(lock_confirmed(&cfd_final), &mut conn)
            .await
            .unwrap();
        append_event(cet_confirmed(&cfd_final), &mut conn)
            .await
            .unwrap();

        let cfd_ids = load_open_cfd_ids(&mut conn).await.unwrap();
        assert!(cfd_ids.is_empty());
    }

    #[tokio::test]
    async fn given_refund_confirmed_then_do_not_load_non_final_cfd() {
        let mut conn = setup_test_db().await;

        let cfd_final = insert(dummy_cfd(), &mut conn).await;
        append_event(lock_confirmed(&cfd_final), &mut conn)
            .await
            .unwrap();
        append_event(refund_confirmed(&cfd_final), &mut conn)
            .await
            .unwrap();

        let cfd_ids = load_open_cfd_ids(&mut conn).await.unwrap();
        assert!(cfd_ids.is_empty());
    }

    #[tokio::test]
    async fn given_setup_failed_then_do_not_load_non_final_cfd() {
        let mut conn = setup_test_db().await;

        let cfd_final = insert(dummy_cfd(), &mut conn).await;
        append_event(lock_confirmed(&cfd_final), &mut conn)
            .await
            .unwrap();
        append_event(setup_failed(&cfd_final), &mut conn)
            .await
            .unwrap();

        let cfd_ids = load_open_cfd_ids(&mut conn).await.unwrap();
        assert!(cfd_ids.is_empty());
    }

    #[tokio::test]
    async fn given_order_rejected_then_do_not_load_non_final_cfd() {
        let mut conn = setup_test_db().await;

        let cfd_final = insert(dummy_cfd(), &mut conn).await;
        append_event(lock_confirmed(&cfd_final), &mut conn)
            .await
            .unwrap();
        append_event(order_rejected(&cfd_final), &mut conn)
            .await
            .unwrap();

        let cfd_ids = load_open_cfd_ids(&mut conn).await.unwrap();
        assert!(cfd_ids.is_empty());
    }

    #[tokio::test]
    async fn given_final_and_non_final_cfd_then_non_final_one_still_loaded() {
        let mut conn = setup_test_db().await;

        let cfd_not_final = insert(dummy_cfd(), &mut conn).await;
        append_event(lock_confirmed(&cfd_not_final), &mut conn)
            .await
            .unwrap();

        let cfd_final = insert(dummy_cfd(), &mut conn).await;
        append_event(lock_confirmed(&cfd_final), &mut conn)
            .await
            .unwrap();
        append_event(order_rejected(&cfd_final), &mut conn)
            .await
            .unwrap();

        let cfd_ids = load_open_cfd_ids(&mut conn).await.unwrap();

        assert_eq!(cfd_ids.len(), 1);
        assert_eq!(*cfd_ids.first().unwrap(), cfd_not_final.id())
    }

    async fn setup_test_db() -> PoolConnection<Postgres> {
        let pool = PgPool::connect(":memory:").await.unwrap();

        run_migrations(&pool).await.unwrap();

        pool.acquire().await.unwrap()
    }

    fn dummy_cfd() -> Cfd {
        Cfd::new(
            OrderId::default(),
            Position::Long,
            Price::new(dec!(60_000)).unwrap(),
            Leverage::TWO,
            Duration::hours(24),
            Role::Taker,
            Usd::new(dec!(1_000)),
            "FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF"
                .parse()
                .unwrap(),
            OpeningFee::new(Amount::from_sat(2000)),
            FundingRate::default(),
            TxFeeRate::default(),
        )
    }

    /// Insert this [`Cfd`] into the database, returning the instance
    /// for further chaining.
    pub async fn insert(cfd: Cfd, conn: &mut PoolConnection<Postgres>) -> Cfd {
        insert_cfd(&cfd, conn).await.unwrap();
        cfd
    }

    fn lock_confirmed(cfd: &Cfd) -> CfdEvent {
        CfdEvent {
            timestamp: Timestamp::now(),
            id: cfd.id(),
            event: EventKind::LockConfirmed,
        }
    }

    fn collab_settlement_confirmed(cfd: &Cfd) -> CfdEvent {
        CfdEvent {
            timestamp: Timestamp::now(),
            id: cfd.id(),
            event: EventKind::CollaborativeSettlementConfirmed,
        }
    }

    fn cet_confirmed(cfd: &Cfd) -> CfdEvent {
        CfdEvent {
            timestamp: Timestamp::now(),
            id: cfd.id(),
            event: EventKind::CetConfirmed,
        }
    }

    fn refund_confirmed(cfd: &Cfd) -> CfdEvent {
        CfdEvent {
            timestamp: Timestamp::now(),
            id: cfd.id(),
            event: EventKind::RefundConfirmed,
        }
    }

    fn setup_failed(cfd: &Cfd) -> CfdEvent {
        CfdEvent {
            timestamp: Timestamp::now(),
            id: cfd.id(),
            event: EventKind::ContractSetupFailed,
        }
    }

    fn order_rejected(cfd: &Cfd) -> CfdEvent {
        CfdEvent {
            timestamp: Timestamp::now(),
            id: cfd.id(),
            event: EventKind::OfferRejected,
        }
    }
}
