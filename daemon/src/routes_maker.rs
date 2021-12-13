use anyhow::Result;
use bdk::bitcoin::Network;
use daemon::auth::Authenticated;
use daemon::maker_cfd;
use daemon::maker_inc_connections;
use daemon::model::cfd::OrderId;
use daemon::model::Price;
use daemon::model::Usd;
use daemon::model::WalletInfo;
use daemon::monitor;
use daemon::oracle;
use daemon::projection::Cfd;
use daemon::projection::CfdAction;
use daemon::projection::Feeds;
use daemon::projection::Identity;
use daemon::routes::EmbeddedFileExt;
use daemon::to_sse_event::ToSseEvent;
use daemon::wallet;
use http_api_problem::HttpApiProblem;
use http_api_problem::StatusCode;
use rocket::http::ContentType;
use rocket::http::Header;
use rocket::http::Status;
use rocket::response::status;
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
use xtra::prelude::*;

pub type Maker = xtra::Address<
    maker_cfd::Actor<oracle::Actor, monitor::Actor, maker_inc_connections::Actor, wallet::Actor>,
>;

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
    cfd_actor: &State<Maker>,
    _auth: Authenticated,
) -> Result<status::Accepted<()>, HttpApiProblem> {
    cfd_actor
        .send(maker_cfd::NewOrder {
            price: order.price,
            min_quantity: order.min_quantity,
            max_quantity: order.max_quantity,
            fee_rate: order.fee_rate.unwrap_or(1),
        })
        .await
        .unwrap_or_else(|e| anyhow::bail!(e))
        .map_err(|e| {
            HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
                .title("Posting offer failed")
                .detail(e.to_string())
        })?;

    Ok(status::Accepted(None))
}

/// A "catcher" for all 401 responses, triggers the browser's basic auth implementation.
#[rocket::catch(401)]
pub fn unauthorized() -> PromptAuthentication {
    PromptAuthentication {
        inner: (),
        www_authenticate: Header::new("WWW-Authenticate", r#"Basic charset="UTF-8"#),
    }
}

/// A rocket responder that prompts the user to sign in to access the API.
#[derive(rocket::Responder)]
#[response(status = 401)]
pub struct PromptAuthentication {
    inner: (),
    www_authenticate: Header<'static>,
}

#[rocket::post("/cfd/<id>/<action>")]
pub async fn post_cfd_action(
    id: OrderId,
    action: CfdAction,
    cfd_actor: &State<Maker>,
    _auth: Authenticated,
) -> Result<status::Accepted<()>, HttpApiProblem> {
    use maker_cfd::*;

    let result = match action {
        CfdAction::AcceptOrder => cfd_actor.send(AcceptOrder { order_id: id }).await,
        CfdAction::RejectOrder => cfd_actor.send(RejectOrder { order_id: id }).await,
        CfdAction::AcceptSettlement => cfd_actor.send(AcceptSettlement { order_id: id }).await,
        CfdAction::RejectSettlement => cfd_actor.send(RejectSettlement { order_id: id }).await,
        CfdAction::AcceptRollOver => cfd_actor.send(AcceptRollOver { order_id: id }).await,
        CfdAction::RejectRollOver => cfd_actor.send(RejectRollOver { order_id: id }).await,
        CfdAction::Commit => cfd_actor.send(Commit { order_id: id }).await,
        CfdAction::Settle => {
            let msg = "Collaborative settlement can only be triggered by taker";
            tracing::error!(msg);
            return Err(HttpApiProblem::new(StatusCode::BAD_REQUEST).detail(msg));
        }
    };

    result.unwrap_or_else(|e| anyhow::bail!(e)).map_err(|e| {
        HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
            .title(action.to_string() + " failed")
            .detail(e.to_string())
    })?;

    Ok(status::Accepted(None))
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
    wallet: &State<Address<wallet::Actor>>,
    network: &State<Network>,
    _auth: Authenticated,
) -> Result<String, HttpApiProblem> {
    let amount =
        (withdraw_request.amount != bdk::bitcoin::Amount::ZERO).then(|| withdraw_request.amount);

    let txid = wallet
        .send(wallet::Withdraw {
            amount,
            address: withdraw_request.address.clone(),
            fee: Some(bdk::FeeRate::from_sat_per_vb(withdraw_request.fee)),
        })
        .await
        .map_err(|e| {
            HttpApiProblem::new(StatusCode::INTERNAL_SERVER_ERROR)
                .title("Could not proceed with withdraw request")
                .detail(e.to_string())
        })?
        .map_err(|e| {
            HttpApiProblem::new(StatusCode::BAD_REQUEST)
                .title("Could not withdraw funds")
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

#[cfg(test)]
mod tests {
    use super::*;
    use daemon::auth::Password;
    use rocket::http::Header;
    use rocket::http::Status;
    use rocket::local::blocking::Client;
    use rocket::Build;
    use rocket::Rocket;

    #[test]
    fn routes_are_password_protected() {
        let client = Client::tracked(rocket()).unwrap();

        let response = client.get("/protected").dispatch();

        assert_eq!(response.status(), Status::Unauthorized);
        assert_eq!(
            response.headers().get_one("WWW-Authenticate"),
            Some(r#"Basic charset="UTF-8"#)
        );
    }

    #[test]
    fn correct_password_grants_access() {
        let client = Client::tracked(rocket()).unwrap();

        let response = client.get("/protected").header(auth_header()).dispatch();

        assert_eq!(response.status(), Status::Ok);
    }

    #[rocket::get("/protected")]
    async fn protected(_auth: Authenticated) {}

    /// Constructs a Rocket instance for testing.
    fn rocket() -> Rocket<Build> {
        rocket::build()
            .manage(Password::from(*b"Now I'm feelin' so fly like a G6"))
            .mount("/", rocket::routes![protected])
            .register("/", rocket::catchers![unauthorized])
    }

    /// Creates an "Authorization" header that matches the password above,
    /// in particular it has been created through:
    /// ```
    /// base64(maker:hex("Now I'm feelin' so fly like a G6"))
    /// ```
    fn auth_header() -> Header<'static> {
        Header::new(
            "Authorization",
            "Basic bWFrZXI6NGU2Zjc3MjA0OTI3NmQyMDY2NjU2NTZjNjk2ZTI3MjA3MzZmMjA2NjZjNzkyMDZjNjk2YjY1MjA2MTIwNDczNg==",
        )
    }
}
