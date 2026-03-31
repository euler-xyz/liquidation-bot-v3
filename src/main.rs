use alloy::{
    primitives::{Address, U256, address},
    providers::{DynProvider, Provider, ProviderBuilder},
};
use std::{collections::HashMap, sync::Arc};

use crate::{
    lens::{AccountLens, fetch_account},
    types::Vault,
    vaults::Vaults,
};

mod accounts;
mod lens;
mod subgraph;
mod types;
mod vaults;

#[tokio::main]
async fn main() {
    let provider = ProviderBuilder::new().connect_http("".parse().unwrap());

    // Our singleton vault store.
    let vaults = &mut Vaults::new(address!("0xA18D79deB85C414989D7297F23e5391703Ea66aB"));

    dbg!(
        fetch_account(
            provider.erased(),
            vaults,
            address!("0xA60c4257c809353039A71527dfe701B577e34bc7"),
            address!("0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383"),
            address!("0x5DaC9ccC215b9aF65b486066786F79d9aA0043DB"),
        )
        .await
    );
}
