# MSBuild Evaluation Compatibility Worklist

This worklist tracks the behavior needed before preprocessing performance can be considered an apples-to-apples comparison with conventional MSBuild. A feature is complete only when focused unit tests and a parity fixture pass against both `dotnet build /pp` and `msbuild-rs --preprocess`.

## Benchmark Baseline

- [x] Measure fresh process startup, project loading, and preprocessed output creation.
- [x] Support configurable warmups and iterations and retain raw CSV samples.
- [x] Add a parity runner that compares normalized preprocessed output for compatibility fixtures.
- [x] Pin the .NET SDK used for repeatable baseline results.
- [ ] Add larger generated fixtures for properties, items, conditions, and import graphs.
- [x] Report wall-clock distribution.
- [ ] Report peak working set.

Run the current baseline after `cargo build --release`:

```powershell
./scripts/compare-preprocess.ps1 -Project ./sample_projects/simple.proj
```

Both implementations now preserve aggregated project source, including unevaluated expressions and source-boundary comments around inlined imports. Semantic compatibility still depends on the evaluation items below.

## Evaluation Semantics

- [x] Use one balanced expression parser for preprocessing, project evaluation, and task execution.

### Properties

- [x] Basic `$(Property)` expansion.
- [x] Evaluate properties in document order with last eligible assignment winning.
- [x] Expand each assignment once against the preceding state, including self,
  before-set, and mutual references; bound actual expression/function nesting
  without interpreting parentheses in literal text.
- [x] Implement global properties and `--property Name=Value` command-line precedence.
- [x] Snapshot environment properties, protect the conventional reserved-name
  set, and synthesize current-file properties only from active evaluation context.
- [x] Preserve lexical project/current-file paths and provide host-resolved
  active .NET SDK and toolset properties during preprocessing.
- [ ] Implement property functions with an explicit allowlist matching MSBuild.
- [x] Implement preprocessing path functions: `GetDirectoryNameOfFileAbove`, `GetPathOfFileAbove`, `MakeRelative`, and `Path.Combine`.
- [ ] Implement registry properties where supported.

#### .NET-backed intrinsic expressions (late-stage)

MSBuild property functions may name .NET types directly. For example:

```xml
$([System.Text.RegularExpressions.Regex]::IsMatch('%(FullPath)', '.+\.css\.aspx'))
```

Support will use two execution tiers:

1. Frequently used types and methods will have native Rust implementations selected by an explicit type-and-method allowlist. This is the preferred path for startup time, throughput, portability, and predictable behavior.
2. Legal MSBuild property-function calls without a native implementation will fall back to a CoreCLR hosted in-process. The fallback will resolve the requested type and method, marshal arguments and return values, and cache runtime/type/method lookup state.

This work is intentionally late in the compatibility plan. The parser, evaluation order, items and metadata, imports, escaping, and native high-value intrinsic set should be stable first. Hosting CoreCLR must not become a prerequisite for projects that only use the native tier.

- [ ] Inventory the .NET types and methods most common in representative evaluated projects.
- [ ] Define the native Rust intrinsic registry and deterministic overload/coercion rules.
- [ ] Define the managed fallback contract, including CoreCLR discovery, startup, invocation, caching, exceptions, and diagnostics.
- [ ] Match MSBuild's allowlist and reject types or members that MSBuild property functions do not permit.
- [ ] Add parity fixtures for static methods, constructors, instance methods, overloads, nested expressions, metadata arguments, null/empty values, and exceptions.
- [ ] Benchmark native and managed tiers separately, including cold CoreCLR startup and warm invocation.
- [ ] Keep CoreCLR unloaded unless a managed fallback is actually required.

### Conditions

- [x] Basic `==` and `!=` comparisons.
- [x] Parse parentheses and boolean `And`/`Or` with MSBuild precedence.
- [x] Support `<`, `>`, `<=`, and `>=` with invariant finite IEEE-754 decimal,
  signed 32-bit hexadecimal, and two-to-four-part version coercion and ordering.
- [x] Support exact MSBuild boolean aliases, case-insensitive comparisons, and
  quoted literals without coercing whitespace or empty strings to booleans.
- [x] Implement `$([MSBuild]::IsOSPlatform(...))` for case-insensitive
  `Windows`, `Linux`, and `OSX` platform names.
- [x] Implement `Exists`, `HasTrailingSlash`, SDK version comparisons, feature-wave checks, and common string predicates.
- [x] Produce errors for malformed conditions instead of treating arbitrary text as true.

The relational and `IsOSPlatform` ports deliberately cover the focused cases
listed in the compatibility matrix, rather than every upstream parser and
coercion permutation. Remaining condition backlog includes legacy
`MSBuildToolsVersion` comparison shims, condition syntax/escaping permutations,
and nonstandard runtime platform identifiers.

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
- [x] Evaluate imports in document order at their source location, including conditions based on the importing project's state.
- [x] Support deterministic preprocessing of import globs and conditional imports.
- [x] Honor `ImportGroup` and child `Import` conditions using the importing
  file's current-file properties and source-position state.
- [x] Detect duplicate imports with normalized lexical full-path identity,
  without resolving symlinks; evaluate each lexical import once.
- [x] Diagnose and skip circular imports by default, matching MSBuild's
  non-`RejectCircularImports` load mode.
- [x] Implement and structurally validate project-level `Choose`, `When`, and
  `Otherwise`, including empty `When` branches, nested choices, properties,
  and items in the selected branch. Every branch is validated even when it is
  not selected.
- [x] Implement ordered implicit `Sdk.props` and `Sdk.targets` imports for
  `Project@Sdk` and top-level `Sdk Name/Version` declarations, with cached
  `dotnet` host discovery that honors `global.json`.
- [x] Preserve an aggregated source representation equivalent to `/pp`.

### Escaping and Parsing

- [ ] Implement MSBuild percent escaping and unescaping.
- [ ] Preserve XML text and attribute semantics, including CDATA and entities.
- [ ] Match case-insensitive property, item, metadata, and function lookup.
- [ ] Match semicolon splitting and empty-value behavior.

## Parity Fixtures

- [ ] Each completed feature has a minimal standalone project fixture.
- [x] Capture queried properties and items from conventional MSBuild for semantic comparison.
- [x] Normalize machine-specific paths before comparing outputs.
- [x] Cover Windows and a non-Windows platform in CI.
- [x] Keep execution/task performance separate from evaluation/preprocessing performance.

The upstream-test mapping and fixture status are maintained in
[the evaluation compatibility matrix](evaluation-compatibility-matrix.md).

## Explicitly deferred property gaps

- `TreatAsLocalProperty` is not implemented. Global properties are therefore
  always immutable during project/import evaluation.
- Lexical absolute local paths, including Windows spelling, are covered. Edge
  UNC normalization remains deferred.
- Version syntax on top-level SDK declarations is retained for installed/custom
  SDK lookup. NuGet acquisition for versioned third-party MSBuild SDKs remains
  deferred.
- Uninitialized-property warning emission is deferred; before-set reads already
  produce the compatible empty value without recursive reevaluation.
- Property lookup is indexed and case-insensitive. Broader item and metadata
  case-insensitivity remains a later wave.
- Import suppression uses normalized lexical full paths (case-insensitive on
  Windows and case-sensitive elsewhere) and deliberately does not resolve
  symlinks. A Windows symlink test is skipped when the process lacks the
  `SeCreateSymbolicLinkPrivilege` privilege. Duplicate-import warning source
  locations and a public strict equivalent to
  `ProjectLoadSettings.RejectCircularImports` remain deferred.
- `Choose` support is limited to project evaluation structure; target-body
  `Choose`/task selection remains deferred.
