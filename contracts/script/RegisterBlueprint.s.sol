// SPDX-License-Identifier: MIT
pragma solidity ^0.8.26;

import {Script, console2} from "forge-std/Script.sol";
import {Types} from "tnt-core/libraries/Types.sol";

interface IInferenceBlueprintRegistry {
    function createBlueprint(Types.BlueprintDefinition calldata definition) external returns (uint64);
}

/// Blueprint adapters supply manager deployment; the definition owns metadata and GPU requirements.
abstract contract RegisterBlueprint is Script {
    function _deployManager() internal virtual returns (address);

    function _definitionPath() internal view virtual returns (string memory) {
        return vm.envOr("BLUEPRINT_DEFINITION", string("../deploy/definition.json"));
    }

    function _guardNetwork(address) internal view virtual {}

    function _afterRegistration(address, uint64) internal virtual {}

    function run() external virtual {
        address registry = vm.envAddress("TANGLE_CORE");
        require(registry.code.length != 0, "TANGLE_CORE has no code");
        _guardNetwork(registry);
        uint256 key = vm.envUint("PRIVATE_KEY");
        vm.startBroadcast(key);
        address manager = _deployManager();
        uint64 id = IInferenceBlueprintRegistry(registry).createBlueprint(_buildDefinition(manager));
        _afterRegistration(registry, id);
        vm.stopBroadcast();
        _report(manager, id);
    }

    function _report(address manager, uint64 id) internal virtual {
        console2.log("DEPLOY_INFERENCE_BSM=%s", vm.toString(manager));
        console2.log("DEPLOY_INFERENCE_BSM_PROXY=%s", vm.toString(manager));
        console2.log("DEPLOY_INFERENCE_BLUEPRINT_ID=%s", vm.toString(id));
    }

    function _buildDefinition(address manager) internal view virtual returns (Types.BlueprintDefinition memory def) {
        string memory json = vm.readFile(_definitionPath());
        def.metadataUri = vm.parseJsonString(json, ".metadata_uri");
        def.metadataHash = keccak256(bytes(json));
        def.manager = manager;
        def.hasConfig = true;
        def.config = Types.BlueprintConfig({
            membership: _membership(vm.parseJsonString(json, ".config.membership")),
            pricing: Types.PricingModel.EventDriven,
            minOperators: uint32(vm.parseJsonUint(json, ".config.min_operators")),
            maxOperators: uint32(vm.parseJsonUint(json, ".config.max_operators")),
            subscriptionRate: 0, subscriptionInterval: 0, eventRate: 0
        });
        def.metadata = Types.BlueprintMetadata({
            name: vm.parseJsonString(json, ".metadata.name"),
            description: vm.parseJsonString(json, ".metadata.description"),
            author: vm.parseJsonString(json, ".metadata.author"),
            category: "AI/Inference",
            codeRepository: vm.parseJsonString(json, ".metadata.code_repository"),
            logo: "",
            website: vm.parseJsonString(json, ".metadata.website"),
            license: vm.parseJsonString(json, ".metadata.license"),
            profilingData: vm.parseJsonString(json, ".metadata.profiling_data")
        });
        uint256 count;
        while (vm.keyExistsJson(json, string.concat(".jobs[", vm.toString(count), "]"))) ++count;
        require(count > 0 && count <= 256, "invalid job count");
        def.jobs = new Types.JobDefinition[](count);
        for (uint256 i; i < count; ++i) {
            def.jobs[i].name = vm.parseJsonString(json, string.concat(".jobs[", vm.toString(i), "].name"));
        }
        string[] memory memberships = vm.parseJsonStringArray(json, ".supported_memberships");
        def.supportedMemberships = new Types.MembershipModel[](memberships.length);
        for (uint256 i; i < memberships.length; ++i) def.supportedMemberships[i] = _membership(memberships[i]);
        def.sources = new Types.BlueprintSource[](1);
        require(keccak256(bytes(vm.parseJsonString(json, ".sources[0].kind"))) == keccak256("Native"), "native source required");
        Types.BlueprintSource memory source;
        source.kind = Types.BlueprintSourceKind.Native;
        string memory fetcher = vm.parseJsonString(json, ".sources[0].native.fetcher");
        require(keccak256(bytes(fetcher)) == keccak256("Github"), "Github source required");
        string memory github = ".sources[0].native.github";
        source.native = Types.NativeSource({
            fetcher: Types.BlueprintFetcherKind.Github,
            artifactUri: vm.serializeJsonType(
                "GithubArtifact(Binary[] binaries,string owner,string repo,string tag)Binary(string arch,string name,string os,string sha256)",
                vm.parseJsonType(json, github, "GithubArtifact(Binary[] binaries,string owner,string repo,string tag)Binary(string arch,string name,string os,string sha256)")
            ),
            entrypoint: vm.parseJsonString(json, ".sources[0].native.entrypoint")
        });
        source.testing = Types.TestingSource({
            cargoPackage: vm.parseJsonString(json, ".sources[0].testing.cargo_package"),
            cargoBin: vm.parseJsonString(json, ".sources[0].testing.cargo_bin"),
            basePath: vm.parseJsonString(json, ".sources[0].testing.base_path")
        });
        source.binaries = new Types.BlueprintBinary[](1);
        require(keccak256(bytes(vm.parseJsonString(json, string.concat(github, ".binaries[0].arch")))) == keccak256("amd64"), "amd64 binary required");
        require(keccak256(bytes(vm.parseJsonString(json, string.concat(github, ".binaries[0].os")))) == keccak256("linux"), "linux binary required");
        string memory hash = vm.parseJsonString(json, string.concat(github, ".binaries[0].sha256"));
        source.binaries[0] = Types.BlueprintBinary({
            arch: Types.BlueprintArchitecture.Amd64,
            os: Types.BlueprintOperatingSystem.Linux,
            name: vm.parseJsonString(json, string.concat(github, ".binaries[0].name")),
            sha256: vm.parseBytes32(string.concat("0x", hash))
        });
        def.sources[0] = source;
    }

    function _membership(string memory value) private pure returns (Types.MembershipModel) {
        if (keccak256(bytes(value)) == keccak256("Dynamic")) return Types.MembershipModel.Dynamic;
        require(keccak256(bytes(value)) == keccak256("Fixed"), "invalid membership");
        return Types.MembershipModel.Fixed;
    }
}
