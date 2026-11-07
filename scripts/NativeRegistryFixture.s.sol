// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;
import {Script, console2} from "forge-std/Script.sol";
import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";
import {Tangle} from "tnt-core/Tangle.sol";
import {TangleBlueprintsFacet} from "tnt-core/facets/tangle/TangleBlueprintsFacet.sol";
import {TangleBlueprintsManagementFacet} from "tnt-core/facets/tangle/TangleBlueprintsManagementFacet.sol";
import {MBSMRegistry} from "tnt-core/MBSMRegistry.sol";
import {MasterBlueprintServiceManager} from "tnt-core/MasterBlueprintServiceManager.sol";

contract NativeRegistryFixture is Script {
    function run() external {
        address owner = vm.envAddress("NATIVE_TEST_OWNER");
        vm.startBroadcast(owner);
        Tangle registry = Tangle(
            payable(address(
                    new ERC1967Proxy(
                        address(new Tangle()), abi.encodeCall(Tangle.initialize, (owner, owner, payable(owner)))
                    )
                ))
        );
        registry.registerFacet(address(new TangleBlueprintsFacet()));
        registry.registerFacet(address(new TangleBlueprintsManagementFacet()));
        MBSMRegistry versions = MBSMRegistry(
            address(new ERC1967Proxy(address(new MBSMRegistry()), abi.encodeCall(MBSMRegistry.initialize, (owner))))
        );
        versions.addVersion(address(new MasterBlueprintServiceManager(owner, address(registry))));
        versions.grantRole(versions.MANAGER_ROLE(), address(registry));
        registry.setMBSMRegistry(address(versions));
        vm.stopBroadcast();
        console2.log("NATIVE_TEST_REGISTRY=%s", vm.toString(address(registry)));
    }
}
