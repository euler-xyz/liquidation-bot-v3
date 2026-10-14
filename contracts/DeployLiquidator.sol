// SPDX-License-Identifier: MIT

pragma solidity ^0.8.24;

import {Script} from "forge-std/Script.sol";
import {console2} from "forge-std/console2.sol";

import {Liquidator} from "./Liquidator.sol";

contract DeployLiquidator is Script {
    error NoCodeAt(string name, address target);

    function run() public returns (Liquidator liquidator) {
        uint256 deployerPrivateKey = vm.envUint("LIQUIDATOR_PRIVATE_KEY");

        address evcAddress = vm.envAddress("EVC_ADDRESS");
        address swapperAddress = vm.envAddress("SWAPPER_ADDRESS");
        address swapVerifierAddress = vm.envAddress("SWAP_VERIFIER_ADDRESS");
        // Required even on a chain without Pyth, where it is the zero address: the bot then
        // expects `PYTH()` to be zero and its config to have no `[pyth]` table.
        address pyth = vm.envAddress("PYTH_ADDRESS");

        address deployer = vm.addr(deployerPrivateKey);
        vm.startBroadcast(deployerPrivateKey);

        uint256 beforeGas = gasleft();
        console2.log("Gas before: ", beforeGas);
        console2.log("Gas price: ", tx.gasprice);

        liquidator = deploy(deployer, evcAddress, swapperAddress, swapVerifierAddress, pyth);
        uint256 afterGas = gasleft();
        console2.log("Gas after: ", afterGas);

        console2.log("Total gas cost: ", (beforeGas - afterGas) * tx.gasprice);

        console2.log("Liquidator deployed at: ", address(liquidator));
        vm.stopBroadcast();
    }

    function deploy(
        address owner,
        address evcAddress,
        address swapperAddress,
        address swapVerifierAddress,
        address pyth
    ) public returns (Liquidator) {
        requireCode("EVC_ADDRESS", evcAddress);
        requireCode("SWAPPER_ADDRESS", swapperAddress);
        requireCode("SWAP_VERIFIER_ADDRESS", swapVerifierAddress);
        if (pyth != address(0)) requireCode("PYTH_ADDRESS", pyth);

        return new Liquidator(owner, swapperAddress, swapVerifierAddress, evcAddress, pyth);
    }

    function requireCode(string memory name, address target) internal view {
        if (target.code.length == 0) revert NoCodeAt(name, target);
    }
}
