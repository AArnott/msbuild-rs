# Tracked performance baseline

This is a compact local Windows record. Raw samples and outputs remain ignored
under `benchmark-results/`; scheduled CI uploads Windows and Linux artifacts
instead of committing them.

- **Collected:** 2026-08-08T06:37:08Z
- **Commit:** `36d3106f5e9530400efcc14af4d1c85be7ed76ef` (clean worktree)
- **Host:** `windows-x64`, Microsoft Windows 10.0.26200,
  AMD Ryzen Threadripper PRO 7995WX 96-Cores
- **SDK:** .NET SDK 10.0.302
- **Preset:** `Benchmark`, seed 1729; 3,000 properties/items/conditions,
  40-deep × 4-wide imports, 800 glob files
- **Generated manifest SHA-256:**
  `3ec4eb2f6397256a03a4b6bec9295ab92bd4def4d1e1d53915adaebc3ab15575`
- **Cases:** simple, properties, items, conditions, imports, representative
- **Sampling:** 2 warmups and 10 measured processes per implementation, per
  case and mode; interleaved order
- **Gate:** all 12 case/mode normalized parity checks passed; no speed
  threshold was applied

Times are milliseconds. `median / p95` is fresh-process wall time. `peak MiB`
is the maximum observed `PeakWorkingSet64` across measured processes. Ratio is
`dotnet median / msbuild-rs median`.

| Case | Mode | dotnet median / p95 | dotnet peak MiB | Rust median / p95 | Rust peak MiB | Ratio | Parity |
|---|---|---:|---:|---:|---:|---:|---|
| simple | preprocess | 1227.47 / 3637.30 | 75.65 | 74.41 / 82.75 | 3.50 | 16.50x | passed |
| simple | evaluation-query | 350.84 / 1493.25 | 75.72 | 71.50 / 74.16 | 2.65 | 4.91x | passed |
| properties | preprocess | 499.37 / 1378.15 | 307.69 | 155.53 / 182.65 | 160.60 | 3.21x | passed |
| properties | evaluation-query | 494.80 / 1178.05 | 306.27 | 155.84 / 166.37 | 156.14 | 3.18x | passed |
| items | preprocess | 440.89 / 2080.34 | 93.52 | 201.06 / 246.55 | 21.01 | 2.19x | passed |
| items | evaluation-query | 446.24 / 1138.73 | 94.36 | 199.66 / 220.75 | 21.30 | 2.23x | passed |
| conditions | preprocess | 405.19 / 1091.63 | 82.45 | 94.23 / 107.35 | 8.95 | 4.30x | passed |
| conditions | evaluation-query | 371.85 / 999.51 | 80.06 | 93.97 / 117.13 | 7.60 | 3.96x | passed |
| imports | preprocess | 390.10 / 669.03 | 78.89 | 79.17 / 87.02 | 8.14 | 4.93x | passed |
| imports | evaluation-query | 472.37 / 1385.35 | 79.94 | 85.34 / 119.70 | 6.68 | 5.53x | passed |
| representative | preprocess | 436.26 / 1016.91 | 105.83 | 128.77 / 142.12 | 33.31 | 3.39x | passed |
| representative | evaluation-query | 433.53 / 1055.66 | 105.97 | 134.85 / 137.94 | 31.01 | 3.21x | passed |

## Interpretation

These are **fresh-process end-to-end preprocess** and **fresh-process
end-to-end evaluation-query** results, not in-process Rust library throughput.
The evaluation-query commands run no targets and write no `/pp` file.

The first investigation run exposed eager decoding of every escaped
property/metadata value as an obvious hot path. Making that decoding lazy cut
the properties-case Rust median and peak memory materially. The item workload
still measured 2.19x and 2.23x; it performs 3,000 explicit item evaluations and
an eager 800-file glob. No comparably obvious safe hot-path fix remained, so
those ratios are reported unchanged.

The dotnet p95 values show substantial local outliers; with ten samples,
nearest-rank p95 is the maximum. Antivirus, scheduling, caches, and power state
can dominate short fresh-process commands. Treat this as reproducible evidence
for this commit/host/manifest, not a cross-platform guarantee. No Linux result
is inferred from this Windows run.
