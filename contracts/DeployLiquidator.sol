// SPDX-License-Identifier: MIT

pragma solidity ^0.8.24;

import {Script} from "forge-std/Script.sol";
import {Test} from "forge-std/Test.sol";

import {Liquidator} from "./Liquidator.sol";

import "forge-std/console2.sol";

contract DeployLiquidator is Script {
    function run() public {
        uint256 deployerPrivateKey = vm.envUint("LIQUIDATOR_PRIVATE_KEY");

        address evcAddress = 0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383;
        address swapperAddress = 0x719F8b330CcA71cb6195D032A43194C7D3F9Fb45;
        address swapVerifierAddress = 0x786c900d7D348662703C38B46f24c1cda2C582AB;
        address pyth = 0x4305FB66699C3B2702D4d05CF36551390A4c69C6;

        address deployer = vm.addr(deployerPrivateKey);
        vm.startBroadcast(deployerPrivateKey);

        uint256 beforeGas = gasleft();
        console2.log("Gas before: ", beforeGas);
        console2.log("Gas price: ", tx.gasprice);

        Liquidator liquidator = new Liquidator(deployer, swapperAddress, swapVerifierAddress, evcAddress, pyth);
        uint256 afterGas = gasleft();
        console2.log("Gas after: ", afterGas);

        console2.log("Total gas cost: ", (beforeGas - afterGas) * tx.gasprice);

        console2.log("Liquidator deployed at: ", address(liquidator));
    }
}
