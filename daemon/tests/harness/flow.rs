use anyhow::{Context, Result};
use daemon::model::cfd::Cfd;
use daemon::projection::CfdOrder;
use daemon::tokio_ext::FutureExt;
use std::time::Duration;
use tokio::sync::watch;

/// Waiting time for the time on the watch channel before returning error
const NEXT_WAIT_TIME: Duration = Duration::from_secs(if cfg!(debug_assertions) { 180 } else { 30 });

/// Returns the first `Cfd` from both channels
///
/// Ensures that there is only one `Cfd` present in both channels.
pub async fn next_cfd(
    rx_a: &mut watch::Receiver<Vec<Cfd>>,
    rx_b: &mut watch::Receiver<Vec<Cfd>>,
) -> Result<(Cfd, Cfd)> {
    let (a, b) = tokio::join!(next(rx_a), next(rx_b));
    let (a, b) = (a?, b?);

    assert_eq!(a.len(), 1);
    assert_eq!(b.len(), 1);

    Ok((a.first().unwrap().clone(), b.first().unwrap().clone()))
}

pub async fn next_order(
    rx_a: &mut watch::Receiver<Option<CfdOrder>>,
    rx_b: &mut watch::Receiver<Option<CfdOrder>>,
) -> Result<(CfdOrder, CfdOrder)> {
    let (a, b) = tokio::join!(next_some(rx_a), next_some(rx_b));

    Ok((a?, b?))
}

/// Returns the value if the next Option received on the stream is Some
pub async fn next_some<T>(rx: &mut watch::Receiver<Option<T>>) -> Result<T>
where
    T: Clone,
{
    next(rx)
        .await?
        .context("Received None when Some was expected")
}

/// Returns true if the next Option received on the stream is None
///
/// Returns false if Some is received.
pub async fn is_next_none<T>(rx: &mut watch::Receiver<Option<T>>) -> Result<bool>
where
    T: Clone,
{
    Ok(next(rx).await?.is_none())
}

/// Returns watch channel value upon change
pub async fn next_custom_time<T>(rx: &mut watch::Receiver<T>, wait_time: Duration) -> Result<T>
where
    T: Clone,
{
    rx.changed().timeout(wait_time).await.context(format!(
        "No change in channel within {} seconds",
        wait_time.as_secs()
    ))??;

    Ok(rx.borrow().clone())
}

/// Returns watch channel value upon change
pub async fn next<T>(rx: &mut watch::Receiver<T>) -> Result<T>
where
    T: Clone,
{
    next_custom_time(rx, NEXT_WAIT_TIME).await
}
