use crate::{
    liquidation::get_shares_to_underlying,
    types::{EVault, Erc4626Vault, Ltv, Vault},
};
use alloy::{
    primitives::{Address, U256},
    providers::DynProvider,
    sol,
};
use anyhow::{Context, Result};
use dashmap::DashMap;
use futures::{StreamExt, stream};
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, info, warn};

/// How many vaults we refresh concurrently during a [`Vaults::refresh_all_ratios`] pass.
const REFRESH_CONCURRENCY: usize = 16;

#[derive(Clone)]
pub struct Vaults {
    vault_lens: Address,
    utils_lens: Address,
    vaults: Arc<DashMap<Address, Vault>>,

    /// The most recently observed `shares_to_underlying` ratio for each vault. Kept
    /// fresh in the background by [`Vaults::refresh_all_ratios`] (run periodically by
    /// [`poll_vault_shares`]). This is the cache [`crate::account::Account::calculate_health`]
    /// reads from: a plain in-memory lookup, never a chain call.
    ratios: Arc<DashMap<Address, U256>>,

    /// The most recently observed LTVs for each EVault (controller), keyed by the
    /// controller address and then the collateral vault address.
    ltvs: Arc<DashMap<Address, Arc<HashMap<Address, Ltv>>>>,
}

sol! {
    #[sol(rpc)]
    contract VaultLens {
        function getVaultInfoStatic(address vault) public view returns (VaultInfoStatic memory);
        function getRecognizedCollateralsLTVInfo(address vault) public view returns (LTVInfo[] memory);
    }

    #[sol(rpc)]
    contract UtilsLens {
        function getVaultInfoERC4626(address vault) public view returns (VaultInfoERC4626 memory);
    }

    struct VaultInfoERC4626 {
        uint256 timestamp;
        address vault;
        string vaultName;
        string vaultSymbol;
        uint256 vaultDecimals;
        address asset;
        string assetName;
        string assetSymbol;
        uint256 assetDecimals;
        uint256 totalShares;
        uint256 totalAssets;
        bool isEVault;
    }

    struct VaultInfoStatic {
        uint256 timestamp;
        address vault;
        string vaultName;
        string vaultSymbol;
        uint256 vaultDecimals;
        address asset;
        string assetName;
        string assetSymbol;
        uint256 assetDecimals;
        address unitOfAccount;
        string unitOfAccountName;
        string unitOfAccountSymbol;
        uint256 unitOfAccountDecimals;
        address dToken;
        address oracle;
        address evc;
        address protocolConfig;
        address balanceTracker;
        address permit2;
        address creator;
    }

    struct LTVInfo {
        address collateral;
        uint256 borrowLTV;
        uint256 liquidationLTV;
        uint256 initialLiquidationLTV;
        uint256 targetTimestamp;
        uint256 rampDuration;
    }
}

impl Vaults {
    pub fn new(vault_lens: Address, utils_lens: Address) -> Vaults {
        Vaults {
            vault_lens,
            utils_lens,
            vaults: Arc::new(DashMap::new()),
            ratios: Arc::new(DashMap::new()),
            ltvs: Arc::new(DashMap::new()),
        }
    }

    pub async fn get_or_fetch(&self, provider: &DynProvider, address: Address) -> Result<Vault> {
        // Check if we already have it stored.
        if let Some(vault) = self.vaults.get(&address) {
            return Ok(vault.clone());
        }

        // Attempt to fetch it as an EVault first. If the lens calls fail we assume the
        // address is not an EVault but a plain ERC4626 vault.
        let vault = match self.fetch_evault(provider, address).await {
            Ok(evault) => Vault::EVault(Arc::from(evault)),
            Err(err) => {
                debug!(
                    vault =? address,
                    err =? err,
                    "Fetching the vault info from the VaultLens failed, assuming this is a plain ERC4626 vault"
                );

                Vault::Erc4626(Arc::from(self.fetch_erc4626(provider, address).await?))
            }
        };

        self.vaults.insert(address, vault.clone());

        Ok(vault)
    }

    /// Fetches the full EVault details through the VaultLens. Fails if the address is
    /// not an EVault.
    async fn fetch_evault(&self, provider: &DynProvider, address: Address) -> Result<EVault> {
        let lens = VaultLens::new(self.vault_lens, provider);
        // TODO: Combine the below 2 calls into a single one, or perform them at the same
        // time.
        let info = lens
            .getVaultInfoStatic(address)
            .call()
            .await
            .with_context(|| {
                format!(
                    "Error while calling the VaultLens for static information on vault {} using lens {}",
                    address, self.vault_lens
                )
            })?;

        let ltvs = self.fetch_ltvs(provider, address).await?;
        self.ltvs.insert(address, Arc::new(ltvs.clone()));

        let shares_to_underlying_ratio = get_shares_to_underlying(provider, address).await?;
        self.store_ratio(address, shares_to_underlying_ratio);

        Ok(EVault {
            erc4626: Erc4626Vault {
                address,
                asset: info.asset,
                shares_to_underlying_ratio,
            },
            unit_of_account: info.unitOfAccount,
            borrow_interest_rate: (),
            supply_interest_rate: (),
            adapter: info.oracle,
            ltvs,
        })
    }

    /// Fetches the LTVs of all collaterals recognized by an EVault, keyed by the collateral
    /// vault address.
    async fn fetch_ltvs(
        &self,
        provider: &DynProvider,
        address: Address,
    ) -> Result<HashMap<Address, Ltv>> {
        let lens = VaultLens::new(self.vault_lens, provider);
        let ltv_info = lens
            .getRecognizedCollateralsLTVInfo(address)
            .call()
            .await
            .with_context(|| {
                format!(
                    "Error while calling the VaultLens for vault {} using lens {}",
                    address, self.vault_lens
                )
            })?;

        Ok(ltv_info
            .iter()
            .map(|ltv| {
                (
                    ltv.collateral,
                    Ltv::new(
                        ltv.collateral,
                        ltv.borrowLTV,
                        ltv.liquidationLTV,
                        ltv.initialLiquidationLTV,
                        ltv.targetTimestamp,
                        ltv.rampDuration,
                    ),
                )
            })
            .collect())
    }

    /// Fetches the details of a plain ERC4626 vault.
    async fn fetch_erc4626(
        &self,
        provider: &DynProvider,
        address: Address,
    ) -> Result<Erc4626Vault> {
        let lens = UtilsLens::new(self.utils_lens, provider);
        let info = lens.getVaultInfoERC4626(address)
            .call()
            .await
            .with_context(|| {
                format!(
                    "Error while calling the UtilsLens for ERC4626 information on vault {} using lens {}",
                    address, self.utils_lens
                )
            })?;

        let shares_to_underlying_ratio = get_shares_to_underlying(provider, address).await?;
        self.store_ratio(address, shares_to_underlying_ratio);

        Ok(Erc4626Vault {
            address,
            asset: info.asset,
            shares_to_underlying_ratio,
        })
    }

    /// Records a freshly observed ratio for a vault.
    fn store_ratio(&self, address: Address, ratio: U256) {
        self.ratios.insert(address, ratio);
    }

    /// Returns the most recently cached `shares_to_underlying` ratio for a vault, if we
    /// have one. This is a plain in-memory lookup: it never blocks and never performs
    /// any chain I/O. The cache is kept fresh by [`Vaults::refresh_all_ratios`] running
    /// on its own schedule, see [`poll_vault_shares`].
    pub fn cached_shares_to_underlying_ratio(&self, address: Address) -> Option<U256> {
        self.ratios.get(&address).map(|entry| *entry)
    }

    /// Returns the most recently cached LTVs of a controller (EVault), if we have them. This
    /// is a plain in-memory lookup, the cache is kept fresh by [`Vaults::refresh_all_ltvs`]
    /// running on its own schedule, see [`poll_vault_ltvs`].
    pub fn cached_ltvs(&self, controller: Address) -> Option<Arc<HashMap<Address, Ltv>>> {
        self.ltvs.get(&controller).map(|entry| entry.clone())
    }

    /// Re-fetches the LTVs for every known EVault and updates both the LTV cache and the
    /// stored vault, so accounts fetched afterwards also get a vault with the current LTVs. A
    /// failure for one vault is logged and does not affect the others: the previously cached
    /// LTVs for that vault are simply left in place until the next successful refresh.
    pub async fn refresh_all_ltvs(&self, provider: &DynProvider) {
        // Only EVaults have LTVs.
        let evaults: Vec<Arc<EVault>> = self
            .vaults
            .iter()
            .filter_map(|entry| entry.value().as_evault().cloned())
            .collect();

        stream::iter(evaults)
            .map(|evault| async move {
                let address = evault.address;
                let ltvs = match self.fetch_ltvs(provider, address).await {
                    Ok(ltvs) => ltvs,
                    Err(err) => {
                        warn!(
                            vault =? address,
                            err =? err,
                            "Could not refresh the LTVs for vault, keeping the previously cached value"
                        );
                        return;
                    }
                };

                if ltvs != evault.ltvs {
                    info!(vault =? address, "The LTVs of vault have changed");
                }

                self.ltvs.insert(address, Arc::new(ltvs.clone()));
                self.vaults.insert(
                    address,
                    Vault::EVault(Arc::new(EVault {
                        ltvs,
                        ..(*evault).clone()
                    })),
                );
            })
            .buffer_unordered(REFRESH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
    }

    /// All vault addresses we currently know about, i.e. have been looked up via
    /// [`Vaults::get_or_fetch`] at least once.
    pub fn known_vault_addresses(&self) -> Vec<Address> {
        self.vaults.iter().map(|entry| *entry.key()).collect()
    }

    /// Re-fetches the `shares_to_underlying` ratio for every known vault and updates the
    /// cache. A failure for one vault is logged and does not affect the others: the
    /// previously cached value for that vault is simply left in place until the next
    /// successful refresh.
    pub async fn refresh_all_ratios(&self, provider: &DynProvider) {
        let addresses = self.known_vault_addresses();

        stream::iter(addresses)
            .map(|address| async move {
                match get_shares_to_underlying(provider, address).await {
                    Ok(ratio) => self.store_ratio(address, ratio),
                    Err(err) => {
                        warn!(
                            vault =? address,
                            err =? err,
                            "Could not refresh the shares_to_underlying ratio for vault, keeping the previously cached value"
                        );
                    }
                }
            })
            .buffer_unordered(REFRESH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
    }

    /// Test-only helper to seed the ratio cache without hitting the chain.
    #[cfg(test)]
    pub(crate) fn insert_ratio_for_test(&self, address: Address, ratio: U256) {
        self.store_ratio(address, ratio);
    }

    /// Test-only helper to seed the LTV cache without hitting the chain.
    #[cfg(test)]
    pub(crate) fn insert_ltvs_for_test(&self, controller: Address, ltvs: HashMap<Address, Ltv>) {
        self.ltvs.insert(controller, Arc::new(ltvs));
    }
}

/// Periodically refreshes the `shares_to_underlying` ratio for every vault the bot
/// currently knows about. Besides a vault's very first fetch, this is the only place
/// that makes a chain call for this value — everything else (in particular
/// `Account::calculate_health`) only ever reads whatever is currently cached.
pub async fn poll_vault_shares(
    provider: DynProvider,
    vaults: Vaults,
    interval: tokio::time::Duration,
) -> Result<()> {
    loop {
        tokio::time::sleep(interval).await;

        let known = vaults.known_vault_addresses();
        if known.is_empty() {
            continue;
        }

        info!(
            "Refreshing the shares_to_underlying ratio for {} known vaults",
            known.len()
        );

        vaults.refresh_all_ratios(&provider).await;
    }
}

/// Periodically refreshes the LTVs of every EVault the bot currently knows about, so that
/// governance changes (lowered or raised LTVs, newly added collaterals) are picked up without
/// a restart.
pub async fn poll_vault_ltvs(
    provider: DynProvider,
    vaults: Vaults,
    interval: tokio::time::Duration,
) -> Result<()> {
    loop {
        tokio::time::sleep(interval).await;

        let known = vaults.known_vault_addresses();
        if known.is_empty() {
            continue;
        }

        info!("Refreshing the LTVs for {} known vaults", known.len());

        vaults.refresh_all_ltvs(&provider).await;
    }
}

#[cfg(test)]
mod test {
    use alloy::{
        node_bindings::Anvil,
        primitives::{Address, U256, address},
        providers::{Provider, ProviderBuilder, ext::AnvilApi},
        rpc::types::TransactionRequest,
        sol,
        sol_types::SolCall,
    };

    use super::Vaults;
    use crate::{config::load_configuration_file_for_test, test_utils::ensure_contracts_on_fork};

    sol! {
        function setLTV(address collateral, uint16 borrowLTV, uint16 liquidationLTV, uint32 rampDuration);
    }

    #[tokio::test]
    async fn refresh_all_ltvs_picks_up_an_on_chain_ltv_change() {
        let block = 25644480;
        // A mainnet EVault, one of its recognized collaterals, and its governor at `block`.
        let vault = address!("0xba98fc35c9dfd69178ad5dce9fa29c64554783b5");
        let collateral = address!("0xAB2726DAf820Aa9270D14Db9B18c8d187cbF2f30");
        let governor = address!("0x9453ee262d7C95955e690AE7aBBD82a08B135685");

        let mainnet_rpc = std::env::var("MAINNET_RPC").expect("MAINNET_RPC must be set");
        let config = load_configuration_file_for_test(&mainnet_rpc, 1).unwrap();

        let network = Anvil::new()
            .fork(mainnet_rpc)
            .fork_block_number(block)
            .try_spawn()
            .unwrap();

        let provider = ProviderBuilder::new()
            .connect_http(network.endpoint_url())
            .erased();

        ensure_contracts_on_fork(
            &provider,
            &config.rpc_url,
            &[config.vault_lens_address, config.utils_lens_address],
        )
        .await
        .unwrap();

        let vaults = Vaults::new(config.vault_lens_address, config.utils_lens_address);

        // First load, this is what gets cached.
        let before = vaults.get_or_fetch(&provider, vault).await.unwrap();
        let before_ltv = before.as_evault().unwrap().ltvs[&collateral].current_liquidation_ltv();

        // The governor lowers the liquidation LTV of the collateral, without a ramp.
        let new_liquidation_ltv: u16 = 1000;
        assert_ne!(before_ltv, U256::from(new_liquidation_ltv));

        provider.anvil_impersonate_account(governor).await.unwrap();
        provider
            .anvil_set_balance(governor, U256::from(10).pow(U256::from(18)))
            .await
            .unwrap();
        let receipt = provider
            .send_transaction(
                TransactionRequest::default()
                    .from(governor)
                    .to(vault)
                    .input(
                        setLTVCall {
                            collateral,
                            borrowLTV: new_liquidation_ltv / 2,
                            liquidationLTV: new_liquidation_ltv,
                            rampDuration: 0,
                        }
                        .abi_encode()
                        .into(),
                    ),
            )
            .await
            .unwrap()
            .get_receipt()
            .await
            .unwrap();
        assert!(receipt.status(), "setLTV reverted");

        vaults.refresh_all_ltvs(&provider).await;

        // The vault handed out to newly fetched accounts reflects the change...
        let after = vaults.get_or_fetch(&provider, vault).await.unwrap();
        assert_eq!(
            after.as_evault().unwrap().ltvs[&collateral].current_liquidation_ltv(),
            U256::from(new_liquidation_ltv)
        );

        // ...and so does the live LTV cache, which `calculate_health` reads.
        let cached = vaults
            .cached_ltvs(vault)
            .expect("expected cached LTVs for the vault");
        assert_eq!(
            cached[&collateral].current_liquidation_ltv(),
            U256::from(new_liquidation_ltv)
        );
    }

    #[test]
    fn cached_ratio_reflects_the_last_inserted_value() {
        let vaults = Vaults::new(Address::random(), Address::random());
        let vault = Address::random();

        // Nothing cached yet.
        assert_eq!(vaults.cached_shares_to_underlying_ratio(vault), None);

        vaults.insert_ratio_for_test(vault, U256::from(1_000_000));
        assert_eq!(
            vaults.cached_shares_to_underlying_ratio(vault),
            Some(U256::from(1_000_000))
        );

        // A later refresh overwrites the previous value: this is exactly what
        // `calculate_health` sees once the background poller runs.
        vaults.insert_ratio_for_test(vault, U256::from(2_000_000));
        assert_eq!(
            vaults.cached_shares_to_underlying_ratio(vault),
            Some(U256::from(2_000_000))
        );

        // Unrelated vaults are unaffected.
        assert_eq!(
            vaults.cached_shares_to_underlying_ratio(Address::random()),
            None
        );
    }

    #[test]
    fn known_vault_addresses_only_includes_vaults_looked_up_through_get_or_fetch() {
        let vaults = Vaults::new(Address::random(), Address::random());
        assert!(vaults.known_vault_addresses().is_empty());

        // Seeding only the ratio cache (which a real refresh pass would never do, since
        // it only ever iterates `known_vault_addresses`) must not make the address
        // "known" — otherwise a stray ratio would cause `refresh_all_ratios` to keep
        // refreshing an address nothing actually references.
        vaults.insert_ratio_for_test(Address::random(), U256::from(1));
        assert!(vaults.known_vault_addresses().is_empty());
    }
}
