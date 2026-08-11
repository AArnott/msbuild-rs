# Performance benchmarking

The suite reports two parity-gated command-line measurements:

- **fresh-process end-to-end preprocess** compares
  `dotnet msbuild -pp:<file>` with `msbuild-rs --preprocess <file>`.
- **fresh-process end-to-end evaluation-query** compares target-free
  `dotnet msbuild -getProperty:... -getItem:...` with the equivalent
  `msbuild-rs --get-property ... --get-item ...` selection. It does not write a
  `/pp` file.

Both include process startup, CLI parsing, SDK/toolset discovery, project
loading, evaluation, query/preprocess serialization, and process exit. They are
not in-process library throughput. This repository has no existing in-process
benchmark harness, so it does not report or compare such a number.

## Reproduce

PowerShell 7, the SDK pinned by `global.json`, and a release build are required.

```powershell
cargo build --release --locked

# One preprocess case.
pwsh -File ./scripts/compare-preprocess.ps1 `
  -Project ./sample_projects/simple.proj `
  -Warmup 5 `
  -Iterations 30 `
  -OutputDirectory ./benchmark-results/simple-preprocess

# One target-free evaluation-query case.
pwsh -File ./scripts/compare-evaluation-performance.ps1 `
  -Project ./sample_projects/simple.proj `
  -PropertyName Configuration,OutputPath `
  -ItemType Compile `
  -FixtureInputFile ./sample_projects/simple.proj `
  -Warmup 5 `
  -Iterations 30 `
  -OutputDirectory ./benchmark-results/simple-evaluation

# Generate and verify fixed-seed inputs.
pwsh -File ./scripts/generate-performance-fixtures.ps1 `
  -Preset Benchmark `
  -OutputDirectory ./benchmark-results/generated-fixtures
pwsh -File ./scripts/generate-performance-fixtures.ps1 `
  -OutputDirectory ./benchmark-results/generated-fixtures `
  -VerifyOnly

# Run both modes for simple and every generated manifest case.
pwsh -File ./scripts/run-performance-suite.ps1 `
  -Preset Benchmark `
  -Warmup 5 `
  -Iterations 30 `
  -OutputDirectory ./benchmark-results/performance
```

`Smoke` is a small validation preset. `Benchmark` defaults to 3,000
properties, items, and conditions, a 40-deep by 4-wide import graph, and 800
glob files. `-PropertyCount`, `-ItemCount`, `-ConditionCount`,
`-ImportDepth`, `-ImportWidth`, and `-GlobFileCount` override either preset.
The deterministic seed defaults to `1729`.

The generated manifest declares `properties`, `items`, `conditions`, `imports`,
and `representative`. The suite adds `simple`. Every declared case is recorded
for both modes; a `-Fixture`, `-Mode`, or `-SkipSimple` exclusion remains in
`suite-summary.*` with an explicit skipped reason. A failure also remains in
the summary and makes the suite fail after the other cases run.

Each generated project exposes a compact query probe after its large workload.
The evaluation-query parity gate compares selected final properties, probe item
identities, and selected metadata. This exercises finalized property and item
evaluation without running targets and keeps the two query payloads small.

## Fixture identity

Generation writes LF-only UTF-8 from deterministic seed/configuration inputs.
`manifest.json` records every generated relative path, byte count, SHA-256,
per-case fixture hash, query selection, and aggregate content hash.
`manifest.sha256` authenticates the manifest. Generation and `-VerifyOnly`
reject any missing, extra, or changed file.

The benchmark prefers that generated manifest. A case identity therefore
includes its root project, imported projects, glob inputs, and every other file
under that generated case. For a non-generated project, the preprocess
comparator derives a deterministic root/import file list from its untimed
aggregate preprocess pass. The evaluation comparator accepts
`-FixtureInputFile`; if neither an explicit list nor a generated manifest is
available, it performs one untimed preprocess pass only to discover the input
list. That discovery is never a measured evaluation sample.

`run-metadata.json` records the identity source and the exact normalized and
resolved input paths, sizes, and SHA-256 values. Changing an imported file
changes the identity even when the root project does not. Identity is finalized
before warmups, so it never depends on timed output.

## Measurement protocol

For each selected case and mode, the comparator:

1. launches both implementations directly with `System.Diagnostics.Process`;
2. retains raw output and stops unless the mode-specific normalized parity gate
   passes;
3. alternates which implementation runs first for each warmup and measured
   pair;
4. measures process start through exit and polls
   `Process.PeakWorkingSet64` every 10 ms; and
5. summarizes only measured, parity-valid, zero-exit samples.

The summary reports wall-clock min, median, arithmetic mean, nearest-rank p95,
max, and median absolute deviation (MAD), plus median/max peak working set.
The ratio is `dotnet median / msbuild-rs median`; larger favors msbuild-rs.
There is deliberately no hard CI speed threshold.

Fresh-process results are sensitive to antivirus activity, power policy, shared
runner load, filesystem cache state, and first-run .NET setup. Prefer medians,
inspect p95/MAD and raw ordering, and compare only the same fixture/manifest
hash, SDK, commit, host class, warmups, and iterations. Peak working set is
process-only and does not attribute kernel cache or short-lived helper memory.
The two CLIs also have different JSON wire shapes; parity compares the same
selected semantic projection rather than raw JSON bytes.

## Artifacts and tracked summary

Raw artifacts are ignored by Git under `benchmark-results/`:

```text
<output>/<os>-<arch>/
  generated-fixtures/
    manifest.json
    manifest.sha256
    <fixture>/...
  suite-summary.csv
  suite-summary.json
  fixtures/<fixture>/<preprocess|evaluation-query>/
    run-metadata.json
    parity.json
    samples.csv
    summary.csv
    summary.json
    raw/
    normalized/
```

Sample rows include mode, sequence, phase, pair order, implementation, wall
time, peak bytes/MiB, exit/parity/valid flags, fixture hash, commit/dirty state,
host information, SDK, and command. Warmups remain in `samples.csv` but are
excluded from summaries.

The compact checked-in Windows baseline is
[`performance-baseline.md`](performance-baseline.md). It records the collection
date, benchmarked commit/worktree state, host, SDK, generated manifest hash,
case set, warmups/iterations, median/p95/peak memory, parity, and ratios for
both modes. It is local Windows evidence, not a portable promise; no Linux
numbers are inferred or fabricated.

The scheduled/manual `Performance benchmarks` workflow runs the full Benchmark
set and both modes on Windows and Linux, then uploads the complete artifact tree
for 30 days. It gathers evidence and parity failures, but applies no flaky
performance pass/fail threshold.
