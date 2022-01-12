use crate::auth::Authenticated;
use crate::model::cfd::OrderId;
use crate::model::Identity;
use crate::model::Price;
use crate::model::Usd;
use crate::model::WalletInfo;
use crate::oracle;
use crate::projection::Cfd;
use crate::projection::Feeds;
use crate::routes::EmbeddedFileExt;
use crate::to_sse_event::ToSseEvent;
use crate::wallet;
use crate::MakerActorSystem;
use anyhow::Result;
use bdk::bitcoin::Network;
use http_api_problem::HttpApiProblem;
use http_api_problem::StatusCode;
use rocket::http::ContentType;
use rocket::http::Status;
use rocket::response::stream::EventStream;
use rocket::response::Responder;
use rocket::serde::json::Json;
use rocket::State;
use rust_embed::RustEmbed;
use serde::Deserialize;
use std::borrow::Cow;
use std::path::PathBuf;
use tokio::select;
use tokio::sync::watch;

pub type Maker = MakerActorSystem<oracle::Actor, wallet::Actor>;

#[allow(clippy::too_many_arguments)]
#[rocket::get("/feed")]
pub async fn maker_feed(
    rx: &State<Feeds>,
    rx_wallet: &State<watch::Receiver<Option<WalletInfo>>>,
    _auth: Authenticated,
) -> EventStream![] {
    let rx = rx.inner();
    let mut rx_cfds = rx.cfds.clone();
    let mut rx_order = rx.order.clone();
    let mut rx_wallet = rx_wallet.inner().clone();
    let mut rx_quote = rx.quote.clone();
    let mut rx_connected_takers = rx.connected_takers.clone();

    EventStream! {
        let wallet_info = rx_wallet.borrow().clone();
        yield wallet_info.to_sse_event();

        let order = rx_order.borrow().clone();
        yield order.to_sse_event();

        let quote = rx_quote.borrow().clone();
        yield quote.to_sse_event();

        let cfds = rx_cfds.borrow().clone();
        yield cfds.to_sse_event();

        let takers = rx_connected_takers.borrow().clone();
        yield takers.to_sse_event();

        loop{
            select! {
                Ok(()) = rx_wallet.changed() => {
                    let wallet_info = rx_wallet.borrow().clone();
                    yield wallet_info.to_sse_event();
                },
                Ok(()) = rx_order.changed() => {
                    let order = rx_order.borrow().clone();
                    yield order.to_sse_event();
                }
                Ok(()) = rx_connected_takers.changed() => {
                    let takers = rx_connected_takers.borrow().clone();
                    yield takers.to_sse_event();
                }
                Ok(()) = rx_cfds.changed() => {
                    let cfds = rx_cfds.borrow().clone();
                    yield cfds.to_sse_event();
                }
                Ok(()) = rx_quote.changed() => {
                    let quote = rx_quote.borrow().clone();
                    yield quote.to_sse_event();
                }
            }
        }
    }
}

/// The maker POSTs this to create a new CfdOrder
// TODO: Use Rocket form?
#[derive(Debug, Clone, Deserialize)]
pub struct CfdNewOrderRequest {
    pub price: Price,
    // TODO: [post-MVP] Representation of the contract size; at the moment the contract size is
    // always 1 USD
    pub min_quantity: Usd,
    pub max_quantity: Usd,
    pub fee_rate: Option<u32>,
}

#[rocket::post("/order/sell", data = "<order>")]
pub async fn post_sell_order(
    order: Json<CfdNewOrderRequest>,
    maker: &State<Maker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    maker
        .new_order(
            order.price,
            order.min_quantity,
            order.max_quantity,
            order.fee_rate,
        )
        .await
        .map_err(|e| {
            HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
                .title("Posting offer failed")
                .detail(e.to_string())
        })?;

    Ok(())
}

#[rocket::post("/cfd/<id>/accept")]
pub async fn accept_order(
    id: OrderId,
    maker: &State<Maker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    maker.accept_order(id).await.map_err(|e| {
        HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
            .title("Failed to accept order")
            .detail(e.to_string())
    })?;

    Ok(())
}

#[rocket::post("/cfd/<id>/reject")]
pub async fn reject_order(
    id: OrderId,
    maker: &State<Maker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    maker.reject_order(id).await.map_err(|e| {
        HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
            .title("Failed to reject order")
            .detail(e.to_string())
    })?;

    Ok(())
}

#[rocket::post("/cfd/<id>/acceptSettlement")]
pub async fn accept_settlement(
    id: OrderId,
    maker: &State<Maker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    maker.accept_settlement(id).await.map_err(|e| {
        HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
            .title("Failed to accept settlement")
            .detail(e.to_string())
    })?;

    Ok(())
}

#[rocket::post("/cfd/<id>/rejectSettlement")]
pub async fn reject_settlement(
    id: OrderId,
    maker: &State<Maker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    maker.reject_settlement(id).await.map_err(|e| {
        HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
            .title("Failed to accept settlement")
            .detail(e.to_string())
    })?;

    Ok(())
}

#[rocket::post("/cfd/<id>/acceptRollover")]
pub async fn accept_rollover(
    id: OrderId,
    maker: &State<Maker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    maker.accept_rollover(id).await.map_err(|e| {
        HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
            .title("Failed to accept rollover")
            .detail(e.to_string())
    })?;

    Ok(())
}

#[rocket::post("/cfd/<id>/rejectRollover")]
pub async fn reject_rollover(
    id: OrderId,
    maker: &State<Maker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    maker.reject_rollover(id).await.map_err(|e| {
        HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
            .title("Failed to accept rollover")
            .detail(e.to_string())
    })?;

    Ok(())
}

#[rocket::post("/cfd/<id>/commit")]
pub async fn commit(
    id: OrderId,
    maker: &State<Maker>,
    _auth: Authenticated,
) -> Result<(), HttpApiProblem> {
    maker.commit(id).await.map_err(|e| {
        HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
            .title("Failed to commit CFD")
            .detail(e.to_string())
    })?;

    Ok(())
}

#[rocket::get("/alive")]
pub fn get_health_check() {}

#[derive(RustEmbed)]
#[folder = "../maker-frontend/dist/maker"]
struct Asset;

#[rocket::get("/assets/<file..>")]
pub fn dist<'r>(file: PathBuf, _auth: Authenticated) -> impl Responder<'r, 'static> {
    let filename = format!("assets/{}", file.display().to_string());
    Asset::get(&filename).into_response(file)
}

#[rocket::get("/<_paths..>", format = "text/html")]
pub fn index<'r>(_paths: PathBuf, _auth: Authenticated) -> impl Responder<'r, 'static> {
    let asset = Asset::get("index.html").ok_or(Status::NotFound)?;
    Ok::<(ContentType, Cow<[u8]>), Status>((ContentType::HTML, asset.data))
}

#[derive(Debug, Clone, Deserialize)]
pub struct WithdrawRequest {
    address: bdk::bitcoin::Address,
    #[serde(with = "::bdk::bitcoin::util::amount::serde::as_btc")]
    amount: bdk::bitcoin::Amount,
    fee: f32,
}

#[rocket::post("/withdraw", data = "<withdraw_request>")]
pub async fn post_withdraw_request(
    withdraw_request: Json<WithdrawRequest>,
    maker: &State<Maker>,
    network: &State<Network>,
    _auth: Authenticated,
) -> Result<String, HttpApiProblem> {
    let amount =
        (withdraw_request.amount != bdk::bitcoin::Amount::ZERO).then(|| withdraw_request.amount);

    let txid = maker
        .withdraw(
            amount,
            withdraw_request.address.clone(),
            withdraw_request.fee,
        )
        .await
        .map_err(|e| {
            HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
                .title("Could not proceed with withdraw request")
                .detail(e.to_string())
        })?;

    let url = match network.inner() {
        Network::Bitcoin => format!("https://mempool.space/tx/{}", txid),
        Network::Testnet => format!("https://mempool.space/testnet/tx/{}", txid),
        Network::Signet => format!("https://mempool.space/signet/tx/{}", txid),
        Network::Regtest => txid.to_string(),
    };

    Ok(url)
}

#[rocket::get("/cfds")]
pub async fn get_cfds<'r>(
    rx: &State<Feeds>,
    _auth: Authenticated,
) -> Result<Json<Vec<Cfd>>, HttpApiProblem> {
    let rx = rx.inner();
    let rx_cfds = rx.cfds.clone();
    let cfds = rx_cfds.borrow().clone();

    Ok(Json(cfds))
}

#[rocket::get("/takers")]
pub async fn get_takers<'r>(
    rx: &State<Feeds>,
    _auth: Authenticated,
) -> Result<Json<Vec<Identity>>, HttpApiProblem> {
    let rx = rx.inner();
    let rx_connected_takers = rx.connected_takers.clone();
    let takers = rx_connected_takers.borrow().clone();

    Ok(Json(takers))
}
