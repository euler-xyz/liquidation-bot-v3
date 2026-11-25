// SPDX-License-Identifier: MIT

pragma solidity ^0.8.26;

import {Test} from "forge-std/Test.sol";

import {Liquidator} from "../src/Liquidator.sol";
import {DeployLiquidator} from "../script/DeployLiquidator.s.sol";

contract DeployLiquidatorTest is Test {
    uint256 constant MAINNET = 1;
    // TAC has no Pyth deployment.
    uint256 constant TAC = 239;

    DeployLiquidator script;

    function setUp() public {
        script = new DeployLiquidator();
    }

    function test_deploysWithAddressesFromConfig() public {
        vm.chainId(MAINNET);
        string memory config = vm.readFile(script.configPath(MAINNET));
        address evc = vm.parseTomlAddress(config, ".evc_address");
        address swapper = vm.parseTomlAddress(config, ".swapper_address");
        address pyth = vm.parseTomlAddress(config, ".pyth.address");
        _etchCode(evc, swapper, pyth);

        Liquidator liquidator = script.run();

        assertEq(liquidator.owner(), address(this));
        assertEq(liquidator.evcAddress(), evc);
        assertEq(liquidator.swapperAddress(), swapper);
        assertEq(liquidator.PYTH(), pyth);
        assertEq(liquidator.swapVerifierAddress(), address(0));
    }

    function test_deploysWithoutPythWhenChainHasNone() public {
        vm.chainId(TAC);
        string memory config = vm.readFile(script.configPath(TAC));
        assertFalse(vm.keyExistsToml(config, ".pyth"));
        address evc = vm.parseTomlAddress(config, ".evc_address");
        address swapper = vm.parseTomlAddress(config, ".swapper_address");
        _etchCode(evc, swapper, address(0));

        Liquidator liquidator = script.run();

        assertEq(liquidator.evcAddress(), evc);
        assertEq(liquidator.swapperAddress(), swapper);
        assertEq(liquidator.PYTH(), address(0));
    }

    function test_revertsWhenConfiguredContractsHaveNoCode() public {
        vm.expectRevert("Configured swapper has no code on this chain");
        script.loadDeployment(MAINNET);
    }

    function test_revertsForChainWithoutConfig() public {
        vm.expectRevert();
        script.loadDeployment(31337);
    }

    function _etchCode(address evc, address swapper, address pyth) internal {
        vm.etch(evc, hex"00");
        vm.etch(swapper, hex"00");
        if (pyth != address(0)) vm.etch(pyth, hex"00");
    }
}
