# liquidation-bot-v3

A liquidation bot for the Euler V3 lending protocol. It tracks borrowers across Euler vaults, monitors their health, and submits liquidation transactions on-chain when positions become eligible.

## What it does

The bot runs as a long-lived process against a single chain. On startup it does a one-shot historical sync from the Euler subgraph to discover every active borrower, then keeps state fresh in three concurrent ways:

- It watches the EVC contract for on-chain events so any new or changed account is picked up in real time.
- It polls the configured oracles (including Pyth) on a fixed interval. When a price moves, every account that depends on the affected oracle is re-evaluated.
- It performs a full resync and health check on every account at a longer interval as a safety net.

When an account becomes unhealthy, the bot picks the most profitable borrow/collateral pair, gets a swap quote from the Euler swap API for the seized collateral, simulates the liquidation, and if the result is profitable submits it through the configured liquidator contract. Profits are routed to the configured profit receiver.

There is an optional HTTP API for observability that exposes the bot's view of accounts, oracles, and overall health.

## How it is configured

Configuration uses three layers, merged in this order (later layers override earlier ones):

1. An `RPC_URL_<chain_id>` environment variable, which seeds `rpc_url` for the matching chain.
2. A TOML file named `Config.<chain_id>.toml` loaded from the current working directory.
3. Any other environment variables, mapped onto config fields by name (e.g. `EOA_PRIVATE_KEY`, `SUBGRAPH_URL_PREFIX`).

The chain the bot runs against is selected with the `CHAIN_ID` environment variable. This determines which config file is loaded and which `RPC_URL_<chain_id>` value is used.

Ready-made config files for every supported chain are checked into the `configs/` directory. They contain the chain-specific contract addresses and subgraph paths but deliberately do not include secrets, RPC endpoints, or the subgraph host. Those must be supplied through environment variables.

### Required environment variables

| Variable | Purpose |
| --- | --- |
| `CHAIN_ID` | Selects which `Config.<chain_id>.toml` is loaded. |
| `RPC_URL_<chain_id>` | RPC endpoint for the chain. The chain id in the variable name must match `CHAIN_ID`. |
| `SUBGRAPH_URL_PREFIX` | Base URL of the subgraph host. It is joined with `subgraph_url_path` from the TOML file to form the full subgraph endpoint. |
| `EOA_ADDRESS` | Public address of the wallet that will sign liquidation transactions. Validated against `EOA_PRIVATE_KEY` on startup. |
| `EOA_PRIVATE_KEY` | Private key for the signing wallet. |

### Optional environment variables

| Variable | Purpose |
| --- | --- |
| `PYTH_API_KEY` | Pyth Hermes API key, sent with every price update request. Used only on chains whose config has a `[pyth]` table. |
| `SWAP_API_HEADER_SECRET` | Sent as the `x-api-key` header to the swap API, which lifts its rate limit. Without it swap quotes can be rate limited. |

### Configuration file fields

The TOML files in `configs/` define everything that is chain-specific. The following fields are recognised:

- `chain_id` (u64): The chain id this config targets. Provided by `CHAIN_ID`; the shipped files leave it out. Cross-checked against the RPC's reported chain id at startup.
- `rpc_url` (URL): Chain RPC endpoint. Usually provided via `RPC_URL_<chain_id>` rather than the file.
- `transaction_rpc_url` (URL, optional): A separate RPC used only to send liquidation transactions, for example an MEV-protected endpoint. It must report the same chain id. Without it, transactions go through `rpc_url`.
- `subgraph_url_prefix` (string): Subgraph host. Usually provided via `SUBGRAPH_URL_PREFIX`. It must end with `/`, since `subgraph_url_path` is joined onto it.
- `subgraph_url_path` (string): Path component of the subgraph URL. Set per chain in the TOML file.
- `swap_url` (URL): Euler swap API used to build collateral-to-debt swap quotes.
- `pricing_url` (URL): Euler pricing API used when evaluating liquidation profitability. On a chain it does not support, only liquidations of debt in the wrapped native token keep their profitability check; the others are executed without one.
- `evc_address` (address): The Ethereum Vault Connector contract for the chain.
- `pyth` (table, optional): The chain's Pyth deployment. Omit the whole table on chains without one.
  - `address`: Pyth contract address.
  - `endpoint`: Pyth Hermes base URL, with a trailing `/` (e.g. `https://hermes.pyth.network/`).
- `swapper_address` (address): Swapper contract used during liquidation execution.
- `wrapped_native_asset_address` (address): Wrapped native token (WETH, WAVAX, etc).
- `oracle_lens_address` (address): Oracle lens contract.
- `account_lens_address` (address): Account lens contract.
- `vault_lens_address` (address): Vault lens contract.
- `utils_lens_address` (address): Utils lens contract.
- `liquidator_address` (address): The liquidator contract the bot submits liquidations through. Its swapper, EVC and Pyth addresses must match `swapper_address`, `evc_address` and `pyth.address` (zero when there is no `[pyth]` table).
- `eoa_address` (address): Public address of the signing wallet.
- `eoa_private_key` (string): Private key of the signing wallet.
- `profit_receiver` (address): Address that receives profit from each liquidation.
- `oracle_polling_interval_seconds` (u64): How often the oracle poller runs. Lower values catch price moves faster at the cost of more RPC traffic.
- `full_resync_and_check_interval_seconds` (u64): How often the bot does a full account refresh and health check across every tracked account.
- `vault_shares_polling_interval_seconds` (u64, default `60`): How often each vault's shares-to-assets ratio is refreshed.
- `vault_ltv_polling_interval_seconds` (u64, default `3600`): How often each vault's LTVs are refreshed, so governance changes are picked up.
- `oracle_type_refresh_interval_seconds` (u64, default `3600`): How often the bot re-detects which oracles depend on Pyth, so oracle router changes are picked up.
- `simulation_mode` (bool, default `false`): When `true` the bot spins up a local Anvil fork of the chain and settles every liquidation against the fork instead of the real network. Useful for dry-running against live data without sending real transactions. Requires `anvil` on `PATH`.
- `enable_observability_api` (bool, default `false`): When `true` the bot serves the observability HTTP API on port 3000.
- `vault_filter` (table, optional): Restricts which vaults the bot will liquidate against. It applies to the vault an account borrows from.
  - `mode`: One of `"None"` (no filter, also the behaviour without the table), `"Whitelist"` (accounts borrowing from any vault outside `items` are not tracked at all), or `"Blacklist"` (accounts borrowing from a vault in `items` are still tracked and shown by the API, but never liquidated).
  - `items`: The vaults the filter applies to, each either a bare address or a table with a reason: `{ address = "0x…", reason = "…" }`.

On startup the bot validates the configuration before starting work. It checks that the RPC reports the expected chain id, that the signing key matches the configured EOA address, that every configured contract address actually has bytecode deployed at it, and that the liquidator contract is wired to the configured swapper, EVC and Pyth. If any of these fail the bot exits.

### Blacklist policy

A vault is blacklisted when a liquidator could not realise meaningful value from its positions, judged by what the collateral can actually be sold or redeemed for rather than by quoted or oracle prices. Examples are collateral or debt in an asset that collapsed, collateral that is shares of vaults left insolvent, collateral priced by an oracle its creator controls, and liquidation gated by credentials the bot does not hold. Vaults whose collateral can still be sold at size stay monitored, even while a feed is stale or withdrawals are frozen.

Every blacklist entry carries a `reason`. The `/vault-filter` endpoint exposes the filter, reasons included, so the risk dashboard can show why an account is not liquidated.

## Setup

The bot is a Rust binary built with Cargo (edition 2024). You will need Rust 1.91 or newer, the minimum of the locked `alloy` crates; the Docker image builds with 1.95.

Clone the repository and build:

```
cargo build --release
```

To run, set the required environment variables and start the binary from a directory containing the appropriate config file. For example, to run against Ethereum mainnet:

```
export CHAIN_ID=1
export RPC_URL_1="https://your-mainnet-rpc"
export SUBGRAPH_URL_PREFIX="https://api.goldsky.com/api/public/project_cm4iagnemt1wp01xn4gh1agft/"
export EOA_ADDRESS="0xYourWalletAddress"
export EOA_PRIVATE_KEY="0xYourPrivateKey"

cd configs
../target/release/liquidation-bot-v3
```

The bot loads `Config.1.toml` from the current directory based on `CHAIN_ID`, so it must be started from a directory that contains the relevant config file (or the file must be copied/symlinked there).

Logging uses a fixed `tracing-subscriber` filter of `warn,liquidation_bot_v3=info`, which keeps the bot's own logs at info level while silencing chatty dependencies. Changing log verbosity requires a code change.

### Docker

A multi-stage Dockerfile is included. It builds the bot, copies every file from `configs/` into the image, and uses [Doppler](https://www.doppler.com/) as the entrypoint for secret injection. Configure `DOPPLER_PROJECT` and `DOPPLER_CONFIG` (and authenticate Doppler) to supply environment variables to the container at runtime. The image does not include Foundry, so `simulation_mode` does not work in it.

If you do not want to use Doppler, the Dockerfile is straightforward to adapt: replace the `doppler run --` entrypoint with a direct invocation of `liquidation-bot-v3` and pass environment variables in however you prefer. The production deployment does this: its task definition runs the binary directly and passes the environment itself.

## Liquidator contract

The bot submits liquidations through the `Liquidator` contract in `contracts/`, one deployment per chain. The contracts build with [Foundry](https://getfoundry.sh/):

```
git submodule update --init
forge build
forge test
```

`contracts/DeployLiquidator.sol` deploys it, reading the EVC, swapper, swap verifier and Pyth addresses from the environment. On chains without Pyth it is deployed with the zero address as Pyth. The signing wallet (`EOA_ADDRESS`) must hold the chain's native token for gas and Pyth update fees; liquidations need no other capital. Deployment, explorer verification and funding are covered in [docs/adding-a-chain.md](docs/adding-a-chain.md).

## Observability API

When `enable_observability_api = true`, the bot serves the following endpoints on port 3000:

- `GET /health` — Reports the bot's current state: `Syncing`, `Healthy`, or `Error` with a message. It returns 503 when the main loop has not reported for twice `full_resync_and_check_interval_seconds`.
- `GET /accounts` — Lists every account the bot is tracking, with its computed health, dependent oracles, and full collateral/borrow positions.
- `GET /oracles` — Lists every oracle the bot is aware of, including the latest cached value.
- `GET /vault-filter` — The configured `vault_filter`: its mode and items, with their reasons.

CORS is open to any origin. The API is read-only.

## Supported chains

The bot runs on Ethereum (1), BSC (56), Unichain (130), Monad (143), Sonic (146), TAC (239), HyperEVM (999), Base (8453), Plasma (9745), Arbitrum (42161), Avalanche (43114), Linea (59144), BOB (60808) and Berachain (80094). TAC and BOB have no Pyth deployment. `configs/Config.1923.toml` (Swell) is not deployed and does not load, since it has no `utils_lens_address`.

Adding a chain takes a Liquidator deployment, a config file, a funded wallet and deployment changes outside this repository. See [docs/adding-a-chain.md](docs/adding-a-chain.md).

## Tests

```
cargo test
```

The configuration validation test exercises every deployed chain's config file against a public RPC for its chain. Some tests run against live chains and fail when their RPC URL is unset: `MAINNET_RPC`, `BASE_RPC` and `BERACHAIN_RPC`. The Pyth price test uses `PYTH_API_KEY`, and the transaction dispatcher tests need `anvil` on `PATH`.

The contracts are tested with `forge test`.
