# Recipe classes and measurements

Arms use one map keyed by exact SDK catalog ids, rather than separate hardware,
TP and overlay tables or duplicated flat recipe rows. This keeps selection local
while the SDK remains the sole hardware source. Throughput observations retain
raw counters, duration and provider/model/workload provenance; class-only rates
cannot distinguish sellers or checkpoint changes and must not drive selection.
