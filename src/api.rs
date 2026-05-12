use axum::{Json, Router, extract::State, http::StatusCode, routing::get};
use http::Method;
use serde::Serialize;
use std::sync::Arc;
use tower::ServiceBuilder;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use crate::{
    account::AccountSolvency,
    accounts::AccountsTracker,
    oracles::{OracleInformation, OraclesCache},
    types::{Account, OracleIdentifier},
};

/// Contains the state and internal knowledge of this instance of the liquidation bot.
#[derive(Clone)]
pub struct BotState {
    pub accounts: Arc<AccountsTracker>,
    pub oracles: OraclesCache,
    pub state: tokio::sync::watch::Receiver<BotHealth>,
}

pub async fn serve(state: BotState) {
    let cors = CorsLayer::new()
        // allow `GET` and `POST` when accessing the resource
        .allow_methods([Method::GET, Method::POST])
        // allow requests from any origin
        .allow_origin(Any);

    // build our application with a single route
    let app = Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .route("/health", get(health))
        .route("/accounts", get(get_accounts))
        .route("/oracles", get(get_oracles))
        .layer(ServiceBuilder::new().layer(cors))
        .with_state(state);

    // run our app with hyper, listening globally on port 3000
    let listener = match tokio::net::TcpListener::bind("0.0.0.0:3000").await {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(
                "Could not bind the API to the port, unable to start API service, err: {}",
                err
            );
            return;
        }
    };

    info!("Serving observability API at port 3000");

    match axum::serve(listener, app).await {
        Ok(_) => {
            info!("Stopped serving observability API");
        }
        Err(err) => {
            tracing::error!("Issue when serving observability API, err: {}", err);
        }
    };
}

#[derive(Serialize, Clone)]
pub enum BotHealth {
    Healthy,
    Syncing,
    Error(String),
}

async fn health(State(state): State<BotState>) -> (StatusCode, Json<BotHealth>) {
    (StatusCode::OK, Json(state.state.borrow().clone()))
}

#[derive(Serialize)]
struct AccountInformation {
    // Details on the account and its assets/debts.
    account: Account,
    /// The current health of the account.
    health: Option<AccountSolvency>,
    /// Reports all the oracles that this account depends on.
    oracles: Vec<OracleIdentifier>,
}

/// Exposes the accounts being tracked and all the information we have on it.
async fn get_accounts(State(state): State<BotState>) -> Json<Vec<AccountInformation>> {
    // Get all the accounts the bot is aware of.
    // Then for each account calculate the health score and report what oracles it is dependent on.
    Json(
        state
            .accounts
            .all_accounts()
            .iter()
            .map(|a| AccountInformation {
                account: a.clone(),
                health: a.calculate_health(&state.oracles).ok(),
                oracles: a.dependent_on(),
            })
            .collect(),
    )
}

/// Exposes all details on oracles that the bot is aware of.
async fn get_oracles(State(state): State<BotState>) -> Json<Vec<OracleInformation>> {
    Json(state.oracles.all())
}
