// SPDX-License-Identifier: MIT

pragma solidity ^0.8.26;

import {Script, console2} from "forge-std/Script.sol";

import {Liquidator} from "../src/Liquidator.sol";

/// @notice Deploys the Liquidator for the chain the script runs against.
/// @dev The EVC, swapper and Pyth addresses are read from the bot's `configs/Config.<chain id>.toml`, so the
/// deployment always matches the addresses the bot validates the liquidator against on startup. Chains without a
/// `[pyth]` section get a liquidator with Pyth set to the zero address, which is what the bot expects there.
///
/// Optional environment variables:
/// - `LIQUIDATOR_OWNER`: owner of the liquidator, defaults to the broadcasting account.
/// - `SWAP_VERIFIER_ADDRESS`: the chain's EVK swap verifier. It is stored on the liquidator for reference only and
///   is not part of the bot config, so it defaults to the zero address.
///
/// Usage:
///   forge script script/DeployLiquidator.s.sol --rpc-url <rpc> --account <keystore> --broadcast --verify
contract DeployLiquidator is Script {
    struct Deployment {
        address owner;
        address swapper;
        address swapVerifier;
        address evc;
        address pyth;
    }

    function run() public returns (Liquidator liquidator) {
        Deployment memory deployment = loadDeployment(block.chainid);

        console2.log("Chain id:      ", block.chainid);
        console2.log("Owner:         ", deployment.owner);
        console2.log("EVC:           ", deployment.evc);
        console2.log("Swapper:       ", deployment.swapper);
        console2.log("Swap verifier: ", deployment.swapVerifier);
        console2.log("Pyth:          ", deployment.pyth);

        vm.startBroadcast();
        liquidator = new Liquidator(
            deployment.owner, deployment.swapper, deployment.swapVerifier, deployment.evc, deployment.pyth
        );
        vm.stopBroadcast();

        console2.log("Liquidator deployed at:", address(liquidator));
        console2.log(string.concat("Set `liquidator_address` in ", configPath(block.chainid), " to the address above."));
    }

    function loadDeployment(uint256 chainId) public view returns (Deployment memory deployment) {
        string memory path = configPath(chainId);
        require(vm.isFile(path), string.concat("No bot config for chain ", vm.toString(chainId), " at ", path));
        string memory config = vm.readFile(path);

        deployment.owner = vm.envOr("LIQUIDATOR_OWNER", msg.sender);
        deployment.swapper = vm.parseTomlAddress(config, ".swapper_address");
        deployment.swapVerifier = vm.envOr("SWAP_VERIFIER_ADDRESS", address(0));
        deployment.evc = vm.parseTomlAddress(config, ".evc_address");
        if (vm.keyExistsToml(config, ".pyth")) {
            deployment.pyth = vm.parseTomlAddress(config, ".pyth.address");
        }

        // Fail before broadcasting rather than deploying a liquidator the bot will refuse to start with.
        require(deployment.owner != address(0), "Owner is the zero address");
        require(deployment.swapper.code.length > 0, "Configured swapper has no code on this chain");
        require(deployment.evc.code.length > 0, "Configured EVC has no code on this chain");
        require(
            deployment.pyth == address(0) || deployment.pyth.code.length > 0,
            "Configured Pyth has no code on this chain"
        );
    }

    function configPath(uint256 chainId) public view returns (string memory) {
        return string.concat(vm.projectRoot(), "/../configs/Config.", vm.toString(chainId), ".toml");
    }
}
