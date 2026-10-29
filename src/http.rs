//! Shared configuration for every HTTP client the bot uses, RPC providers included.

use std::time::Duration;

use tracing::error;

/// How long a single HTTP request (including every RPC call) may take before it is aborted.
/// Without a timeout a request to an endpoint that stops responding hangs forever, and with it
/// whatever was waiting on it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// How long establishing a connection may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A client builder with the timeouts applied, for clients that need further configuration.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
}

/// Builds a client from a builder. Building only fails if the TLS backend can not be
/// initialized, in which case we fall back to a default client (which would hit the same issue
/// on its first request, but that is reported where the request is made).
pub fn build(builder: reqwest::ClientBuilder) -> reqwest::Client {
    builder.build().unwrap_or_else(|err| {
        error!("Could not build the HTTP client, falling back to a client without timeouts, err: {err:?}");
        reqwest::Client::new()
    })
}

/// A client with the timeouts applied.
pub fn client() -> reqwest::Client {
    build(client_builder())
}

/// A client with the timeouts applied for RPC providers, use with
/// `ProviderBuilder::connect_reqwest`. Alloy depends on a different reqwest version than we do,
/// so this is built with the one it re-exports.
pub fn rpc_client() -> alloy::transports::http::reqwest::Client {
    alloy::transports::http::reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .unwrap_or_else(|err| {
            error!("Could not build the RPC client, falling back to a client without timeouts, err: {err:?}");
            alloy::transports::http::reqwest::Client::new()
        })
}
