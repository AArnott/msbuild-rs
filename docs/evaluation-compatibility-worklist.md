# MSBuild Evaluation Compatibility Worklist

This worklist tracks the behavior needed before preprocessing performance can be considered an apples-to-apples comparison with conventional MSBuild. A feature is complete only when focused unit tests and a parity fixture pass against both `dotnet build /pp` and `msbuild-rs --preprocess`.

## Benchmark Baseline

- [x] Measure fresh process startup, project loading, and preprocessed output creation.
- [x] Support configurable warmups and iterations and retain raw CSV samples.
- [ ] Add a parity runner that compares normalized preprocessed output for compatibility fixtures.
- [ ] Pin the .NET SDK used for repeatable baseline results.
- [ ] Add larger generated fixtures for properties, items, conditions, and import graphs.
- [ ] Report wall-clock distribution and peak working set.

Run the current baseline after `cargo build --release`:

```powershell
./scripts/compare-preprocess.ps1 -Project ./sample_projects/simple.proj
```

Both implementations now preserve aggregated project source, including unevaluated expressions and source-boundary comments around inlined imports. Semantic compatibility still depends on the evaluation items below.

## Evaluation Semantics

- [x] Use one balanced expression parser for preprocessing, project evaluation, and task execution.

### Properties

- [x] Basic `$(Property)` expansion.
- [ ] Evaluate properties in document order with last assignment winning.
- [ ] Recursively expand property values with cycle detection.
- [ ] Implement global properties and command-line property precedence.
- [ ] Implement environment and reserved properties such as `MSBuildProjectDirectory`.
- [x] Provide core project/current-file paths and active .NET SDK properties during preprocessing.
- [ ] Implement property functions with an explicit allowlist matching MSBuild.
- [x] Implement preprocessing path functions: `GetDirectoryNameOfFileAbove`, `GetPathOfFileAbove`, `MakeRelative`, and `Path.Combine`.
- [ ] Implement registry properties where supported.

### Conditions

- [x] Basic `==` and `!=` comparisons.
- [x] Parse parentheses and boolean `And`/`Or` with MSBuild precedence.
- [ ] Support relational operators and numeric/version comparisons.
- [x] Support boolean coercion, case-insensitive comparisons, and quoted literals.
- [ ] Implement intrinsic condition functions: `Exists`, `HasTrailingSlash`, and `IsOsPlatform`.
- [x] Implement `Exists`, `HasTrailingSlash`, SDK version comparisons, feature-wave checks, and common string predicates.
- [x] Produce errors for malformed conditions instead of treating arbitrary text as true.

### Items and Metadata

- [x] Basic item includes and `@(ItemType)` expansion.
- [ ] Implement item transforms, including `@(Item->'%(Metadata)')`.
- [ ] Implement custom and well-known item metadata.
- [ ] Implement custom separators in item expressions.
- [ ] Implement `Exclude`, `Remove`, and `Update` operations.
- [ ] Implement wildcard and recursive glob expansion with MSBuild escaping rules.
- [ ] Implement item functions and item-expression chaining.
- [ ] Evaluate item definitions and metadata in MSBuild document order.

### Imports and Project Structure

- [x] Resolve imports relative to the importing file.
- [x] Recursively process imports in document order at their source location.
- [x] Support deterministic import globs and conditional imports.
- [ ] Honor `ImportGroup` conditions.
- [ ] Detect duplicate and cyclic imports with compatible diagnostics.
- [ ] Implement `Choose`, `When`, and `Otherwise`.
- [x] Implement implicit `Sdk.props` and `Sdk.targets` imports for installed .NET SDKs.
- [x] Preserve an aggregated source representation equivalent to `/pp`.

### Escaping and Parsing

- [ ] Implement MSBuild percent escaping and unescaping.
- [ ] Preserve XML text and attribute semantics, including CDATA and entities.
- [ ] Match case-insensitive property, item, metadata, and function lookup.
- [ ] Match semicolon splitting and empty-value behavior.

## Parity Fixtures

- [ ] Each completed feature has a minimal standalone project fixture.
- [ ] Capture queried properties and items from conventional MSBuild for semantic comparison.
- [ ] Normalize machine-specific paths before comparing outputs.
- [ ] Cover Windows and a non-Windows platform in CI.
- [ ] Keep execution/task performance separate from evaluation/preprocessing performance.
