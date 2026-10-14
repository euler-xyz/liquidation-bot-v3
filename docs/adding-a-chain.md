# Adding a chain

This is the runbook for running the bot on a new chain. Each chain runs its own bot process, with its own Liquidator contract and its own `configs/Config.<chainId>.toml`. Keep this file current: a PR that changes any of these steps updates it.

## 0. Prerequisites

The bot depends on these before it can start on a chain:

| Dependency | Why |
|---|---|
| Euler core, periphery and lens contracts, listed in euler-interfaces `addresses/<chainId>/` | Liquidator constructor arguments and config addresses |
| The chain's Euler subgraph | The startup sync discovers borrowers from it (`subgraph_url_path`) |
| The swap API (`swap.euler.finance`) serves the chain | Every liquidation swaps the seized collateral through it. See `docs/adding-a-chain.md` in euler-orderflow-router |
| V3 indexes the chain | Profit is priced through `<pricing_url>/v3/tokens/<chainId>/<asset>/price`. If V3 doesn't support the chain, only liquidations of debt in the wrapped native token keep their profitability check; the others are executed without one |
| The chain supports the Cancun EVM | The Liquidator is compiled for `evm_version = "cancun"` and uses `PUSH0` and `MCOPY`. Without them it can't be deployed or its liquidations revert |
| `RPC_URL_<chainId>` in the bot's Doppler config | Written by the `add-chain` command in euler-infra-utils |

## 1. Deploy the Liquidator

The Liquidator is the contract the bot sends liquidations through. Build it from this repository:

```sh
git submodule update --init
forge build
forge test
```

`contracts/DeployLiquidator.sol` reads its inputs from the environment. The addresses come from euler-interfaces:

| Variable | Source |
|---|---|
| `EVC_ADDRESS` | `addresses/<chainId>/CoreAddresses.json` → `evc` |
| `SWAPPER_ADDRESS` | `addresses/<chainId>/PeripheryAddresses.json` → `swapper` |
| `SWAP_VERIFIER_ADDRESS` | `addresses/<chainId>/PeripheryAddresses.json` → `swapVerifier` |
| `PYTH_ADDRESS` | Pyth's EVM contract address for the chain (not in euler-interfaces), or `0x0000000000000000000000000000000000000000` on a chain without Pyth |
| `LIQUIDATOR_PRIVATE_KEY` | The deployer's key. Its address becomes the contract's `owner()`. It needs only gas for the deployment |

All five are required. The script reverts with `NoCodeAt(<variable>, <address>)` if the EVC, swapper, swap verifier or a non-zero Pyth address has no code on the chain.

```sh
forge script contracts/DeployLiquidator.sol --rpc-url "$RPC_URL" --broadcast
```

The swapper and EVC are immutable in the contract. The bot refuses to start if `swapperAddress()` and `evcAddress()` differ from `swapper_address` and `evc_address` in its config. A new swapper deployment therefore needs a new Liquidator.

### Chains without Pyth

Deploy with `PYTH_ADDRESS` set to the zero address and leave the `[pyth]` table out of the config. TAC and BOB run this way.

- The constructor accepts the zero address. Only `liquidateSingleCollateralWithPythOracle`, `simulatePythUpdateAndGetAccountStatus` and `simulatePythUpdateAndCheckLiquidation` use `PYTH`. Without `[pyth]` the bot never calls them; it liquidates through `liquidateSingleCollateral`.
- At startup the bot requires `PYTH()` to be the zero address when `[pyth]` is absent, and to equal `[pyth].address` when it is present.
- An account whose oracle needs a Pyth update is not liquidated on such a chain; the bot reports an oracle error for it.
- `PYTH` is immutable. Adding Pyth to the chain later means a new Liquidator and a new `liquidator_address`.

### Explorer verification

`foundry.toml` pins the compiler settings (solc 0.8.33, optimizer off) that every deployed Liquidator was built with, so verification needs no extra compiler flags. Verify in the same run by adding `--verify` and the explorer's flags to the `forge script` command, or afterwards:

```sh
forge verify-contract "$LIQUIDATOR" contracts/Liquidator.sol:Liquidator \
  --chain "$CHAIN_ID" \
  --constructor-args "$(cast abi-encode 'constructor(address,address,address,address,address)' \
      "$OWNER" "$SWAPPER_ADDRESS" "$SWAP_VERIFIER_ADDRESS" "$EVC_ADDRESS" "$PYTH_ADDRESS")" \
  --watch
```

The constructor order is owner, swapper, swap verifier, EVC, Pyth. Pick the verifier for the chain's explorer:

- Etherscan family: `--verifier etherscan` with `ETHERSCAN_API_KEY` set.
- Blockscout: `--verifier blockscout --verifier-url <explorer>/api/`.
- Anything else Etherscan-compatible: `--verifier custom --verifier-url <api> --verifier-api-key <key>`.

## 2. Add `configs/Config.<chainId>.toml`

Copy a config of a chain with the same Pyth situation and replace the chain-specific values. Configuration is merged in this order, later layers winning:

1. `RPC_URL_<CHAIN_ID>` as `rpc_url`.
2. `Config.<CHAIN_ID>.toml`, searched for in the working directory and then its parents.
3. Every environment variable, lowercased, as the field of the same name (`EOA_ADDRESS` → `eoa_address`).

Every field the bot parses (`Config` in `src/config.rs`):

| Field | Type | Required | Value |
|---|---|---|---|
| `chain_id` | integer | yes | From `CHAIN_ID`; the shipped files leave it out. Must match the RPC's chain id |
| `rpc_url` | URL | yes | From `RPC_URL_<chainId>`; not in the file |
| `transaction_rpc_url` | URL | no | A separate RPC used only to send liquidations, e.g. an MEV-protected one. Must report the same chain id. Only Ethereum sets it |
| `subgraph_url_prefix` | string | yes | From `SUBGRAPH_URL_PREFIX`; not in the file. Must end with `/` |
| `subgraph_url_path` | string | yes | The chain's subgraph path, `subgraphs/euler-simple-<chain>/<version>/gn` |
| `swap_url` | URL | yes | `https://swap.euler.finance` |
| `pricing_url` | URL | yes | `https://v3.euler.finance` |
| `evc_address` | address | yes | `CoreAddresses.json` → `evc`; must equal the Liquidator's `evcAddress()` |
| `swapper_address` | address | yes | `PeripheryAddresses.json` → `swapper`; must equal the Liquidator's `swapperAddress()` |
| `wrapped_native_asset_address` | address | yes | The chain's canonical wrapped native token (WETH, WAVAX, ...); not in euler-interfaces |
| `oracle_lens_address` | address | yes | `LensAddresses.json` → `oracleLens` |
| `account_lens_address` | address | yes | `LensAddresses.json` → `accountLens` |
| `vault_lens_address` | address | yes | `LensAddresses.json` → `vaultLens` |
| `utils_lens_address` | address | yes | `LensAddresses.json` → `utilsLens` |
| `liquidator_address` | address | yes | The Liquidator from step 1 |
| `profit_receiver` | address | yes | Receives the profit of every liquidation; the shipped configs share one receiver |
| `eoa_address` | address | yes | From `EOA_ADDRESS`; not in the file |
| `eoa_private_key` | string | yes | From `EOA_PRIVATE_KEY`; not in the file |
| `oracle_polling_interval_seconds` | integer | yes | `30` in the shipped configs |
| `full_resync_and_check_interval_seconds` | integer | yes | `3600` in the shipped configs. `/health` returns 503 once the main loop has been silent for twice this long |
| `vault_shares_polling_interval_seconds` | integer | no, `60` | Refresh of each vault's shares-to-assets ratio |
| `vault_ltv_polling_interval_seconds` | integer | no, `3600` | Refresh of each vault's LTVs |
| `oracle_type_refresh_interval_seconds` | integer | no, `3600` | Re-detection of which oracles need Pyth |
| `simulation_mode` | bool | no, `false` | Settle liquidations on a local Anvil fork instead of the chain. Needs `anvil` on `PATH`; the Docker image has none |
| `enable_observability_api` | bool | no, `false` | Must be `true` in production: the load balancer health check calls `/health` on port 3000 |
| `[pyth]` `address` | address | with `[pyth]` | Pyth contract; must equal the Liquidator's `PYTH()` |
| `[pyth]` `endpoint` | string | with `[pyth]` | Hermes base URL with a trailing `/`, `https://hermes.pyth.network/` |
| `[vault_filter]` `mode` | string | with `[vault_filter]` | `"None"`, `"Whitelist"` or `"Blacklist"`. Without the table, no filter applies |
| `[vault_filter]` `items` | list | with `[vault_filter]` | Vault addresses, bare or as `{ address = "0x…", reason = "…" }`. Every blacklist entry should carry a reason (see [Blacklist policy](../README.md#blacklist-policy)) |

The Pyth API key is not a config field: `PYTH_API_KEY` fills it when `[pyth]` is present.

A file in `configs/` that no bot runs carries `deployed = false`. The bot ignores the key, and the config tests skip the file.

At startup the bot checks the RPC's chain id, that every configured contract address has code, and that the Liquidator's swapper, EVC and Pyth match the config. It exits if any check fails.

In the same PR, add the chain to [Supported chains](../README.md#supported-chains) in the README. The tests pick up the new file from `configs/` with no test change: `cargo test` checks that it loads, and with `RPC_URL_<chainId>` set, `cargo test validate_configuration_files` runs the startup checks against it.

Optionally, run the bot locally against the chain without sending transactions. Set `CHAIN_ID`, `RPC_URL_<chainId>`, `SUBGRAPH_URL_PREFIX`, a throwaway `EOA_ADDRESS` and `EOA_PRIVATE_KEY` pair, and `SIMULATION_MODE=true`. Start the binary from `configs/` and watch `/health` and `/accounts` on port 3000.

## 3. Wallet and secrets

Fund the signing wallet, `EOA_ADDRESS`, with the chain's native token. It pays gas for every liquidation, plus the Pyth update fee on liquidations that need a Pyth update. Liquidations need no other capital, since the seized collateral is swapped to repay the debt in the same transaction.

The task's environment, by name:

| Name | Source | Needed |
|---|---|---|
| `CHAIN_ID` | Set per service by terraform | yes |
| `RPC_URL_<chainId>` | Doppler | yes |
| `SUBGRAPH_URL_PREFIX` | Doppler | yes |
| `EOA_ADDRESS` | Doppler | yes |
| `EOA_PRIVATE_KEY` | AWS SSM, injected by the task definition rather than Doppler | yes |
| `PYTH_API_KEY` | Doppler | on chains with `[pyth]` |
| `SWAP_API_HEADER_SECRET` | Doppler. Sent as `x-api-key` to the swap API so its rate limit doesn't apply | recommended |
| `BETTER_STACK_SOURCE_TOKEN`, `BETTER_STACK_INGESTING_HOST` | Doppler; read by the log router container, not the bot | for log shipping |

Apart from `RPC_URL_<chainId>`, a new chain needs no new secret.

## 4. Build and deploy

On every push to `master`, `.github/workflows/build-image.yml` builds the Docker image and pushes it to ECR as `<ECR_REPOSITORY>:<commit SHA>`. It never runs on pull requests or in forks. The image contains every `configs/Config.<chainId>.toml`, so a new chain needs an image built from a commit that has its config.

The deployment is the euler-terraform stack `app/stacks/liquidation_bot_fargate_v3`:

1. Add the chain id to `chain_ids` in `variables.tf`.
2. Add it to `chain_priority_map` in `main.tf` with an unused priority. The map gives each chain a fixed listener rule priority, so adding a chain never shifts the others. Priorities must be unique on the load balancer listener, so pick one no other rule uses. A chain missing from the map fails the plan.
3. Apply with `docker_image_link` set to `<registry>/<ECR_REPOSITORY>:<commit SHA>`. Terraform reads Doppler at apply time; a new Doppler value reaches the tasks only at the next apply.

Per chain this creates a target group `liquidation-bot-v3-<chainId>`, a listener rule for the host `<chainId>-liquidations.euler.finance`, and the ECS service `liquidation-bot-v3-<chainId>-<environment>`.

### DNS

`<chainId>-liquidations.euler.finance` is a proxied record on Cloudflare that points at the production load balancer. Until a pending change moves these records into terraform, create it by hand next to the existing chains' records.

### Check the deployment

- `https://<chainId>-liquidations.euler.finance/health` goes from `Syncing` to `Healthy`.
- `/accounts` lists accounts, and `/vault-filter` returns the configured filter.

## 5. Risk dashboard

euler-risk-dashboard lists the bots it monitors in `NEXT_PUBLIC_BOT_INSTANCES`, a JSON array baked in at build time:

1. Add the chain to `DEFAULT_BOTS` in `src/config/defaults.ts`: `id`, `chainId`, `chainName`, `chainShortName` and `baseUrl` (`https://<chainId>-liquidations.euler.finance`). Add `explorerUrl` if `src/lib/explorer.ts` has no explorer for the chain.
2. `npm run config:production` prints the new array. Store it as `NEXT_PUBLIC_BOT_INSTANCES` in the dashboard's Doppler config, then rebuild the dashboard.

## Order for a new chain

1. The prerequisites in step 0 are in place.
2. The Liquidator is deployed and verified.
3. The config PR is merged and its image is built.
4. The wallet is funded.
5. Terraform is applied with that image and the DNS record exists.
6. `/health` reports `Healthy`.
7. The dashboard lists the chain.

## One-time setup for the image workflow

The workflow signs in to AWS with GitHub OIDC; there are no stored keys. A repository admin sets up:

1. **OIDC provider.** The AWS account trusts `token.actions.githubusercontent.com` with audience `sts.amazonaws.com`. Skip this if the account already has it.
2. **IAM role** for the workflow, with this trust policy:

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [
       {
         "Effect": "Allow",
         "Principal": {
           "Federated": "arn:aws:iam::<account-id>:oidc-provider/token.actions.githubusercontent.com"
         },
         "Action": "sts:AssumeRoleWithWebIdentity",
         "Condition": {
           "StringEquals": {
             "token.actions.githubusercontent.com:aud": "sts.amazonaws.com",
             "token.actions.githubusercontent.com:sub": "repo:euler-xyz/liquidation-bot-v3:ref:refs/heads/master"
           }
         }
       }
     ]
   }
   ```

   The `sub` condition limits the role to pushes to `master` in this repository. If the organization customizes its OIDC subject claim, match that format instead. The role's permissions:
   - `ecr:GetAuthorizationToken` on `*`
   - on the bot's repository: `ecr:BatchCheckLayerAvailability`, `ecr:BatchGetImage`, `ecr:GetDownloadUrlForLayer`, `ecr:InitiateLayerUpload`, `ecr:UploadLayerPart`, `ecr:CompleteLayerUpload` and `ecr:PutImage`
3. **ECR repository.** Use the one the `liquidation_bot_fargate_v3` stack deploys from, in the role's account. If it holds only this workflow's images, make its tags immutable: each tag is a commit SHA and never changes. A re-run for a commit whose image is already pushed then fails at the push step.
4. **Repository secret** `AWS_ROLE_ARN`, the ARN of the role from step 2, under Settings → Secrets and variables → Actions → Secrets. It's a secret, not a variable, so the account ID in it stays masked in this public repository's Actions logs.
5. **Repository variables**, under Settings → Secrets and variables → Actions → Variables:

   | Variable | Value |
   |---|---|
   | `AWS_REGION` | region of the ECR registry |
   | `ECR_REPOSITORY` | name of the repository from step 3 |

Until all three variables are set, the workflow fails at its first step and names the missing ones. The Rust build is cached in the GitHub Actions cache, which needs no setup.
