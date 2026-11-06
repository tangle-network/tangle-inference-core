# Serving terms

- **Family**: a base model architecture/checkpoint lineage whose derivatives share a serving recipe. _Avoid_: per-model serving registry.
- **Arm**: one quantization/runtime choice within a family. _Avoid_: GPU shape.
- **Class entry**: an arm's supported TP values and overlays for one exact SDK GPU class id. It contains no copied hardware facts.
- **Replica**: one engine worker process serving a model on its assigned devices.
- **Replica set**: the workers for one model behind one cache-aware SGLang router.
- **Weight manifest**: the SHA-256-pinned complete file list for a model directory.
- **Verified weights**: a model directory published only after its manifest and every file verify.
- **Measurement**: an immutable observation tied to a model/manifest, runtime, provider SKU and workload. _Avoid_: class-only throughput.
