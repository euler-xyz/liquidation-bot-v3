// SPDX-License-Identifier: MIT

pragma solidity ^0.8.24;

import {Test} from "forge-std/Test.sol";

import {DeployLiquidator} from "../DeployLiquidator.sol";
import {Liquidator} from "../Liquidator.sol";

contract DeployLiquidatorTest is Test {
    // Anvil's first well-known development key, it never holds funds.
    uint256 internal constant DEPLOYER_KEY = 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80;

    bytes internal constant SOME_CODE = hex"00";

    address internal evc;
    address internal swapper;
    address internal swapVerifier;
    address internal pyth;
    address internal noCode;

    DeployLiquidator internal script;

    function setUp() public {
        script = new DeployLiquidator();

        evc = makeAddr("evc");
        swapper = makeAddr("swapper");
        swapVerifier = makeAddr("swapVerifier");
        pyth = makeAddr("pyth");
        noCode = makeAddr("noCode");

        vm.etch(evc, SOME_CODE);
        vm.etch(swapper, SOME_CODE);
        vm.etch(swapVerifier, SOME_CODE);
        vm.etch(pyth, SOME_CODE);
    }

    // Environment variables are process wide and forge runs tests in parallel, so this is the
    // only test that sets them.
    function test_run_readsAddressesFromEnv() public {
        vm.setEnv("LIQUIDATOR_PRIVATE_KEY", vm.toString(bytes32(DEPLOYER_KEY)));
        vm.setEnv("EVC_ADDRESS", vm.toString(evc));
        vm.setEnv("SWAPPER_ADDRESS", vm.toString(swapper));
        vm.setEnv("SWAP_VERIFIER_ADDRESS", vm.toString(swapVerifier));

        vm.setEnv("PYTH_ADDRESS", vm.toString(pyth));
        assertDeployed(script.run(), pyth);

        vm.setEnv("PYTH_ADDRESS", vm.toString(address(0)));
        assertDeployed(script.run(), address(0));
    }

    function test_deploy_withoutPyth() public {
        Liquidator liquidator = script.deploy(vm.addr(DEPLOYER_KEY), evc, swapper, swapVerifier, address(0));

        assertDeployed(liquidator, address(0));
    }

    function test_deploy_revertsWhenEvcHasNoCode() public {
        vm.expectRevert(abi.encodeWithSelector(DeployLiquidator.NoCodeAt.selector, "EVC_ADDRESS", noCode));
        script.deploy(vm.addr(DEPLOYER_KEY), noCode, swapper, swapVerifier, pyth);
    }

    function test_deploy_revertsWhenSwapperHasNoCode() public {
        vm.expectRevert(abi.encodeWithSelector(DeployLiquidator.NoCodeAt.selector, "SWAPPER_ADDRESS", noCode));
        script.deploy(vm.addr(DEPLOYER_KEY), evc, noCode, swapVerifier, pyth);
    }

    function test_deploy_revertsWhenSwapVerifierHasNoCode() public {
        vm.expectRevert(abi.encodeWithSelector(DeployLiquidator.NoCodeAt.selector, "SWAP_VERIFIER_ADDRESS", noCode));
        script.deploy(vm.addr(DEPLOYER_KEY), evc, swapper, noCode, pyth);
    }

    function test_deploy_revertsWhenPythHasNoCode() public {
        vm.expectRevert(abi.encodeWithSelector(DeployLiquidator.NoCodeAt.selector, "PYTH_ADDRESS", noCode));
        script.deploy(vm.addr(DEPLOYER_KEY), evc, swapper, swapVerifier, noCode);
    }

    function assertDeployed(Liquidator liquidator, address expectedPyth) internal view {
        assertGt(address(liquidator).code.length, 0, "liquidator has no code");
        assertEq(liquidator.owner(), vm.addr(DEPLOYER_KEY), "owner");
        assertEq(liquidator.evcAddress(), evc, "evc");
        assertEq(liquidator.swapperAddress(), swapper, "swapper");
        assertEq(liquidator.swapVerifierAddress(), swapVerifier, "swap verifier");
        assertEq(liquidator.PYTH(), expectedPyth, "pyth");
    }
}
