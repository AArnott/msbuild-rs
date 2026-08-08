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
- [x] Implement an explicit representative native property-function allowlist
  while preserving MSBuild's receiver restrictions. Representative upstream
  allowed/rejected cases pass; deliberately pruned legal calls are not claimed
  as implemented.
- [x] Implement preprocessing path functions: `GetDirectoryNameOfFileAbove`,
  `GetPathOfFileAbove`, `MakeRelative`, and `Path.Combine`, using host-specific
  lexical .NET path rules rather than filesystem canonicalization.
- [x] Implement read-only registry properties where supported.

The native registry is initialized once and indexed by normalized type, member,
and invocation kind. Overload arity, parameter/params coercion, and per-overload
null policy are enforced before dispatch. Values retain `null`, arrays, numeric
types, and `System.Char` boundaries through chains. Integral MSBuild arithmetic
is unchecked/wrapping, shifts use CLR masks, floating operations retain IEEE
NaN/infinity, and array results escape each element before joining with a raw
semicolon.

The retained correctness-tested core includes ordinal `System.String`
operations (`Copy`, null predicates, `Join`, `CompareOrdinal`, `Contains`,
`Substring`, invariant casing, parameterless trim, replace/split/equality,
insertion/removal, length/indexer); host-lexical `System.IO.Path` filename,
extension, root, combine, change-extension, and full-path members; selected
`System.Math` and `System.Convert` overloads; all supported `Version`
constructor shapes; Guid N/D/B/P/X formatting; invariant Int32/Int64/UInt64
D/X formatting; and a deliberately narrow ISO DateTime parse plus numeric
custom-format subset. `Environment.ExpandEnvironmentVariables` scans `%name%`
tokens once, preserves missing/malformed tokens, and uses Windows-insensitive
or Unix-sensitive lookup against the evaluation environment snapshot.
`StableStringHash` retains Legacy, SHA-256, and the distinct signed-Int32,
UTF-16 `Fnv1a32bit`/`Fnv1a32bitFast` algorithms.

MSBuild version helpers use the upstream `SimpleVersion` grammar and reject
invalid inputs. Feature-wave checks honor the resolved
`MSBuildDisableFeaturesFromVersion` boundary for the pinned MSBuild 18.6
toolset (including wave rounding/clamping). `DoesTaskHostExist` validates
runtime/architecture names, resolves current runtime/architecture, and checks
the active toolset executable. Cross-architecture toolset probing returns a
controlled native-tier error because no alternate toolset path is available.
On Unix, current-host availability follows whether that SDK actually ships an
`MSBuild` apphost. `IsRunningFromVisualStudio` is always false because
msbuild-rs is a standalone host.
Disallowed receivers and members are rejected before argument evaluation.
There is no reflection, subprocess dispatch, or runtime startup.

Native-tier backlog (rather than approximate behavior): current-culture
`String.StartsWith`, `EndsWith`, `CompareTo`, `IndexOf`, `LastIndexOf`,
`ToLower`, and `ToUpper`; broad DateTime parsing/formatting; Guid equality
binding; params-character trim overloads; Double custom formats; and unlisted
CLR numeric overloads. The NuGet-backed MSBuild TFM helpers
(`GetTargetFrameworkIdentifier`, `GetTargetFrameworkVersion`,
`GetTargetPlatformIdentifier`, `GetTargetPlatformVersion`, and
`IsTargetFrameworkCompatible`) were pruned instead of retaining simplified
identifier/version ordering; `Environment.Is64BitOperatingSystem` was likewise
pruned because process bitness is not an OS-bitness answer on every host.
The pinned direct baselines retained in the pruning test are
`net8.0-windows10` platform `10.0`, `net48` platform `0.0`, `net48` compatible
with `netstandard2.0`, and `netcoreapp1.0` incompatible with
`netstandard2.1`.
These legal calls await a maintained NuGet-compatible implementation, an exact
native entry, or the excluded late-stage managed fallback.

On Windows, `$(Registry:...)` and the native MSBuild registry intrinsics are
read-only and support string/expanded-string, signed DWORD/QWORD,
multi-string, binary, and `REG_NONE` values. Property functions retain numbers
and arrays through chains (`CompareTo`, `Length`, indexers, and `GetValue`);
array rendering escapes each element while retaining raw list delimiters.
The legacy `$(Registry:...)` scalar path intentionally differs for string
values: a registry string `A;B` remains a two-item list, while the same string
returned by `GetRegistryValue` is escaped as one string item. Full supported
hive names are accepted (including `HKEY_PERFORMANCE_DATA`'s pseudo-hive
missing behavior); unsupported short aliases are rejected. Registry views
accept the named forms and numeric string forms `0`/`256`/`512`; typed
non-string objects in the `params object[]` are ignored.

Missing-state defaults follow the two distinct MSBuild implementations:
`GetRegistryValue` returns null for a missing key but its supplied default for
a missing value; `GetRegistryValueFromView` retains its default for missing
keys but becomes null after finding an existing key without the value. Supplied
typed defaults retain their type through subsequent chains. A null value name
selects the default registry value. An omitted view retains the supplied
default because MSBuild's synthesized boxed enum is ignored by its string-only
view loop. On non-Windows,
`$(Registry:...)` expands to empty before syntax validation, and both
intrinsics return null/the supplied default before hive or view validation,
matching modern dotnet MSBuild.

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
- [x] Parse literal transforms, multiple item references, custom vector and
  transform separators, whitespace around arrows, and chained transforms/item
  functions while retaining structured source-item metadata and escaping.
- [x] Implement the common evaluation-time intrinsic item functions: `Count`,
  `Reverse`, `Distinct`, `DistinctWithCase`, `Metadata`, `HasMetadata`,
  `WithMetadataValue`, `WithoutMetadataValue`, `AnyHaveMetadataValue`,
  `ClearMetadata`, `Exists`, `DirectoryName`, `Combine`, and the non-timestamp
  well-known item-spec modifiers.
- [ ] Implement the remaining obscure item-function surface:
  `GetPathsOfAllDirectoriesAbove` and a deliberately allowlisted subset of
  per-item `System.String` methods/properties.
- [x] Implement indexed, case-insensitive custom metadata and the `Identity`,
  `FullPath`, `RootDir`, `Filename`, `Extension`, `RelativeDir`, `Directory`,
  `RecursiveDir`, and defining-project well-known metadata.
- [ ] Implement timestamp well-known metadata (`ModifiedTime`, `CreatedTime`,
  and `AccessedTime`).
- [x] Implement custom separators in item expressions.
- [x] Implement ordered evaluation-time `Exclude`, `Remove`, and `Update`,
  including item-expression/transform match inputs, per-item conditions,
  metadata predecessors, and current item-definition defaults on updates.
- [x] Evaluate each item operation condition once before expanding its specs,
  then evaluate all candidates and child metadata against the immutable
  pre-operation item vector and batch-apply the result.
- [x] Reject direct custom and well-known metadata references in item
  operation-level conditions with `MSB4191`/`MSB4190`, while retaining
  current-item metadata context for child metadata conditions.
- [x] Implement deterministic eager `*`, `?`, and recursive `**` item-glob
  expansion from the root project directory, per-Include excludes, recursive
  path metadata, escaped wildcard literals, and platform path/case rules.
- [x] Use the MSBuild wildcard grammar and project-rooted lexical identity
  matching (including absolute/relative and Windows drive/root-relative
  equivalence), preserve authored `./`, `../`, and root-relative glob identity
  separately from normalized traversal paths, prune safely excluded recursive
  directory subtrees, and index exact item mutations without resolving
  symlinks.
- [x] Preserve terminal directory separators so `tree/*/` and `tree/**/`
  enumerate no files, and apply the extensionless-file special case only to
  the exact filename pattern `*.*`.
- [x] Bound exact-mutation identity buckets and item-definition default layers
  across 10,000 remove/include cycles and 10,000 repeated updates.

Pipeline entries now distinguish retained source metadata, cleared metadata,
and source-less scalars. Source-independent stages such as `Combine` can
consume scalars; item-spec modifiers, metadata filters/transforms, `Exists`,
and `DirectoryName` require source-item capability and fail clearly when it is
absent. This preserves empty transform correlation through chained functions,
makes explicit separators atomic, and matches escaped ordinal distinctness
without changing the still-open remaining-item-function checkbox above.
- [x] Preserve escaped wildcard/list syntax until classification so `%2A`, `%3F`,
  `%3B`, and `%25NN` are not reinterpreted; only unescaped `*` and `?` classify
  a specification as a wildcard (`[` is literal).
- [x] Evaluate item definitions and child metadata/conditions in document order,
  with immutable layered defaults and explicit metadata precedence.
- [x] Reject attempts to define reserved well-known metadata on items or item
  definitions, using case-insensitive `MSB4033` diagnostics.

### Imports and Project Structure

- [x] Resolve imports relative to the importing file.
- [x] Resolve relative item identities and path well-known metadata against the
  root project directory while retaining the declaring file for
  `DefiningProject*` metadata.
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

- [x] Implement exactly-once MSBuild percent escaping and unescaping while
  retaining authored escaped provenance through list/wildcard classification
  and metadata/property expansion. Function operands cross into decoded text;
  function results are escaped before expression reinsertion.
- [x] Preserve XML attribute/property/metadata text semantics, including
  entities, CDATA, and whitespace, while rejecting DTD/external entities.
- [x] Match indexed case-insensitive property, item, metadata, and function
  lookup without changing authored order/casing.
- [x] Match expression-aware semicolon splitting and empty/whitespace behavior.

## Parity Fixtures

- [ ] Each completed feature has a minimal standalone project fixture.
- [x] Capture queried properties and items from conventional MSBuild for semantic comparison.
- [x] Normalize machine-specific paths before comparing outputs.
- [x] Cover Windows and a non-Windows platform in CI.
- [x] Keep execution/task performance separate from evaluation/preprocessing performance.

The upstream-test mapping and fixture status are maintained in
[the evaluation compatibility matrix](evaluation-compatibility-matrix.md).

## Explicitly deferred gaps

- `TreatAsLocalProperty` is not implemented. Global properties are therefore
  always immutable during project/import evaluation.
- Lexical absolute local paths, including Windows spelling, are covered. Edge
  UNC normalization remains deferred.
- Version syntax on top-level SDK declarations is retained for installed/custom
  SDK lookup. NuGet acquisition for versioned third-party MSBuild SDKs remains
  deferred.
- Uninitialized-property warning emission is deferred; before-set reads already
  produce the compatible empty value without recursive reevaluation.
- Property, item-type, and metadata lookup are indexed and case-insensitive.
- Timestamp well-known metadata (`ModifiedTime`, `CreatedTime`, and
  `AccessedTime`) remains deferred; all listed non-timestamp metadata is
  available in transforms.
- Item wildcards are intentionally eager. MSBuild's opt-in
  `MsBuildSkipEagerWildCardEvaluationRegexes` lazy representation, synthetic
  `MSBuildItemGlob` items, and `GetAllGlobs` reporting remain deferred. Repeated
  identical eager patterns are cached within one evaluation.
- `GetPathsOfAllDirectoriesAbove` and arbitrary per-item .NET string
  functions remain deferred. String methods will require an explicit safe
  allowlist rather than unrestricted dispatch.
- Native property functions deliberately remain a supported subset of
  MSBuild's legal .NET receiver surface. Regex, URI/culture/time-span,
  directory/file enumeration, ToolLocationHelper, broad numeric/enum
  overloads, current-culture String comparison/search/casing, and full
  culture-sensitive .NET formatting remain deferred to additional exact native
  entries or the untouched late-stage CoreCLR plan. Removed members are not
  approximated by ordinal Rust operations.
- SDK-style evaluation now passes the native property-function and
  semicolon-separated import stages and reaches
  `Microsoft.NET.Sdk.ImportWorkloads.props`. Resolution of the virtual
  `Microsoft.NET.SDK.WorkloadAutoImportPropsLocator` SDK remains a loader
  backlog item, so full SDK project evaluation is not claimed.
- Target-execution-only item mutation features (`KeepDuplicates`,
  `KeepMetadata`, `RemoveMetadata`, `MatchOnMetadata`, and
  `MatchOnMetadataOptions`) are outside evaluation scope.
- Import suppression uses normalized lexical full paths (case-insensitive on
  Windows and case-sensitive elsewhere) and deliberately does not resolve
  symlinks. A Windows symlink test is skipped when the process lacks the
  `SeCreateSymbolicLinkPrivilege` privilege. Duplicate-import warning source
  locations and a public strict equivalent to
  `ProjectLoadSettings.RejectCircularImports` remain deferred.
- `Choose` support is limited to project evaluation structure; target-body
  `Choose`/task selection remains deferred.
