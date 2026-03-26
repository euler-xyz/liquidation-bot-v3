use alloy::primitives::{Address, FixedBytes, U256};
use gql_client::{Client, GraphQLError};
use reqwest::Url;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Debug)]
struct MetaBlockResponse {
    #[serde(rename = "_meta")]
    meta: Meta,
}

#[derive(Deserialize, Debug)]
struct Meta {
    block: Block,
}

#[derive(Deserialize, Debug)]
struct Block {
    number: u64,
}

#[derive(Debug)]
pub enum SubGraphError {
    GQLError(GraphQLError),
    JsonDecodeError,
}

pub async fn fetch_latest_indexed_block(url: Url) -> Result<u64, SubGraphError> {
    let query = r#"{
    _meta {
      block {
         number
        }
      }
    }
    "#;

    let client = Client::new(url);
    let result: MetaBlockResponse = client
        .query(query)
        .await
        .map_err(SubGraphError::GQLError)?
        .ok_or(SubGraphError::JsonDecodeError)?;

    Ok(result.meta.block.number)
}

#[derive(Serialize)]
pub struct TrackingVaultBalancesArgs {
    pub id_gt: FixedBytes<40>,
    pub at_block: u64,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TrackingVaultBalancesResponseItem {
    id: FixedBytes<40>,
    account: Address,
    vault: Address,
    debt: U256,
    address_prefix: FixedBytes<19>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct TrackingVaultBalancesResponse {
    tracking_vault_balances: Vec<TrackingVaultBalancesResponseItem>,
}

pub async fn fetch_tracking_vault_balances(
    url: Url,
    args: TrackingVaultBalancesArgs,
) -> Result<Vec<TrackingVaultBalancesResponseItem>, SubGraphError> {
    let query = r#"
        query VaultBalances($id_gt: String!, $at_block: Int!) {
            trackingVaultBalances(
                where: { debt_gt: "0", id_gt: $id_gt }
                first: 1000
                orderBy: id
                orderDirection: asc,
                block: {number: $at_block}
            ) {
                id
                account
                vault
                debt
                addressPrefix
            }
        }
    "#;

    let client = Client::new(url);
    let result: TrackingVaultBalancesResponse = client
        .query_with_vars(query, args)
        .await
        .map_err(SubGraphError::GQLError)?
        .ok_or(SubGraphError::JsonDecodeError)?;

    Ok(result.tracking_vault_balances)
}

#[cfg(test)]
mod test {
    use alloy::primitives::FixedBytes;
    use reqwest::Url;

    use crate::subgraph::{
        TrackingVaultBalancesArgs, fetch_latest_indexed_block, fetch_tracking_vault_balances,
    };

    const ENDPOINT: &str = "https://api.goldsky.com/api/public/project_cm4iagnemt1wp01xn4gh1agft/subgraphs/euler-simple-base/latest/gn";

    #[tokio::test]
    async fn fetch_latest_block() {
        let latest_indexed_block = fetch_latest_indexed_block(Url::parse(ENDPOINT).unwrap())
            .await
            .unwrap();

        assert!(latest_indexed_block > 0);
    }

    #[tokio::test]
    async fn fetch_vault_balances() {
        let args = TrackingVaultBalancesArgs {
            id_gt: FixedBytes::ZERO,
            at_block: 43873296,
        };

        let balances = fetch_tracking_vault_balances(Url::parse(ENDPOINT).unwrap(), args)
            .await
            .unwrap();

        assert!(!balances.is_empty());
    }
}
