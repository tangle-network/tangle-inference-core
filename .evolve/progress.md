# tangle-inference-core — Evolve Progress

## Gen 1: Payment Hardening (2026-04-16)

### What shipped
1. **PaymentProvider trait** — decouples billing from ShieldedCredits. Three implementations: ShieldedProvider (existing), DirectProvider (ERC-20 transfer verification), NoopProvider (open endpoint).
2. **4 CRITICAL/HIGH security fixes** — tx_hash replay protection, block_number required, expected_token required, strict amount parsing.
3. **AppState wired** — `payment_provider: Arc<dyn PaymentProvider>` alongside existing `billing` for backward compat. `from_config` creates from `billing.payment_mode`.
4. **Generic payment_gate + settle_payment** — work with any PaymentProvider. New blueprints use these instead of ShieldedCredits-specific billing_gate.
5. **PaymentMode in BillingConfig** — operators set `payment_mode = "shielded" | "direct" | "none"` in config.
6. **47 tests** — 23 original + 19 adversarial + 5 Anvil E2E (real Foundry chain, real ERC-20).

### Test matrix
| Category | Count | Coverage |
|---|---|---|
| Original (billing, config, nonce, cost models, SpendAuth recovery) | 23 | ShieldedCredits path |
| Adversarial (type confusion, serde boundary, bad inputs) | 19 | PaymentProvider trait dispatch, PaymentProof deserialization |
| Anvil E2E (real chain, real ERC-20) | 5 | DirectProvider authorize, replay rejection, insufficient amount, wrong recipient, nonexistent tx |

### Remaining for Gen 2
- Settlement retry loop (automated recovery for failed claim_payment)
- AccountGuard channel-based decrement (no Drop race)
- SettlementRecoveryQueue bounds + alerting
- `from` field validation in DirectTransfer proof
