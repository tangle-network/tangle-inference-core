# Corrected serving ownership

Pin qualified default-branch commits and released SDK artifacts. The SDK catalog
must be merged to blueprint main before any consumer pins it. The cloud-only
cargo-tangle binary and native tnt-core manager initialization release are core
merge prerequisites; do not copy their missing interfaces into core.

| Capability | Owner |
| --- | --- |
| Engine plans, replica/router children, verified weights | tangle-inference-core |
| GPU ids, decimal GB, detection, TEE, keystore | Blueprint SDK |
| Manager initialization and registry callback | tnt-core base contract |
| Blueprint registration and release artifacts | cargo-tangle and cargo-dist |
| Client SpendAuth signatures | tcloud |
| Request mapping, model admission and pricing policy | Individual blueprint |
| Leases, renewal, holds and namespace retirement | Central orchestration over SDK providers |

Arms select exact catalog ids. Each class entry owns its supported TP values and
argument/environment overlay; VRAM and node limits are read from the SDK. A
measurement binds model/manifest, arm, class, TP, replicas, provider SKU, region,
workload and engine digest. Derive throughput from recorded counters and elapsed
time, and keep prices/ranking in their SDK owner.

Operator-lite parity is exercised by abliterated-serving's maintained
`smoke.py --parity`: bearer, model/context metadata, bounded cold readiness,
Retry-After, streaming, tools, minors-content refusal and cached usage. The Worker
and old provider remain until the real node passes that boundary. A controlled
small-model fixture belongs in tests, never the production family catalog.

The existing operator-lite/Ollama cached-billing lane remains a required proof.
Preserve its configured service and use an isolated candidate process for checks.
