use std::sync::Arc;

use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use tracing::info;

use crate::{
    account::AccountSolvency, accounts::AccountsTracker, oracles::OraclesCache, types::Account,
};

/// Contains the state and internal knowledge of this instance of the liquidation bot.
#[derive(Clone)]
pub struct BotState {
    pub accounts: Arc<AccountsTracker>,
    pub oracles: OraclesCache,
}

pub async fn serve(state: BotState) {
    // build our application with a single route
    let app = Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .route("/accounts", get(get_accounts))
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

#[derive(Serialize)]
struct AccountInformation {
    account: Account,
    health: Option<AccountSolvency>,
}

/// Exposes the accounts being tracked and all the information we have on it.
async fn get_accounts(State(state): State<BotState>) -> Json<Vec<AccountInformation>> {
    // Get all the accounts the bot is aware of.
    // Then for each account calculate the health score.
    Json(
        state
            .accounts
            .all_accounts()
            .iter()
            .map(|a| AccountInformation {
                account: a.clone(),
                health: a.calculate_health(&state.oracles).ok(),
            })
            .collect(),
    )
}
