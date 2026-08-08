# Preprocess performance benchmarking

The performance suite compares fresh release processes for conventional
`dotnet msbuild -pp` and `msbuild-rs --preprocess`. Neither command executes a
target. A fixture is timed only after its raw preprocess outputs normalize to
the same bytes.

## Reproduce

PowerShell 7, the SDK pinned by `global.json`, and a release build are required.

```powershell
cargo build --release --locked

# One project, with parity checked before any warmup or measured process.
./scripts/compare-preprocess.ps1 `
  -Project ./sample_projects/simple.proj `
  -Warmup 5 `
  -Iterations 30 `
  -OutputDirectory ./benchmark-results/simple

# Generate the fixed-seed large cases and verify every recorded hash.
./scripts/generate-performance-fixtures.ps1 `
  -Preset Benchmark `
  -OutputDirectory ./benchmark-results/generated-fixtures
./scripts/generate-performance-fixtures.ps1 `
  -OutputDirectory ./benchmark-results/generated-fixtures `
  -VerifyOnly

# Run simple plus all five generated cases.
./scripts/run-performance-suite.ps1 `
  -Preset Benchmark `
  -Warmup 5 `
  -Iterations 30 `
  -OutputDirectory ./benchmark-results/performance
```

`Smoke` is a small generator preset for pull-request validation. `Benchmark`
defaults to 3,000 properties, items, and conditions, a 40-deep by 4-wide import
graph, and 800 glob files. `-PropertyCount`, `-ItemCount`,
`-ConditionCount`, `-ImportDepth`, `-ImportWidth`, and `-GlobFileCount`
override either preset. The seed defaults to `1729`.

The generated cases are `properties`, `items`, `conditions`, `imports`, and
`representative`. They include percent-escaped lists and wildcards, XML
entities, item definitions and custom metadata, eager `*`, `?`, and `**`
globs, import conditions, and nested import depth. They intentionally use only
the compatibility surface outside the deferred CoreCLR tier.

Generation writes LF-only UTF-8 from deterministic seed/config inputs.
`manifest.json` records every generated relative path, byte count, SHA-256,
per-case fixture hash, and an aggregate content hash. `manifest.sha256`
authenticates the manifest itself. Generation and `-VerifyOnly` reject any
missing, extra, or changed file.

## Measurement protocol

The comparator:

1. launches both implementations directly with
   `System.Diagnostics.Process`;
2. retains raw and path-normalized preprocess output and stops if normalized
   parity fails;
3. alternates which implementation runs first for each warmup and measured
   pair;
4. records elapsed wall time around process start through exit and the maximum
   observed OS-reported `Process.PeakWorkingSet64` (polled every 10 ms so the
   value remains available after short-lived processes exit); and
5. summarizes measured, parity-valid, zero-exit samples only.

The summary reports wall-clock min, median, arithmetic mean, nearest-rank p95,
max, and median absolute deviation (MAD), plus median/max peak working set.
The displayed speed ratio is `dotnet median / Rust median`.

Fresh-process measurements include executable loading, CLI parsing, SDK/toolset
discovery, evaluation, serialization, and output-file I/O. They are not
steady-state library throughput. Antivirus activity, power policy, shared
runner load, filesystem cache state, and first-run .NET setup can dominate an
individual sample. Prefer medians, inspect MAD/p95 and raw ordering, and compare
the same fixture/hash, SDK, commit, host class, warmups, and iteration count.
Peak working set is process-only; it does not attribute kernel cache or
short-lived helper-process memory.

For the repository's exact `global.json` pin (`rollForward: disable`),
msbuild-rs resolves the installed SDK directory and bundled MSBuild version
metadata directly. Other SDK roll-forward policies retain the `dotnet --info`
fallback. Results therefore describe the pinned, reproducible path; compare
fallback discovery separately if that is the deployment of interest.

## Artifact schema

`run-performance-suite.ps1` separates hosts and fixtures:

```text
<output>/<os>-<arch>/
  generated-fixtures/
    manifest.json
    manifest.sha256
    <fixture>/...
  suite-summary.csv
  suite-summary.json
  fixtures/<fixture>/
    run-metadata.json
    parity.json
    samples.csv
    summary.csv
    summary.json
    raw/
      dotnet-preprocessed.xml
      rust-preprocessed.xml
      *-parity.stdout.txt
      *-parity.stderr.txt
      <sequence>-<phase>-<iteration>-<implementation>.*.txt
    normalized/
      dotnet-preprocessed.xml
      rust-preprocessed.xml
    preprocess-mismatch.txt  # failure only
```

Each `samples.csv` row is one direct process and includes sequence, phase,
iteration, pair order, implementation, wall time, peak bytes/MiB, exit code,
parity/valid/measured flags, fixture hash, commit SHA, dirty state in
the CSV and `run-metadata.json`, host OS/architecture/CPU, SDK version, and full
command.
Warmups remain in the raw CSV but are excluded from `summary.*`.

`parity.json` records both parity process commands, exit codes, elapsed time,
and peak memory. `summary.json` contains the distributions and median speed
ratio. `suite-summary.*` aggregates fixture summaries and explicitly records
that no performance pass/fail threshold was applied.

The scheduled/manual `Preprocess performance` workflow runs the Benchmark
preset on Windows and Linux and uploads the complete tree. It is evidence, not
a flaky regression gate. Semantic property/item evaluation remains in
`compare-evaluation.ps1` and `run-compatibility-fixtures.ps1`; those scripts do
not compare evaluation speed.
