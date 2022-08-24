use crate::models;
use crate::models::into_complete_fee_and_flow;
use anyhow::bail;
use anyhow::Result;
use bdk::bitcoin::hashes::hex::ToHex;
use delete::delete;
use model::Cet;
use model::CompleteFee;
use model::Dlc;
use model::FundingFee;
use model::RevokedCommit;
use models::BitMexPriceEventId;
use sqlx::SqliteConnection;
use sqlx::SqliteExecutor;

mod delete;

/// Overwrite a CFD's latest rollover data.
///
/// After a successful rollover, we can forget about the previous `Dlc`, `FundingFee` and
/// `CompleteFee`.
pub async fn overwrite(
    conn: &mut SqliteConnection,
    event_id: i64,
    order_id: models::OrderId,
    dlc: Dlc,
    funding_fee: FundingFee,
    complete_fee: Option<CompleteFee>,
) -> Result<()> {
    delete(&mut *conn, order_id).await?;

    insert_rollover_completed_event_data(
        &mut *conn,
        event_id,
        &dlc,
        funding_fee,
        complete_fee,
        order_id,
    )
    .await?;

    for revoked in dlc.revoked_commit {
        insert_revoked_commit_transaction(&mut *conn, order_id, revoked).await?;
    }

    for (event_id, cets) in dlc.cets {
        for cet in cets {
            insert_cet(&mut *conn, event_id.into(), order_id, cet).await?;
        }
    }

    Ok(())
}

/// Inserts RolloverCompleted data and returns the resulting rowid
async fn insert_rollover_completed_event_data(
    conn: impl SqliteExecutor<'_>,
    event_id: i64,
    dlc: &Dlc,
    funding_fee: FundingFee,
    complete_fee: Option<CompleteFee>,
    order_id: models::OrderId,
) -> Result<()> {
    let (lock_tx, lock_tx_descriptor) = dlc.lock.clone();
    let (commit_tx, commit_adaptor_signature, commit_descriptor) = dlc.commit.clone();
    let (refund_tx, refund_signature) = dlc.refund.clone();

    let lock_tx = models::Transaction::from(lock_tx);
    let commit_tx = models::Transaction::from(commit_tx);
    let refund_tx = models::Transaction::from(refund_tx);

    let commit_adaptor_signature = models::AdaptorSignature::from(commit_adaptor_signature);

    // casting because u64 is not implemented for sqlx: https://github.com/launchbadge/sqlx/pull/919#discussion_r557256333
    let funding_fee_as_sat = funding_fee.fee.as_sat() as i64;
    // TODO: these seem to be redundant and should be in `cfds` table only
    let maker_lock_amount = dlc.maker_lock_amount.as_sat() as i64;
    let taker_lock_amount = dlc.taker_lock_amount.as_sat() as i64;

    let maker_address = dlc.maker_address.to_string();
    let taker_address = dlc.taker_address.to_string();

    let lock_tx_descriptor = lock_tx_descriptor.to_string();
    let commit_tx_descriptor = commit_descriptor.to_string();
    let refund_signature = refund_signature.to_string();

    let identity = models::SecretKey::from(dlc.identity);
    let publish_sk = models::SecretKey::from(dlc.publish);
    let revocation_secret = models::SecretKey::from(dlc.revocation);
    let identity_counterparty = models::PublicKey::from(dlc.identity_counterparty);
    let publish_pk_counterparty = models::PublicKey::from(dlc.publish_pk_counterparty);
    let revocation_pk_counterparty = models::PublicKey::from(dlc.revocation_pk_counterparty);
    let rate = models::FundingRate::from(funding_fee.rate);
    let settlement_event_id = models::BitMexPriceEventId::from(dlc.settlement_event_id);

    let (complete_fee, complete_fee_flow) = into_complete_fee_and_flow(complete_fee);

    let query_result = sqlx::query!(
        r#"
            insert into rollover_completed_event_data (
                cfd_id,
                event_id,
                settlement_event_id,
                refund_timelock,
                funding_fee,
                rate,
                identity,
                identity_counterparty,
                maker_address,
                taker_address,
                maker_lock_amount,
                taker_lock_amount,
                publish_sk,
                publish_pk_counterparty,
                revocation_secret,
                revocation_pk_counterparty,
                lock_tx,
                lock_tx_descriptor,
                commit_tx,
                commit_adaptor_signature,
                commit_descriptor,
                refund_tx,
                refund_signature,
                complete_fee,
                complete_fee_flow
            ) values (
            (select id from cfds where cfds.order_id = $1),
            $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23, $24, $25
            )
        "#,
        order_id,
        event_id,
        settlement_event_id,
        dlc.refund_timelock,
        funding_fee_as_sat,
        rate,
        identity,
        identity_counterparty,
        maker_address,
        taker_address,
        maker_lock_amount,
        taker_lock_amount,
        publish_sk,
        publish_pk_counterparty,
        revocation_secret,
        revocation_pk_counterparty,
        lock_tx,
        lock_tx_descriptor,
        commit_tx,
        commit_adaptor_signature,
        commit_tx_descriptor,
        refund_tx,
        refund_signature,
        complete_fee,
        complete_fee_flow,
    )
    .execute(conn)
    .await?;

    if query_result.rows_affected() != 1 {
        bail!("failed to insert rollover event data");
    }
    Ok(())
}

async fn insert_revoked_commit_transaction(
    conn: &mut SqliteConnection,
    order_id: models::OrderId,
    revoked: RevokedCommit,
) -> Result<()> {
    let revoked_tx_script_pubkey = revoked.script_pubkey.to_hex();
    let revocation_sk_theirs = models::SecretKey::from(revoked.revocation_sk_theirs);
    let revocation_sk_ours = revoked.revocation_sk_ours.map(models::SecretKey::from);
    let publication_pk_theirs = models::PublicKey::from(revoked.publication_pk_theirs);
    let encsig_ours = models::AdaptorSignature::from(revoked.encsig_ours);
    let txid = models::Txid::from(revoked.txid);
    let settlement_event_id = revoked
        .settlement_event_id
        .map(models::BitMexPriceEventId::from);

    let (complete_fee, complete_fee_flow) = into_complete_fee_and_flow(revoked.complete_fee);

    let query_result = sqlx::query!(
        r#"
                insert into revoked_commit_transactions (
                    cfd_id,
                    encsig_ours,
                    publication_pk_theirs,
                    revocation_sk_theirs,
                    script_pubkey,
                    txid,
                    settlement_event_id,
                    complete_fee,
                    complete_fee_flow,
                    revocation_sk_ours
                ) values ( (select id from cfds where cfds.order_id = $1), $2, $3, $4, $5, $6, $7, $8, $9, $10 )
            "#,
        order_id,
        encsig_ours,
        publication_pk_theirs,
        revocation_sk_theirs,
        revoked_tx_script_pubkey,
        txid,
        settlement_event_id,
        complete_fee,
        complete_fee_flow,
        revocation_sk_ours
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() != 1 {
        bail!("failed to insert revoked transaction data");
    }
    Ok(())
}

async fn insert_cet(
    conn: &mut SqliteConnection,
    event_id: BitMexPriceEventId,
    order_id: models::OrderId,
    cet: Cet,
) -> Result<()> {
    let maker_amount = cet.maker_amount.as_sat() as i64;
    let taker_amount = cet.taker_amount.as_sat() as i64;
    let n_bits = cet.n_bits as i64;
    let range_start = *cet.range.start() as i64;
    let range_end = *cet.range.end() as i64;
    let adaptor_sig = models::AdaptorSignature::from(cet.adaptor_sig);

    let txid = cet.txid.to_string();
    let query_result = sqlx::query!(
        r#"
                insert into open_cets (
                    cfd_id,
                    oracle_event_id,
                    adaptor_sig,
                    maker_amount,
                    taker_amount,
                    n_bits,
                    range_start,
                    range_end,
                    txid
                ) values ( (select id from cfds where cfds.order_id = $1), $2, $3, $4, $5, $6, $7, $8, $9 )
            "#,
        order_id,
        event_id,
        adaptor_sig,
        maker_amount,
        taker_amount,
        n_bits,
        range_start,
        range_end,
        txid,
    )
    .execute(&mut *conn)
    .await?;

    if query_result.rows_affected() != 1 {
        bail!("failed to insert cet data");
    }
    Ok(())
}
