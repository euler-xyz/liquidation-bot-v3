//! Helpers that are shared between tests.

use alloy::{
    primitives::Address,
    providers::{DynProvider, Provider, ProviderBuilder, ext::AnvilApi},
};
use anyhow::{Context, Result, bail};
use reqwest::Url;

/// Ensures that all the given contracts exist on an Anvil fork.
///
/// When forking an old block some of the configured contracts (mostly the lenses) may not
/// have been deployed yet at that block. For every address that has no code on the fork,
/// this fetches the contract code as it currently exists on the live chain and injects it
/// into the fork. That way tests can always use the addresses from the chain configs
/// without having to worry about whether a specific contract already existed at the
/// forked block.
///
/// NOTE: This only copies code, not storage. So it only works for stateless contracts
/// (like the lenses) or contracts whose configuration lives in immutables (which are part
/// of the code).
pub async fn ensure_contracts_on_fork(
    fork: &DynProvider,
    live_rpc_url: &Url,
    addresses: &[Address],
) -> Result<()> {
    let live = ProviderBuilder::new()
        .connect_http(live_rpc_url.clone())
        .erased();

    for address in addresses {
        // The contract already exists at the forked block, nothing to do.
        if !fork
            .get_code_at(*address)
            .await
            .with_context(|| format!("While checking the code of {address} on the fork"))?
            .is_empty()
        {
            continue;
        }

        // Fetch the code as it currently exists on the live chain.
        let code = live
            .get_code_at(*address)
            .await
            .with_context(|| format!("While fetching the code of {address} from the live chain"))?;

        if code.is_empty() {
            bail!(
                "Contract {address} does not exist on the live chain either, this is likely a misconfiguration"
            );
        }

        fork.anvil_set_code(*address, code)
            .await
            .with_context(|| format!("While injecting the code of {address} into the fork"))?;
    }

    Ok(())
}
