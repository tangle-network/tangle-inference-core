// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {BlueprintServiceManagerBase} from "tnt-core/BlueprintServiceManagerBase.sol";

/// Bind the registry during construction or atomic proxy initialization, before broadcasting registration.
abstract contract InferenceBSMBase is BlueprintServiceManagerBase {
    function _bindRegistrationCore(address registry) internal {
        require(registry != address(0), "Tangle registry required");
        if (tangleCore != address(0)) revert AlreadyInitialized();
        tangleCore = registry;
        blueprintOwner = msg.sender;
        blueprintId = type(uint64).max;
    }

    function onBlueprintCreated(uint64 id, address owner, address registry) external override {
        if (tangleCore == address(0) || msg.sender != tangleCore || registry != tangleCore) {
            revert OnlyTangleAllowed(msg.sender, tangleCore);
        }
        if (blueprintId != type(uint64).max) revert AlreadyInitialized();
        require(owner == blueprintOwner, "Unexpected blueprint creator");
        require(id != type(uint64).max, "Invalid blueprint id");
        blueprintId = id;
    }
}
