use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use bdk::bitcoin::Amount;
use bdk::bitcoin::OutPoint;
use bdk::bitcoin::Script;
use bdk::miniscript::DescriptorTrait;
use chashmap_async::CHashMap;
use futures::future::BoxFuture;
use futures::FutureExt;
use futures::Stream;
use futures::StreamExt;
use maia::TransactionExt;
use model::CfdEvent;
use model::Contracts;
use model::Dlc;
use model::EventKind;
use model::FeeAccount;
use model::Fees;
use model::FundingRate;
use model::Identity;
use model::Leverage;
use model::OpeningFee;
use model::OrderId;
use model::Payout;
use model::Position;
use model::Price;
use model::Role;
use model::Timestamp;
use model::TxFeeRate;
use model::Txid;
use model::Usd;
use model::Vout;
use sqlx::migrate::MigrateError;
use sqlx::pool::PoolConnection;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::Connection as _;
use sqlx::Sqlite;
use sqlx::SqlitePool;
use sqlx::Transaction;
use std::any::Any;
use std::any::TypeId;
use std::path::PathBuf;
use std::sync::Arc;
use time::Duration;
use time::OffsetDateTime;

#[derive(Clone)]
pub struct Connection {
    inner: SqlitePool,
    aggregate_cache: Arc<CHashMap<(TypeId, OrderId), Box<dyn Any + Send + Sync + 'static>>>,
}

impl Connection {
    fn new(pool: SqlitePool) -> Self {
        Self {
            inner: pool,
            aggregate_cache: Arc::new(CHashMap::new()),
        }
    }

    pub async fn close(self) {
        self.inner.close().await;
    }
}

/// Connects to the SQLite database at the given path.
///
/// If the database does not exist, it will be created. If it does exist, we load it and apply all
/// pending migrations. If applying migrations fails, the old database is backed up next to it and a
/// new one is created.
pub fn connect(path: PathBuf) -> BoxFuture<'static, Result<Connection>> {
    async move {
        let pool = SqlitePool::connect_with(
            SqliteConnectOptions::new()
                .create_if_missing(true)
                .filename(&path),
        )
        .await?;

        let path_display = path.display();

        // Attempt to migrate, early return if successful
        let error = match run_migrations(&pool).await {
            Ok(()) => {
                tracing::info!("Opened database at {path_display}");

                return Ok(Connection::new(pool));
            }
            Err(e) => e,
        };

        // Attempt to recover from _some_ problems during migration.
        // These two can happen if someone tampered with the migrations or messed with the DB.
        if let Some(MigrateError::VersionMissing(_) | MigrateError::VersionMismatch(_)) =
            error.downcast_ref::<MigrateError>()
        {
            tracing::error!("{:#}", error);

            let unix_timestamp = time::OffsetDateTime::now_utc().unix_timestamp();

            let new_path = PathBuf::from(format!("{path_display}-{unix_timestamp}-backup"));
            let new_path_display = new_path.display();

            tracing::info!("Backing up old database at {path_display} to {new_path_display}");

            tokio::fs::rename(&path, &new_path)
                .await
                .context("Failed to rename database file")?;

            tracing::info!("Starting with a new database!");

            // recurse to reconnect (async recursion requires a `BoxFuture`)
            return connect(path).await;
        }

        Err(error)
    }
    .boxed()
}

pub async fn memory() -> Result<Connection> {
    // Note: Every :memory: database is distinct from every other. So, opening two database
    // connections each with the filename ":memory:" will create two independent in-memory
    // databases. see: https://www.sqlite.org/inmemorydb.html
    let pool = SqlitePool::connect(":memory:").await?;

    run_migrations(&pool).await?;

    Ok(Connection::new(pool))
}

async fn run_migrations(pool: &SqlitePool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("Failed to run migrations")?;

    Ok(())
}

impl Connection {
    pub async fn insert_cfd(&self, cfd: &model::Cfd) -> Result<()> {
        let mut conn = self.inner.acquire().await?;

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
        .execute(&mut conn)
        .await?;

        if query_result.rows_affected() != 1 {
            anyhow::bail!("failed to insert cfd");
        }

        Ok(())
    }

    /// Appends an event to the `events` table.
    ///
    /// To make handling of `None` events more ergonomic, you can pass anything in here that
    /// implements `Into<Option>` event.
    pub async fn append_event(&self, event: impl Into<Option<CfdEvent>>) -> Result<()> {
        let mut conn = self.inner.acquire().await?;

        let event = match event.into() {
            Some(event) => event,
            None => return Ok(()),
        };

        let (event_name, event_data) = event.event.to_json();

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
        .execute(&mut conn)
        .await?;

        if query_result.rows_affected() != 1 {
            anyhow::bail!("failed to insert event");
        }

        tracing::info!(event = %event_name, order_id = %event.id, "Appended event to database");

        Ok(())
    }

    /// Load a CFD in its latest version from the database.
    pub async fn load_open_cfd<C>(&self, id: OrderId, args: C::CtorArgs) -> Result<C, Error>
    where
        C: CfdAggregate,
    {
        let mut conn = self.inner.acquire().await?;
        let mut db_tx = conn.begin().await?;

        let cache_key = (TypeId::of::<C>(), id);
        let aggregate = std::any::type_name::<C>();

        let cfd = match self.aggregate_cache.remove(&cache_key).await {
            None => {
                // No cache entry? Load the CFD row. Version will be 0 because we haven't applied
                // any events, thus all events will be loaded.
                let cfd = load_cfd_row(&mut db_tx, id).await?;

                C::new(args, cfd)
            }
            Some(cfd) => {
                // Got a cache entry: Downcast it to the type at hand.

                *cfd.downcast::<C>()
                    .expect("we index by type id, must be able to downcast")
            }
        };
        let cfd_version = cfd.version();

        let events = load_cfd_events(&mut db_tx, id, cfd_version).await?;
        let num_events = events.len();

        tracing::debug!(order_id = %id, %aggregate, %cfd_version, %num_events, "Applying new events to CFD");

        let cfd = events.into_iter().fold(cfd, C::apply);

        self.aggregate_cache
            .insert(cache_key, Box::new(cfd.clone()))
            .await;

        db_tx.commit().await?;

        Ok(cfd)
    }

    pub fn load_all_cfds<C>(&self, args: C::CtorArgs) -> impl Stream<Item = Result<C>> + Unpin + '_
    where
        C: ClosedCfdAggregate + Unpin,
        C::CtorArgs: Clone + Send + Sync,
    {
        let stream = async_stream::try_stream! {
            let mut conn = self.inner.acquire().await?;

            let ids = sqlx::query!(
                r#"
                SELECT
                    uuid as "uuid: model::OrderId"
                FROM
                    cfds
                "#
            )
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|r| r.uuid);

            drop(conn);

            for id in ids {
                let open_cfd = self
                    .load_open_cfd(id, args.clone())
                    .await
                    .with_context(|| format!("Failed to load open CFD {id}"))?;

                yield open_cfd;
            }

            let mut conn = self.inner.acquire().await?;

            let ids = sqlx::query!(
                r#"
                SELECT
                    uuid as "uuid: model::OrderId"
                FROM
                    closed_cfds
                "#
            )
            .fetch_all(&mut *conn)
            .await?
            .into_iter()
            .map(|r| r.uuid);

            drop(conn);

            for id in ids {
                let closed_cfd = self
                    .load_closed_cfd(id, args.clone())
                    .await
                    .with_context(|| format!("Failed to load closed CFD {id}"))?;

                yield closed_cfd;
            }
        };

        stream.boxed()
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
    pub fn load_all_open_cfds<C>(
        &self,
        args: C::CtorArgs,
    ) -> impl Stream<Item = Result<C>> + Unpin + '_
    where
        C: CfdAggregate + Unpin,
        C::CtorArgs: Clone + Send + Sync,
    {
        let stream = async_stream::try_stream! {
            let ids = self.load_open_cfd_ids().await?;

            for id in ids {
                let cfd = match self.load_open_cfd(id, args.clone()).await {
                    Ok(cfd) => Ok(cfd),
                    Err(Error::OpenCfdNotFound) => {
                        tracing::trace!(order_id=%id, target="db", "Ignoring open CFD not found because it was likely moved to the closed table");
                        continue;
                    },
                    Err(e) => {
                        Err(e)
                    }
                }?;

                yield cfd;
            }
        };

        stream.boxed()
    }

    pub async fn load_open_cfd_ids(&self) -> Result<Vec<OrderId>> {
        let mut conn = self.inner.acquire().await?;

        let ids = sqlx::query!(
            r#"
            select
                id as cfd_id,
                uuid as "uuid: model::OrderId"
            from
                cfds
            where not exists (
                select id from EVENTS as events
                where events.cfd_id = cfds.id and
                (
                    events.name = $1 or
                    events.name = $2
                )
            )
            "#,
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

    pub async fn move_to_closed_cfds(&self) -> Result<()> {
        let ids = self.closed_cfd_ids_according_to_the_blockchain().await?;

        if !ids.is_empty() {
            tracing::debug!("Moving CFDs to closed_cfds table: {ids:?}");
        }

        for id in ids.into_iter() {
            let pool = self.inner.clone();
            let fut = async move {
                let mut conn = pool.acquire().await?;
                let mut db_tx = conn.begin().await?;

                let cfd = load_cfd_row(&mut db_tx, id).await?;
                let events = load_cfd_events(&mut db_tx, id, 0).await?;
                let event_log = EventLog::new(&events);

                let closed_cfd = ClosedCfdInputAggregate::new(cfd);
                let closed_cfd = events
                    .into_iter()
                    .try_fold(closed_cfd, ClosedCfdInputAggregate::apply)?
                    .build()?;

                insert_closed_cfd(&mut db_tx, closed_cfd).await?;
                insert_event_log(&mut db_tx, id, event_log).await?;

                match closed_cfd {
                    ClosedCfdInput {
                        collaborative_settlement: Some(collaborative_settlement),
                        commit: None,
                        non_collaborative_settlement: None,
                        refund: None,
                        ..
                    } => {
                        insert_collaborative_settlement_tx(
                            &mut db_tx,
                            id,
                            collaborative_settlement,
                        )
                        .await?;
                    }
                    ClosedCfdInput {
                        collaborative_settlement: None,
                        commit: Some(commit),
                        non_collaborative_settlement: Some(cet),
                        refund: None,
                        ..
                    } => {
                        insert_commit_tx(&mut db_tx, id, commit).await?;
                        insert_cet(&mut db_tx, id, cet).await?;
                    }
                    ClosedCfdInput {
                        collaborative_settlement: None,
                        commit: Some(commit),
                        non_collaborative_settlement: None,
                        refund: Some(refund),
                        ..
                    } => {
                        insert_commit_tx(&mut db_tx, id, commit).await?;
                        insert_refund_tx(&mut db_tx, id, refund).await?;
                    }
                    ClosedCfdInput {
                        collaborative_settlement,
                        commit,
                        non_collaborative_settlement,
                        refund,
                        ..
                    } => bail!(
                        "CFD to be closed has insane combination of transactions:
                          {collaborative_settlement:?},
                          {commit:?},
                          {non_collaborative_settlement:?},
                          {refund:?}"
                    ),
                }

                delete_from_events_table(&mut db_tx, id).await?;
                delete_from_cfds_table(&mut db_tx, id).await?;

                db_tx.commit().await?;

                anyhow::Ok(())
            };

            if let Err(e) = fut.await {
                tracing::warn!(order_id = %id, "Failed to move closed CFD: {e:#}");
            }
        }

        Ok(())
    }

    /// Load a closed CFD from the database.
    async fn load_closed_cfd<C>(&self, id: OrderId, args: C::CtorArgs) -> Result<C>
    where
        C: ClosedCfdAggregate,
    {
        let mut conn = self.inner.acquire().await?;

        let cfd = sqlx::query!(
            r#"
            SELECT
                uuid as "uuid: model::OrderId",
                position as "position: model::Position",
                initial_price as "initial_price: model::Price",
                taker_leverage as "taker_leverage: model::Leverage",
                n_contracts as "n_contracts: model::Contracts",
                counterparty_network_identity as "counterparty_network_identity: model::Identity",
                role as "role: model::Role",
                fees as "fees: model::Fees",
                expiry_timestamp,
                lock_txid as "lock_txid: model::Txid",
                lock_dlc_vout as "lock_dlc_vout: model::Vout"
            FROM
                closed_cfds
            WHERE
                closed_cfds.uuid = $1
            "#,
            id
        )
        .fetch_one(&mut conn)
        .await?;

        let expiry_timestamp = OffsetDateTime::from_unix_timestamp(cfd.expiry_timestamp)?;

        let collaborative_settlement = load_collaborative_settlement_tx(&mut conn, id).await?;

        let commit = load_commit_tx(&mut conn, id).await?;

        let non_collaborative_settlement = load_cet(&mut conn, id).await?;

        let refund = load_refund_tx(&mut conn, id).await?;

        let settlement = match (
            collaborative_settlement,
            commit,
            non_collaborative_settlement,
            refund,
        ) {
            (
                Some(CollaborativeSettlement {
                    txid,
                    vout,
                    payout,
                    price,
                    ..
                }),
                None,
                None,
                None,
            ) => Settlement::Collaborative {
                txid,
                vout,
                payout,
                price,
            },
            (
                None,
                Some(commit_txid),
                Some(Cet {
                    txid,
                    vout,
                    payout,
                    price,
                    ..
                }),
                None,
            ) => Settlement::Cet {
                commit_txid,
                txid,
                vout,
                payout,
                price,
            },
            (
                None,
                Some(commit_txid),
                None,
                Some(Refund {
                    txid, vout, payout, ..
                }),
            ) => Settlement::Refund {
                commit_txid,
                txid,
                vout,
                payout,
            },
            _ => {
                bail!(
                    "Closed CFD has insane combination of transactions:
                       {collaborative_settlement:?},
                       {commit:?},
                       {non_collaborative_settlement:?},
                       {refund:?}"
                )
            }
        };

        let cfd = ClosedCfd {
            id,
            position: cfd.position,
            initial_price: cfd.initial_price,
            taker_leverage: cfd.taker_leverage,
            n_contracts: cfd.n_contracts,
            counterparty_network_identity: cfd.counterparty_network_identity,
            role: cfd.role,
            fees: cfd.fees,
            expiry_timestamp,
            lock: Lock {
                txid: cfd.lock_txid,
                dlc_vout: cfd.lock_dlc_vout,
            },
            settlement,
        };

        Ok(C::new_closed(args, cfd))
    }

    async fn closed_cfd_ids_according_to_the_blockchain(&self) -> Result<Vec<OrderId>> {
        let mut conn = self.inner.acquire().await?;

        let ids = sqlx::query!(
            r#"
            select
                id as cfd_id,
                uuid as "uuid: model::OrderId"
            from
                cfds
            where exists (
                select id from EVENTS as events
                where events.cfd_id = cfds.id and
                (
                    events.name = $1 or
                    events.name = $2 or
                    events.name= $3
                )
            )
            "#,
            EventKind::COLLABORATIVE_SETTLEMENT_CONFIRMED,
            EventKind::CET_CONFIRMED,
            EventKind::REFUND_CONFIRMED,
        )
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|r| r.uuid)
        .collect();

        Ok(ids)
    }
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

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("The CFD requested was not found in the open CFDs")]
    OpenCfdNotFound,
    #[error("{0:#}")]
    Sqlx(#[source] sqlx::Error),
    #[error("{0:#}")]
    Other(#[source] anyhow::Error),
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Error::Sqlx(e)
    }
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Other(e)
    }
}

/// Data loaded from the database about a closed CFD.
#[derive(Debug, Clone, Copy)]
pub struct ClosedCfd {
    pub id: OrderId,
    pub position: Position,
    pub initial_price: Price,
    pub taker_leverage: Leverage,
    pub n_contracts: Contracts,
    pub counterparty_network_identity: Identity,
    pub role: Role,
    pub fees: Fees,
    pub expiry_timestamp: OffsetDateTime,
    pub lock: Lock,
    pub settlement: Settlement,
}

/// Data loaded from the database about the lock transaction of a
/// closed CFD.
#[derive(Debug, Clone, Copy)]
pub struct Lock {
    pub txid: Txid,
    pub dlc_vout: Vout,
}

/// Data loaded from the database about the way in which a closed CFD
/// was settled.
///
/// It is represented using an `enum` rather than a series of optional
/// fields so that only sane combinations of transactions can be
/// loaded from the database.
#[derive(Debug, Clone, Copy)]
pub enum Settlement {
    Collaborative {
        txid: Txid,
        vout: Vout,
        payout: Payout,
        price: Price,
    },
    Cet {
        commit_txid: Txid,
        txid: Txid,
        vout: Vout,
        payout: Payout,
        price: Price,
    },
    Refund {
        commit_txid: Txid,
        txid: Txid,
        vout: Vout,
        payout: Payout,
    },
}

/// All the data related to a closed CFD that we want to store in the
/// database.
#[derive(Debug, Clone, Copy)]
struct ClosedCfdInput {
    id: OrderId,
    position: Position,
    initial_price: Price,
    taker_leverage: Leverage,
    n_contracts: Contracts,
    counterparty_network_identity: Identity,
    role: Role,
    fees: Fees,
    expiry_timestamp: OffsetDateTime,
    lock: LockInput,
    collaborative_settlement: Option<CollaborativeSettlement>,
    commit: Option<Commit>,
    non_collaborative_settlement: Option<Cet>,
    refund: Option<Refund>,
}

/// Auxiliary type used to gradually combine a `Cfd` with its list of
/// `CfdEvent`s.
///
/// Once all the `CfdEvent`s have been applied, we can build a
/// `ClosedCfdInput` which is used for database insertion.
#[derive(Debug, Clone)]
struct ClosedCfdInputAggregate {
    id: OrderId,
    position: Position,
    initial_price: Price,
    taker_leverage: Leverage,
    n_contracts: Contracts,
    counterparty_network_identity: Identity,
    role: Role,
    fee_account: FeeAccount,
    own_script_pubkey: Option<Script>,
    expiry_timestamp: Option<OffsetDateTime>,
    lock: Option<LockInput>,
    commit: Option<Commit>,
    collaborative_settlement: Option<CollaborativeSettlement>,
    cet: Option<Cet>,
    refund: Option<Refund>,
    latest_dlc: Option<Dlc>,
}

#[derive(Debug, Clone, Copy)]
pub struct LockInput {
    txid: Txid,
    dlc_vout: Vout,
    timestamp: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct CollaborativeSettlement {
    txid: Txid,
    vout: Vout,
    payout: Payout,
    price: Price,
    timestamp: Timestamp,
}

#[derive(Debug, Clone, Copy)]
struct Commit {
    txid: Txid,
    timestamp: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Cet {
    txid: Txid,
    vout: Vout,
    payout: Payout,
    price: Price,
    timestamp: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Refund {
    txid: Txid,
    vout: Vout,
    payout: Payout,
    timestamp: Timestamp,
}

impl ClosedCfdInputAggregate {
    fn new(cfd: Cfd) -> Self {
        let Cfd {
            id,
            position,
            initial_price,
            taker_leverage,
            settlement_interval: _,
            quantity_usd,
            counterparty_network_identity,
            role,
            opening_fee,
            ..
        } = cfd;
        let n_contracts = quantity_usd
            .try_into_u64()
            .expect("number of contracts to fit into a u64");
        let n_contracts = Contracts::new(n_contracts);

        Self {
            id,
            position,
            initial_price,
            taker_leverage,
            n_contracts,
            counterparty_network_identity,
            role,
            fee_account: FeeAccount::new(position, role).add_opening_fee(opening_fee),
            own_script_pubkey: None,
            expiry_timestamp: None,
            lock: None,
            commit: None,
            collaborative_settlement: None,
            cet: None,
            refund: None,
            latest_dlc: None,
        }
    }

    fn apply(mut self, event: CfdEvent) -> Result<Self> {
        use model::EventKind::*;
        match event.event {
            ContractSetupStarted => {}
            ContractSetupCompleted { dlc } => {
                let script_pubkey = dlc.lock.1.script_pubkey();
                let OutPoint { txid, vout } = dlc
                    .lock
                    .0
                    .outpoint(&script_pubkey)
                    .context("Missing DLC in lock TX")?;

                let txid = Txid::new(txid);
                let dlc_vout = Vout::new(vout);

                self.lock = Some(LockInput {
                    txid,
                    dlc_vout,
                    timestamp: event.timestamp,
                });

                self.own_script_pubkey = Some(dlc.script_pubkey_for(self.role));

                self.expiry_timestamp = Some(dlc.settlement_event_id.timestamp());
                self.latest_dlc = Some(dlc);
            }
            ContractSetupFailed => {}
            OfferRejected => {}
            RolloverStarted => {}
            RolloverAccepted => {}
            RolloverRejected => {}
            RolloverCompleted { dlc, funding_fee } => {
                self.own_script_pubkey = Some(dlc.script_pubkey_for(self.role));

                self.fee_account = self.fee_account.add_funding_fee(funding_fee);

                self.expiry_timestamp = Some(dlc.settlement_event_id.timestamp());
                self.latest_dlc = Some(dlc);
            }
            RolloverFailed => {}
            CollaborativeSettlementStarted { .. } => {}
            CollaborativeSettlementProposalAccepted => {}
            CollaborativeSettlementCompleted {
                spend_tx,
                script,
                price,
            } => {
                let OutPoint { txid, vout } = spend_tx
                    .outpoint(&script)
                    .context("Missing spend script in collaborative settlement TX")?;

                let payout = &spend_tx
                    .output
                    .get(vout as usize)
                    .with_context(|| format!("No output at vout {vout}"))?;
                let payout = Payout::new(Amount::from_sat(payout.value));

                let txid = Txid::new(txid);
                let vout = Vout::new(vout);

                self.collaborative_settlement = Some(CollaborativeSettlement {
                    txid,
                    vout,
                    payout,
                    price,
                    timestamp: event.timestamp,
                })
            }
            CollaborativeSettlementRejected => {}
            CollaborativeSettlementFailed => {}
            LockConfirmed => {}
            LockConfirmedAfterFinality => {}
            CommitConfirmed => {
                self.commit = match self.latest_dlc {
                    None => {
                        bail!("No DLC after commit confirmed");
                    }
                    Some(ref dlc) => {
                        let script_pubkey = dlc.commit.2.script_pubkey();
                        let OutPoint { txid, .. } = dlc
                            .commit
                            .0
                            .outpoint(&script_pubkey)
                            .context("Missing DLC in commit TX")?;

                        Some(Commit {
                            txid: Txid::new(txid),
                            timestamp: event.timestamp,
                        })
                    }
                };
            }
            CetConfirmed => {}
            RefundConfirmed => {}
            RevokeConfirmed => {}
            CollaborativeSettlementConfirmed => {}
            CetTimelockExpiredPriorOracleAttestation => {}
            CetTimelockExpiredPostOracleAttestation { cet: _ } => {
                // if we have an attestation we have already updated
                // the `self.non_collaborative_settlement` field in
                // the `OracleAttestedPriorCetTimelock` branch.
                //
                // We could repeat that work here just in case, but we
                // don't have the closing price, so the
                // `NonCollaborativeSettlement` struct would be
                // incomplete
            }
            RefundTimelockExpired { refund_tx } => {
                let own_script_pubkey = self
                    .own_script_pubkey
                    .as_ref()
                    .context("Missing DLC after refund timelock has expired")?;
                let OutPoint { txid, vout } = refund_tx
                    .outpoint(own_script_pubkey)
                    .context("Missing spend script in refund TX")?;

                let payout = &refund_tx
                    .output
                    .get(vout as usize)
                    .with_context(|| format!("No output at vout {vout}"))?;
                let payout = Payout::new(Amount::from_sat(payout.value));

                let txid = Txid::new(txid);
                let vout = Vout::new(vout);

                self.refund = Some(Refund {
                    txid,
                    vout,
                    payout,
                    timestamp: event.timestamp,
                })
            }
            OracleAttestedPriorCetTimelock {
                timelocked_cet,
                commit_tx,
                price,
            } => {
                if self.commit.is_none() {
                    self.commit = commit_tx.map(|tx| Commit {
                        txid: Txid::new(tx.txid()),
                        timestamp: event.timestamp,
                    });
                }

                let own_script_pubkey = self
                    .own_script_pubkey
                    .as_ref()
                    .context("Missing DLC after CET was chosen")?;
                let OutPoint { txid, vout } = timelocked_cet
                    .outpoint(own_script_pubkey)
                    .context("Missing spend script in CET")?;

                let payout = &timelocked_cet
                    .output
                    .get(vout as usize)
                    .with_context(|| format!("No output at vout {vout}"))?;
                let payout = Payout::new(Amount::from_sat(payout.value));

                let txid = Txid::new(txid);
                let vout = Vout::new(vout);

                self.cet = Some(Cet {
                    txid,
                    vout,
                    payout,
                    price,
                    timestamp: event.timestamp,
                })
            }
            OracleAttestedPostCetTimelock { cet, price } => {
                let own_script_pubkey = self
                    .own_script_pubkey
                    .as_ref()
                    .context("Missing DLC after CET was chosen")?;
                let OutPoint { txid, vout } = cet
                    .outpoint(own_script_pubkey)
                    .context("Missing spend script in CET")?;

                let payout = &cet
                    .output
                    .get(vout as usize)
                    .with_context(|| format!("No output at vout {vout}"))?;
                let payout = Payout::new(Amount::from_sat(payout.value));

                let txid = Txid::new(txid);
                let vout = Vout::new(vout);

                self.cet = Some(Cet {
                    txid,
                    vout,
                    payout,
                    price,
                    timestamp: event.timestamp,
                })
            }
            ManualCommit { tx } => {
                self.commit = Some(Commit {
                    txid: Txid::new(tx.txid()),
                    timestamp: event.timestamp,
                });
            }
        }

        Ok(self)
    }

    fn build(self) -> Result<ClosedCfdInput> {
        let Self {
            id,
            position,
            initial_price,
            taker_leverage,
            n_contracts,
            counterparty_network_identity,
            role,
            fee_account,
            expiry_timestamp,
            lock,
            commit,
            collaborative_settlement,
            cet: non_collaborative_settlement,
            refund,
            ..
        } = self;

        Ok(ClosedCfdInput {
            id,
            position,
            initial_price,
            taker_leverage,
            n_contracts,
            counterparty_network_identity,
            role,
            fees: Fees::new(fee_account.balance()),
            expiry_timestamp: expiry_timestamp.context("missing expiry timestamp")?,
            lock: lock.context("missing lock")?,
            collaborative_settlement,
            commit,
            non_collaborative_settlement,
            refund,
        })
    }
}

struct EventLog(Vec<EventLogEntry>);

impl EventLog {
    fn new(events: &[CfdEvent]) -> Self {
        Self(events.iter().map(EventLogEntry::from).collect())
    }
}

struct EventLogEntry {
    name: String,
    created_at: i64,
}

impl From<&CfdEvent> for EventLogEntry {
    fn from(event: &CfdEvent) -> Self {
        let name = event.event.to_string();
        let created_at = event.timestamp.seconds();

        Self { name, created_at }
    }
}

/// A trait for abstracting over an aggregate.
///
/// Aggregating all available events differs based on the module. Thus, to provide a single
/// interface we ask the caller to provide us with the bare minimum API so we can build (and
/// therefore cache) the aggregate for them.
pub trait CfdAggregate: Clone + Send + Sync + 'static {
    type CtorArgs;

    fn new(args: Self::CtorArgs, cfd: Cfd) -> Self;
    fn apply(self, event: CfdEvent) -> Self;
    fn version(&self) -> u32;
}

/// A trait for building an aggregate based on a `ClosedCfd`.
pub trait ClosedCfdAggregate: CfdAggregate {
    fn new_closed(args: Self::CtorArgs, cfd: ClosedCfd) -> Self;
}

async fn load_cfd_row(conn: &mut Transaction<'_, Sqlite>, id: OrderId) -> Result<Cfd, Error> {
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
        id
    )
    .fetch_optional(&mut *conn)
    .await?
    .ok_or(Error::OpenCfdNotFound)?;

    Ok(Cfd {
        id: cfd_row.uuid,
        position: cfd_row.position,
        initial_price: cfd_row.initial_price,
        taker_leverage: cfd_row.leverage,
        settlement_interval: Duration::hours(cfd_row.settlement_time_interval_hours),
        quantity_usd: cfd_row.quantity_usd,
        counterparty_network_identity: cfd_row.counterparty_network_identity,
        role: cfd_row.role,
        opening_fee: cfd_row.opening_fee,
        initial_funding_rate: cfd_row.initial_funding_rate,
        initial_tx_fee_rate: cfd_row.initial_tx_fee_rate,
    })
}

/// Load events for a given CFD but only onwards from the specified version.
///
/// The version of a CFD is the number of events that have been applied. If we have an aggregate
/// instance in version 3, we can avoid loading the first 3 events and only apply the ones after.
async fn load_cfd_events(
    conn: &mut Transaction<'_, Sqlite>,
    id: OrderId,
    from_version: u32,
) -> Result<Vec<CfdEvent>> {
    let events = sqlx::query!(
        r#"

        select
            name,
            data,
            created_at as "created_at: model::Timestamp"
        from
            events
        join
            cfds c on c.id = events.cfd_id
        where
            uuid = $1
        limit $2,-1
            "#,
        id,
        from_version
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

    Ok(events)
}

async fn insert_closed_cfd(conn: &mut Transaction<'_, Sqlite>, cfd: ClosedCfdInput) -> Result<()> {
    let expiry_timestamp = cfd.expiry_timestamp.unix_timestamp();

    let query_result = sqlx::query!(
        r#"
        INSERT INTO closed_cfds
        (
            uuid,
            position,
            initial_price,
            taker_leverage,
            n_contracts,
            counterparty_network_identity,
            role,
            fees,
            expiry_timestamp,
            lock_txid,
            lock_dlc_vout,
            lock_timestamp
        )
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
        "#,
        cfd.id,
        cfd.position,
        cfd.initial_price,
        cfd.taker_leverage,
        cfd.n_contracts,
        cfd.counterparty_network_identity,
        cfd.role,
        cfd.fees,
        expiry_timestamp,
        cfd.lock.txid,
        cfd.lock.dlc_vout,
        cfd.lock.timestamp
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() != 1 {
        anyhow::bail!("failed to insert into closed_cfds");
    }

    Ok(())
}

async fn insert_collaborative_settlement_tx(
    conn: &mut Transaction<'_, Sqlite>,
    id: OrderId,
    CollaborativeSettlement {
        txid,
        vout,
        payout,
        price,
        timestamp,
    }: CollaborativeSettlement,
) -> Result<()> {
    let query_result = sqlx::query!(
        r#"
        INSERT INTO collaborative_settlement_txs
        (
            cfd_id,
            txid,
            vout,
            payout,
            price,
            timestamp
        )
        VALUES
        (
            (SELECT id FROM closed_cfds WHERE closed_cfds.uuid = $1),
            $2, $3, $4, $5, $6
        )
        "#,
        id,
        txid,
        vout,
        payout,
        price,
        timestamp
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() != 1 {
        anyhow::bail!("failed to insert into collaborative_settlement_txs");
    }

    Ok(())
}

async fn insert_commit_tx(
    conn: &mut Transaction<'_, Sqlite>,
    id: OrderId,
    Commit { txid, timestamp }: Commit,
) -> Result<()> {
    let query_result = sqlx::query!(
        r#"
        INSERT INTO commit_txs
        (
            cfd_id,
            txid,
            timestamp
        )
        VALUES
        (
            (SELECT id FROM closed_cfds WHERE closed_cfds.uuid = $1),
            $2,
            $3
        )
        "#,
        id,
        txid,
        timestamp
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() != 1 {
        anyhow::bail!("failed to insert into commit_txs");
    }

    Ok(())
}

async fn insert_cet(
    conn: &mut Transaction<'_, Sqlite>,
    id: OrderId,
    Cet {
        txid,
        vout,
        payout,
        price,
        timestamp,
    }: Cet,
) -> Result<()> {
    let query_result = sqlx::query!(
        r#"
        INSERT INTO cets
        (
            cfd_id,
            txid,
            vout,
            payout,
            price,
            timestamp
        )
        VALUES
        (
            (SELECT id FROM closed_cfds WHERE closed_cfds.uuid = $1),
            $2, $3, $4, $5, $6
        )
        "#,
        id,
        txid,
        vout,
        payout,
        price,
        timestamp
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() != 1 {
        anyhow::bail!("failed to insert into cets");
    }

    Ok(())
}

async fn insert_refund_tx(
    conn: &mut Transaction<'_, Sqlite>,
    id: OrderId,
    Refund {
        txid,
        vout,
        payout,
        timestamp,
    }: Refund,
) -> Result<()> {
    let query_result = sqlx::query!(
        r#"
        INSERT INTO refund_txs
        (
            cfd_id,
            txid,
            vout,
            payout,
            timestamp
        )
        VALUES
        (
            (SELECT id FROM closed_cfds WHERE closed_cfds.uuid = $1),
            $2, $3, $4, $5
        )
        "#,
        id,
        txid,
        vout,
        payout,
        timestamp
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() != 1 {
        anyhow::bail!("failed to insert into refund_txs");
    }

    Ok(())
}

async fn insert_event_log(
    conn: &mut Transaction<'_, Sqlite>,
    id: OrderId,
    event_log: EventLog,
) -> Result<()> {
    for EventLogEntry { name, created_at } in event_log.0.iter() {
        let query_result = sqlx::query!(
            r#"
            INSERT INTO event_log (
                cfd_id,
                name,
                created_at
            )
            VALUES
            (
                (SELECT id FROM closed_cfds WHERE closed_cfds.uuid = $1),
                $2, $3
            )
            "#,
            id,
            name,
            created_at
        )
        .execute(&mut *conn)
        .await?;

        if query_result.rows_affected() != 1 {
            anyhow::bail!("failed to insert into event_log");
        }
    }

    Ok(())
}

async fn load_collaborative_settlement_tx(
    conn: &mut PoolConnection<Sqlite>,
    id: OrderId,
) -> Result<Option<CollaborativeSettlement>> {
    let row = sqlx::query_as!(
        CollaborativeSettlement,
        r#"
        SELECT
            txid as "txid: model::Txid",
            vout as "vout: model::Vout",
            payout as "payout: model::Payout",
            price as "price: model::Price",
            timestamp as "timestamp: model::Timestamp"
        FROM
            collaborative_settlement_txs
        JOIN
            closed_cfds c on c.id = collaborative_settlement_txs.cfd_id
        WHERE
            c.uuid = $1
        "#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row)
}

async fn load_commit_tx(conn: &mut PoolConnection<Sqlite>, id: OrderId) -> Result<Option<Txid>> {
    let txid = sqlx::query!(
        r#"
        SELECT
            txid as "txid: model::Txid"
        FROM
            commit_txs
        JOIN
            closed_cfds c on c.id = commit_txs.cfd_id
        WHERE
            c.uuid = $1
        "#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?
    .map(|row| row.txid);

    Ok(txid)
}

async fn load_cet(conn: &mut PoolConnection<Sqlite>, id: OrderId) -> Result<Option<Cet>> {
    let row = sqlx::query_as!(
        Cet,
        r#"
        SELECT
            txid as "txid: model::Txid",
            vout as "vout: model::Vout",
            payout as "payout: model::Payout",
            price as "price: model::Price",
            timestamp as "timestamp: model::Timestamp"
        FROM
            cets
        JOIN
            closed_cfds c on c.id = cets.cfd_id
        WHERE
            c.uuid = $1
        "#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row)
}

async fn load_refund_tx(conn: &mut PoolConnection<Sqlite>, id: OrderId) -> Result<Option<Refund>> {
    let row = sqlx::query_as!(
        Refund,
        r#"
        SELECT
            txid as "txid: model::Txid",
            vout as "vout: model::Vout",
            payout as "payout: model::Payout",
            timestamp as "timestamp: model::Timestamp"
        FROM
            refund_txs
        JOIN
            closed_cfds c on c.id = refund_txs.cfd_id
        WHERE
            c.uuid = $1
        "#,
        id
    )
    .fetch_optional(&mut *conn)
    .await?;

    Ok(row)
}

async fn delete_from_cfds_table(conn: &mut Transaction<'_, Sqlite>, id: OrderId) -> Result<()> {
    let query_result = sqlx::query!(
        r#"
        DELETE FROM
            cfds
        WHERE
            cfds.uuid = $1
        "#,
        id,
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() != 1 {
        anyhow::bail!("failed to delete from cfds");
    }

    Ok(())
}

async fn delete_from_events_table(conn: &mut Transaction<'_, Sqlite>, id: OrderId) -> Result<()> {
    let query_result = sqlx::query!(
        r#"
        DELETE FROM
            events
        WHERE events.cfd_id IN
            (SELECT id FROM cfds WHERE cfds.uuid = $1)
        "#,
        id,
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() < 1 {
        anyhow::bail!("failed to delete from events");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bdk::bitcoin::Amount;
    use bdk::bitcoin::SignedAmount;
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
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    #[tokio::test]
    async fn test_insert_and_load_cfd() {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        db.insert_cfd(&cfd).await.unwrap();
        let mut conn = db.inner.acquire().await.unwrap();
        let mut db_tx = conn.begin().await.unwrap();

        let super::Cfd {
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
        } = load_cfd_row(&mut db_tx, cfd.id()).await.unwrap();

        db_tx.commit().await.unwrap();

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
    async fn test_append_events() {
        let db = memory().await.unwrap();

        let cfd = dummy_cfd();
        db.insert_cfd(&cfd).await.unwrap();

        let timestamp = Timestamp::now();

        let event1 = CfdEvent {
            timestamp,
            id: cfd.id(),
            event: EventKind::OfferRejected,
        };

        db.append_event(event1.clone()).await.unwrap();

        let mut conn = db.inner.acquire().await.unwrap();

        let mut db_tx = conn.begin().await.unwrap();
        let events = load_cfd_events(&mut db_tx, cfd.id(), 0).await.unwrap();
        db_tx.commit().await.unwrap();
        assert_eq!(events, vec![event1.clone()]);

        let event2 = CfdEvent {
            timestamp,
            id: cfd.id(),
            event: EventKind::RevokeConfirmed,
        };

        db.append_event(event2.clone()).await.unwrap();

        // let mut conn = db.inner.acquire().await.unwrap();
        let mut db_tx = conn.begin().await.unwrap();
        let events = load_cfd_events(&mut db_tx, cfd.id(), 0).await.unwrap();
        db_tx.commit().await.unwrap();
        assert_eq!(events, vec![event1, event2])
    }

    #[tokio::test]
    async fn given_setup_failed_then_do_not_load_non_final_cfd() {
        let db = memory().await.unwrap();

        let cfd_final = dummy_cfd();
        db.insert_cfd(&cfd_final).await.unwrap();

        db.append_event(lock_confirmed(&cfd_final)).await.unwrap();
        db.append_event(setup_failed(&cfd_final)).await.unwrap();

        let cfd_ids = db.load_open_cfd_ids().await.unwrap();
        assert!(cfd_ids.is_empty());
    }

    #[tokio::test]
    async fn given_order_rejected_then_do_not_load_non_final_cfd() {
        let db = memory().await.unwrap();

        let cfd_final = dummy_cfd();
        db.insert_cfd(&cfd_final).await.unwrap();

        db.append_event(lock_confirmed(&cfd_final)).await.unwrap();
        db.append_event(order_rejected(&cfd_final)).await.unwrap();

        let cfd_ids = db.load_open_cfd_ids().await.unwrap();
        assert!(cfd_ids.is_empty());
    }

    #[tokio::test]
    async fn given_final_and_non_final_cfd_then_non_final_one_still_loaded() {
        let db = memory().await.unwrap();

        let cfd_not_final = dummy_cfd();
        db.insert_cfd(&cfd_not_final).await.unwrap();

        db.append_event(lock_confirmed(&cfd_not_final))
            .await
            .unwrap();

        let cfd_final = dummy_cfd();
        db.insert_cfd(&cfd_final).await.unwrap();

        db.append_event(lock_confirmed(&cfd_final)).await.unwrap();
        db.append_event(order_rejected(&cfd_final)).await.unwrap();

        let cfd_ids = db.load_open_cfd_ids().await.unwrap();

        assert_eq!(cfd_ids.len(), 1);
        assert_eq!(*cfd_ids.first().unwrap(), cfd_not_final.id())
    }

    #[tokio::test]
    async fn given_confirmed_settlement_when_move_cfds_to_closed_table_then_can_load_cfd_as_closed()
    {
        let db = memory().await.unwrap();

        let (cfd, contract_setup_completed, collaborative_settlement_completed) =
            cfd_collaboratively_settled();
        let order_id = cfd.id();

        db.insert_cfd(&cfd).await.unwrap();

        db.append_event(contract_setup_completed).await.unwrap();
        db.append_event(collaborative_settlement_completed)
            .await
            .unwrap();
        db.append_event(collab_settlement_confirmed(&cfd))
            .await
            .unwrap();

        db.move_to_closed_cfds().await.unwrap();

        let load_from_open = db.load_open_cfd::<DummyAggregate>(order_id, ()).await;
        let load_from_events = {
            let mut conn = db.inner.acquire().await.unwrap();
            let mut db_tx = conn.begin().await.unwrap();
            let res = load_cfd_events(&mut db_tx, order_id, 0).await.unwrap();
            db_tx.commit().await.unwrap();

            res
        };
        let load_from_closed = db.load_closed_cfd::<DummyAggregate>(order_id, ()).await;

        assert!(load_from_open.is_err());
        assert!(load_from_events.is_empty());
        assert!(load_from_closed.is_ok());
    }

    #[tokio::test]
    async fn given_settlement_not_confirmed_when_move_cfds_to_closed_table_then_cannot_load_cfd_as_closed(
    ) {
        let db = memory().await.unwrap();

        let (cfd, contract_setup_completed, collaborative_settlement_completed) =
            cfd_collaboratively_settled();
        let order_id = cfd.id();

        db.insert_cfd(&cfd).await.unwrap();

        db.append_event(contract_setup_completed).await.unwrap();
        db.append_event(collaborative_settlement_completed)
            .await
            .unwrap();

        db.move_to_closed_cfds().await.unwrap();

        let load_from_open = db.load_open_cfd::<DummyAggregate>(order_id, ()).await;
        let load_from_events = {
            let mut conn = db.inner.acquire().await.unwrap();
            let mut db_tx = conn.begin().await.unwrap();
            let res = load_cfd_events(&mut db_tx, order_id, 0).await.unwrap();
            db_tx.commit().await.unwrap();

            res
        };
        let load_from_closed = db.load_closed_cfd::<DummyAggregate>(order_id, ()).await;

        assert!(load_from_open.is_ok());
        assert_eq!(load_from_events.len(), 2);
        assert!(load_from_closed.is_err());
    }

    #[tokio::test]
    async fn given_confirmed_settlement_when_move_cfds_to_closed_table_then_projection_aggregate_stays_the_same(
    ) {
        let db = memory().await.unwrap();

        let (cfd, contract_setup_completed, collaborative_settlement_completed) =
            cfd_collaboratively_settled();
        let order_id = cfd.id();

        db.insert_cfd(&cfd).await.unwrap();

        db.append_event(contract_setup_completed).await.unwrap();
        db.append_event(collaborative_settlement_completed)
            .await
            .unwrap();

        db.append_event(collab_settlement_confirmed(&cfd))
            .await
            .unwrap();

        let projection_open = db
            .load_open_cfd::<crate::projection::Cfd>(order_id, bdk::bitcoin::Network::Testnet)
            .await
            .unwrap();
        let projection_open = projection_open.with_current_quote(None); // to update payout-related fields

        db.move_to_closed_cfds().await.unwrap();

        let projection_closed = db
            .load_closed_cfd::<crate::projection::Cfd>(order_id, bdk::bitcoin::Network::Testnet)
            .await
            .unwrap();

        // this comparison actually omits the `aggregated` field on
        // `projection::Cfd` because it is not used when aggregating
        // from a closed CFD
        assert_eq!(projection_open, projection_closed);
    }

    #[tokio::test]
    async fn insert_cet_roundtrip() {
        let db = memory().await.unwrap();

        let mut conn = db.inner.acquire().await.unwrap();
        let mut db_tx = conn.begin().await.unwrap();

        let id = OrderId::default();

        insert_dummy_closed_cfd(&mut db_tx, id).await.unwrap();

        let inserted = Cet {
            txid: Txid::new(bdk::bitcoin::Txid::default()),
            vout: Vout::new(0),
            payout: Payout::new(Amount::ONE_BTC),
            price: Price::new(dec!(40_000)).unwrap(),
            timestamp: Timestamp::new(10_000),
        };

        insert_cet(&mut db_tx, id, inserted).await.unwrap();
        db_tx.commit().await.unwrap();

        let loaded = load_cet(&mut conn, id).await.unwrap().unwrap();

        assert_eq!(inserted, loaded);
    }

    #[tokio::test]
    async fn insert_collaborative_settlement_tx_roundtrip() {
        let db = memory().await.unwrap();

        let mut conn = db.inner.acquire().await.unwrap();
        let mut db_tx = conn.begin().await.unwrap();

        let id = OrderId::default();

        insert_dummy_closed_cfd(&mut db_tx, id).await.unwrap();

        let inserted = CollaborativeSettlement {
            txid: Txid::new(bdk::bitcoin::Txid::default()),
            vout: Vout::new(0),
            payout: Payout::new(Amount::ONE_BTC),
            price: Price::new(dec!(40_000)).unwrap(),
            timestamp: Timestamp::new(10_000),
        };

        insert_collaborative_settlement_tx(&mut db_tx, id, inserted)
            .await
            .unwrap();
        db_tx.commit().await.unwrap();

        let loaded = load_collaborative_settlement_tx(&mut conn, id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(loaded, inserted);
    }

    #[tokio::test]
    async fn insert_commit_tx_roundtrip() {
        let db = memory().await.unwrap();

        let mut conn = db.inner.acquire().await.unwrap();
        let mut db_tx = conn.begin().await.unwrap();

        let id = OrderId::default();

        insert_dummy_closed_cfd(&mut db_tx, id).await.unwrap();

        let inserted = Commit {
            txid: Txid::new(bdk::bitcoin::Txid::default()),
            timestamp: Timestamp::new(10_000),
        };

        insert_commit_tx(&mut db_tx, id, inserted).await.unwrap();
        db_tx.commit().await.unwrap();

        let loaded = load_commit_tx(&mut conn, id).await.unwrap().unwrap();

        assert_eq!(loaded, inserted.txid);
    }

    #[tokio::test]
    async fn insert_refund_tx_roundtrip() {
        let db = memory().await.unwrap();

        let mut conn = db.inner.acquire().await.unwrap();
        let mut db_tx = conn.begin().await.unwrap();

        let id = OrderId::default();

        insert_dummy_closed_cfd(&mut db_tx, id).await.unwrap();

        let inserted = Refund {
            txid: Txid::new(bdk::bitcoin::Txid::default()),
            vout: Vout::new(0),
            payout: Payout::new(Amount::ONE_BTC),
            timestamp: Timestamp::new(10_000),
        };

        insert_refund_tx(&mut db_tx, id, inserted).await.unwrap();
        db_tx.commit().await.unwrap();

        let loaded = load_refund_tx(&mut conn, id).await.unwrap().unwrap();

        assert_eq!(loaded, inserted);
    }

    async fn insert_dummy_closed_cfd(
        conn: &mut Transaction<'_, Sqlite>,
        id: OrderId,
    ) -> Result<()> {
        let cfd = ClosedCfdInput {
            id,
            position: Position::Long,
            initial_price: Price::new(Decimal::ONE).unwrap(),
            taker_leverage: Leverage::TWO,
            n_contracts: Contracts::new(100),
            counterparty_network_identity: dummy_identity(),
            role: Role::Maker,
            fees: Fees::new(SignedAmount::ONE_BTC),
            expiry_timestamp: OffsetDateTime::now_utc(),
            lock: LockInput {
                txid: Txid::new(bdk::bitcoin::Txid::default()),
                dlc_vout: Vout::new(0),
                timestamp: Timestamp::new(4_000),
            },
            collaborative_settlement: None,
            commit: None,
            non_collaborative_settlement: None,
            refund: None,
        };

        insert_closed_cfd(conn, cfd).await?;

        Ok(())
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

    fn dummy_identity() -> Identity {
        Identity::new(x25519_dalek::PublicKey::from(
            *b"hello world, oh what a beautiful",
        ))
    }

    fn cfd_collaboratively_settled() -> (Cfd, CfdEvent, CfdEvent) {
        // 1|<RANDOM-ORDER-ID>|Long|41772.8325|2|24|100|
        // 69a42aa90da8b065b9532b62bff940a3ba07dbbb11d4482c7db83a7e049a9f1e|Taker|0|0|1
        let order_id = OrderId::default();
        let cfd = Cfd::new(
            order_id,
            Position::Long,
            Price::new(dec!(41_772.8325)).unwrap(),
            Leverage::TWO,
            Duration::hours(24),
            Role::Taker,
            Usd::new(dec!(100)),
            "69a42aa90da8b065b9532b62bff940a3ba07dbbb11d4482c7db83a7e049a9f1e"
                .parse()
                .unwrap(),
            OpeningFee::new(Amount::ZERO),
            FundingRate::default(),
            TxFeeRate::default(),
        );

        let contract_setup_completed =
            std::fs::read_to_string("./src/test_events/contract_setup_completed.json").unwrap();
        let contract_setup_completed =
            serde_json::from_str::<EventKind>(&contract_setup_completed).unwrap();
        let contract_setup_completed = CfdEvent {
            timestamp: Timestamp::now(),
            id: order_id,
            event: contract_setup_completed,
        };

        let collaborative_settlement_completed =
            std::fs::read_to_string("./src/test_events/collaborative_settlement_completed.json")
                .unwrap();
        let collaborative_settlement_completed =
            serde_json::from_str::<EventKind>(&collaborative_settlement_completed).unwrap();
        let collaborative_settlement_completed = CfdEvent {
            timestamp: Timestamp::now(),
            id: order_id,
            event: collaborative_settlement_completed,
        };

        (
            cfd,
            contract_setup_completed,
            collaborative_settlement_completed,
        )
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

    #[derive(Clone)]
    struct DummyAggregate;

    impl CfdAggregate for DummyAggregate {
        type CtorArgs = ();

        fn new(_: Self::CtorArgs, _: crate::db::Cfd) -> Self {
            Self
        }

        fn apply(self, _: CfdEvent) -> Self {
            Self
        }

        fn version(&self) -> u32 {
            0
        }
    }

    impl ClosedCfdAggregate for DummyAggregate {
        fn new_closed(_: Self::CtorArgs, _: ClosedCfd) -> Self {
            Self
        }
    }
}
