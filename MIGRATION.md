# Inference blueprint adapters

Pin `tangle-inference-core` by full Git commit in every blueprint's Cargo manifest.
Core owns engine supervision and recipes, billing, HTTP payment gates, replay
protection, GPU health, metrics, QoS and the SDK runner boilerplate.
Blueprints retain their HTTP adapters, model rules, pricing and chain job schemas.

- Import `engine::{EngineConfig, EngineKind, EngineProcess, RecipeCatalog}`.
  Derivatives name `base_family`; select `recipe_arm`, `gpu_class` and tensor
  parallelism. Core applies arm/GPU args and env, rejecting unsupported shapes.
  `external` attaches an existing backend.
- Expand `run_operator!` with the adapter's registration payload, router and
  background service. Reuse `qos::{QoSConfig, start_heartbeat}`.
- Inherit `contracts/InferenceBSMBase.sol` and bind the trusted registry in the
  constructor or atomic proxy initializer before registration.
- Inherit `contracts/script/RegisterBlueprint.s.sol` in a small manager-deployment
  adapter. The shared builder reads `deploy/definition.json`, including
  `metadata.profiling_data`; the manager address comes from deployment.
  Set `TANGLE_CORE`, supply the signing key through the vault, and grant Foundry
  read access to the definition. Use `BLUEPRINT_DEFINITION` for another path.
- Resolve the pinned checkout using Cargo metadata's `manifest_path` for
  `tangle-inference-core`, then pass the Foundry remapping
  `tangle-inference-core/=<core checkout>/contracts/` on compile, test and deploy.
- `scripts/sign-spend-auth.mjs` takes its private key on stdin and the remaining
  SpendAuthorization fields as flags. It shares Rust's verification digest.
  `scripts/test-full-protocol.sh [blueprint directory]` runs real chain integration
  and optional blueprint checks. Run these through the admitted Beelink gate.

The catalog move preserves abliterated-inference's JSON format and pinned image.
SDK detection/catalog and class plus VRAM-per-GPU requirements replace the legacy
GPU shape when the SDK owner releases them; the lane STATUS records that dependency.
