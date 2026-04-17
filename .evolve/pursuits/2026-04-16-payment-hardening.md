# Pursuit: Payment Hardening — Ship DirectProvider as Production-Ready
Generation: 1
Status: building
Started: 2026-04-16

## Thesis

Wire PaymentProvider trait through the entire server stack so blueprints can switch payment modes via config without code changes. Fix every architectural gap the harden run exposed.

## Moonshot considered

Replace the entire billing/server module with a generic payment middleware (axum layer) that handles any PaymentProvider transparently. All payment logic in one middleware, no per-endpoint billing code.

**Half-adopted.** A full middleware rewrite is too risky for one session — the existing billing_gate/settle_billing pattern works and blueprints depend on it. Instead: make billing_gate/settle_billing PaymentProvider-generic so they work with any provider, keep the same call pattern blueprints already use.

## Changes (6, ordered by dependency)

### 1. AppState uses Arc<dyn PaymentProvider> instead of Arc<BillingClient>

AppState.billing becomes Arc<dyn PaymentProvider>. AppStateBuilder accepts any provider. from_config creates the provider via create_provider(mode, ...).

### 2. Generic billing_gate that works with any PaymentProvider

Current billing_gate extracts SpendAuth, calls validate_spend_auth (ShieldedCredits-specific), calls authorize_spend. New version: extract PaymentProof (SpendAuth OR DirectTransfer), call provider.authorize(proof). Same shape, any provider.

### 3. Generic settle_billing that works with any PaymentProvider

Current settle_billing calls BillingClient.claim_payment. New version: call provider.settle(proof, actual_cost). ShieldedProvider does on-chain claim. DirectProvider is a no-op. Same shape.

### 4. Fix settle_billing failure recovery

Add automated retry loop to SettlementRecoveryQueue. On startup, replay failed settlements. Add queue depth metric. Cap queue size.

### 5. Fix AccountGuard Drop race

Replace try_write + spawn fallback with a channel-based decrement. Drop sends to an mpsc channel, a background task processes decrements. No lost decrements.

### 6. PaymentMode in BillingConfig

Add payment_mode field to BillingConfig (default: "shielded" for backward compat). from_config reads it and creates the right provider.

## Build Status

| # | Change | Status |
|---|---|---|
| 1 | AppState Arc<dyn PaymentProvider> | not started |
| 2 | Generic billing_gate | not started |
| 3 | Generic settle_billing | not started |
| 4 | Settlement retry loop | not started |
| 5 | AccountGuard channel fix | not started |
| 6 | PaymentMode in config | not started |
