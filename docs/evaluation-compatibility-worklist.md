# MSBuild Evaluation Compatibility Worklist

This worklist tracks the behavior needed before preprocessing performance can
be considered an apples-to-apples comparison with conventional MSBuild. A
feature is complete only when focused unit tests and a minimal semantic and/or
normalized preprocess fixture pass against dotnet MSBuild and msbuild-rs.

## Benchmark Baseline

- [x] Measure fresh process startup, project loading, and preprocessed output creation.
- [x] Support configurable warmups and iterations and retain raw CSV samples.
- [x] Add a parity runner that compares normalized preprocessed output for compatibility fixtures.
- [x] Pin the .NET SDK used for repeatable baseline results.
- [x] Add larger generated fixtures for properties, items, conditions, and import graphs.
- [x] Report wall-clock distribution.
- [x] Report peak working set.
- [x] Compare target-free finalized property/item evaluation through equivalent
  `-getProperty`/`-getItem` and msbuild-rs query selections.
- [x] Run and summarize simple plus every generated manifest case in both
  preprocess and evaluation-query modes.

The performance-finalization baseline is complete. Generated inputs use a
fixed seed/configuration with per-file, per-fixture, aggregate, and manifest
SHA-256 values. Timed processes are interleaved and record elapsed wall time
and `PeakWorkingSet64`; timing is ineligible unless normalized mode-specific
parity passes first. Both ratios are fresh-process end-to-end CLI measurements,
not in-process library throughput, and no hard CI speed threshold is applied.

Run the current baseline after `cargo build --release`:

```powershell
./scripts/compare-preprocess.ps1 -Project ./sample_projects/simple.proj -Warmup 5 -Iterations 30
./scripts/run-performance-suite.ps1 -Preset Benchmark -Warmup 5 -Iterations 30
```

Both implementations preserve aggregated project source, including unevaluated
expressions and source-boundary comments around inlined imports. See
[the performance benchmarking guide](performance-benchmarking.md) for fixture
scale knobs, artifact schemas, commands, and interpretation.

## Evaluation Semantics

- [x] Use one balanced expression parser for preprocessing, project evaluation, and task execution.

### Properties

- [x] Basic `$(Property)` expansion.
- [x] Evaluate properties in document order with last eligible assignment winning.
- [x] Expand each assignment once against the preceding state, including self,
  before-set, and mutual references; bound actual expression/function nesting
  without interpreting parentheses in literal text.
- [x] Implement global properties and `--property Name=Value` command-line
  precedence, including source-position `TreatAsLocalProperty` union semantics
  across the root project and imports.
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

`TreatAsLocalProperty` is expanded and split when each project/import root is
entered, before that file's implicit top SDK imports. Names are validated,
trimmed, accumulated case-insensitively, and never retroactive: a declaration
in an import permits assignments in that import and subsequent parent content,
but cannot revive a parent assignment skipped above the import. The original
global-property dictionary/value remains the expansion predecessor.

The native registry is initialized once and indexed by normalized type, member,
and invocation kind. Overload arity, parameter/params coercion, and per-overload
null policy are enforced before dispatch. Values retain `null`, arrays, numeric
types, and `System.Char` boundaries through exact nested calls and chains.
Native nulls also retain their descriptor's declared result type, so a missing
`Environment.GetEnvironmentVariable` remains a nullable `System.String` for
later overload selection; an authored `null` literal remains untyped.
String arrays bind as arrays, while `Char` binds only to a retained `Char`
overload and is not silently stringified. Final array rendering discards only
leading empty elements, then preserves interior/trailing empties while escaping
each element and joining with raw semicolons. Integral MSBuild arithmetic is
unchecked/wrapping, and shifts use CLR masks.

The retained correctness-tested core includes ordinal `System.String`
operations (`Copy`, null predicates, `Join`, `CompareOrdinal`, `Contains`,
`Substring`, invariant casing, trim character sets, replace/split/equality,
insertion/removal, length/indexer); host-lexical `System.IO.Path` filename,
extension, root, combine, change-extension, and full-path members; integral
`System.Math.Abs`; typed and radix-based `System.Convert` overloads that do not
consult current culture; all supported `Version` constructor shapes; Guid
N/D/B/P parsing and N/D/B/P/X formatting; invariant Int32/Int64/UInt64 D/X
formatting; and a deliberately narrow ISO DateTime parse plus numeric
custom-format subset. `Environment.ExpandEnvironmentVariables` scans `%name%`
tokens once and preserves missing/malformed tokens. Windows keys use
.NET-compatible Unicode ordinal-ignore-case folding; Unix keys remain
case-sensitive.
The pinned SDK also requires ASCII `StartsWith`/`EndsWith`, integral decimal
spellings such as `10.0` for integral arithmetic overloads, the common
`net`/`netcoreapp`/`netstandard`/short `net4x` TFM projection helpers, and
empty-platform `ToolLocationHelper` probes. Those narrow inputs are retained
and direct-fixture tested; broader culture-sensitive or NuGet compatibility
behavior is not inferred.
Invariant casing follows ICU simple one-scalar mappings plus .NET invariant
special handling for dotted/dotless I, as verified against .NET 10's
[`InvariantModeCasing`](https://github.com/dotnet/runtime/blob/v10.0.10/src/libraries/System.Private.CoreLib/src/System/Globalization/InvariantModeCasing.cs);
the direct fixture covers `ß`, Greek sigma, `İ`, and `ı`.
`StableStringHash` retains Legacy, SHA-256, and the distinct signed-Int32,
UTF-16 `Fnv1a32bit`/`Fnv1a32bitFast` algorithms.

`String.Split()` is retained, while explicit null `Split`, null `String.Join`
separators, and null unary `Path` calls are rejected like direct MSBuild.
`Path.ChangeExtension` retains its null behavior, and
`MSBuild.Unescape(null)` returns empty. File-above searches make relative starts
lexically absolute; `GetPathOfFileAbove` rejects file names containing a host
directory separator.

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

Native-tier backlog (rather than approximate behavior): floating MSBuild
arithmetic overloads, floating `System.Math` entries,
`System.Convert.ToDouble`, culture-sensitive unary
`Convert` string-to-number and number-to-string overloads, and
`System.Double.ToString`; current-culture
`non-ASCII/current-culture `String.StartsWith` and `EndsWith`, plus `CompareTo`,
`IndexOf`, `LastIndexOf`,
`ToLower`, and `ToUpper`; broad DateTime parsing/formatting; Guid equality
binding and Guid X parsing (X formatting remains); Double custom formats; and
unlisted CLR numeric overloads. Floating
spellings including `inf` are rejected because no culture-sensitive floating
overload is retained. `IsTargetFrameworkCompatible` and TFM grammars outside
the explicitly tested projection subset remain pruned rather than approximated;
`Environment.Is64BitOperatingSystem` is likewise pruned because process
bitness is not an OS-bitness answer on every host.
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
missing behavior); unsupported short aliases are rejected. Registry views use
`Enum.Parse`-compatible trimming, signs, leading zeroes, and comma-combined
names, then accept only resulting values `0`/`256`/`512`; typed non-string
objects in the `params object[]` are ignored. Registry strings retain embedded
NULs, and multi-strings retain interior empty elements while removing only
their required terminal NULs. Bounded retries cover `ERROR_MORE_DATA` races in
both size and data query phases.

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
- [x] Define the native Rust intrinsic registry and deterministic
  overload/coercion rules for the retained representative subset.
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
- [x] Implement the remaining evaluation-time item-function surface:
  `GetPathsOfAllDirectoriesAbove` and a deliberately allowlisted subset of
  per-item `System.String` methods/properties.
- [x] Implement indexed, case-insensitive custom metadata and the `Identity`,
  `FullPath`, `RootDir`, `Filename`, `Extension`, `RelativeDir`, `Directory`,
  `RecursiveDir`, and defining-project well-known metadata.
- [x] Implement timestamp well-known metadata (`ModifiedTime`, `CreatedTime`,
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
  directory subtrees, and index exact item mutations without globally
  canonicalizing authored identities.
- [x] Follow directory symlinks during recursive traversal while suppressing
  only canonical identities already present in the active ancestor chain, so
  logical symlink paths remain distinct and cycles terminate.
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

`GetPathsOfAllDirectoriesAbove` normalizes each source item against its project
directory, deduplicates with .NET ordinal-ignore-case semantics, and returns the
sorted ancestor union. Per-item string dispatch is a static allowlist:
`Trim()`, `Trim/TrimStart/TrimEnd(string-as-char-set)`, `Replace`,
`Substring`, `Contains`, ordinal `Equals`, `ToLowerInvariant`,
`ToUpperInvariant`, and `get_Length`. This includes the `Trim`, `TrimStart`,
and `Replace` forms present in the pinned .NET 10 SDK. Current-culture members
are deliberately rejected.

Timestamp metadata follows MSBuild's unusual filesystem rooting: relative item
identities are probed from the process working directory, not the root or
defining project directory; absolute identities are probed directly.
Nonexistent paths and directories return empty values. Values use local host
time in `yyyy-MM-dd HH:mm:ss.fffffff` form. Windows uses creation/write/access
file times; macOS uses birth/modify/access times; Linux uses filesystem birth
time when reported and otherwise, like .NET, synthesizes creation as the older
of change and modification time. Host access-time mount policy is preserved.
Each individual timestamp metadata lookup reads current filesystem state.
`evaluated_metadata` and evaluation-query projections share one local stat
across all three timestamp names. No mutable item cache or read-serializing
lock is retained, so finalized projects remain `Send + Sync`.

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
- [x] Resolve the installed workload virtual SDKs natively from the selected
  host SDK: manifest and pack roots, workload sets/install state, RID aliases,
  installed SDK packs with `AutoImport.props`, and manifest
  `WorkloadManifest.targets`. Resolver state is built at most once per
  evaluation, never invokes a process per import, and never acquires NuGet
  packages.
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

- [x] Each completed feature has a minimal standalone project fixture.
- [x] Capture queried properties and items from conventional MSBuild for semantic comparison.
- [x] Normalize machine-specific paths before comparing outputs.
- [x] Cover Windows and a non-Windows platform in CI.
- [x] Keep execution/task performance separate from evaluation/preprocessing performance.

The upstream-test mapping and fixture status are maintained in
[the evaluation compatibility matrix](evaluation-compatibility-matrix.md).
`scripts/run-compatibility-fixtures.ps1` discovers every semantic and
preprocess manifest; CI runs that same entry point on Windows, Linux, and macOS.
The installed-SDK preprocess fixture uses a semantic XML projection: it compares
the complete expanded element/attribute/text tree while ignoring comments,
layout, namespace serialization, and the equivalent root `Sdk` versus inferred
`DefaultTargets` spelling.

## Explicitly deferred gaps

All non-late-stage checklist entries are complete. The unchecked CoreCLR items
remain intentionally late-stage; the edge and out-of-scope behavior below is
also explicitly deferred.

- Lexical absolute local paths, including Windows spelling, are covered. Edge
  UNC normalization remains deferred.
- Version syntax on top-level SDK declarations is retained for installed/custom
  SDK lookup. NuGet acquisition for versioned third-party MSBuild SDKs remains
  deferred.
- Uninitialized-property warning emission is deferred; before-set reads already
  produce the compatible empty value without recursive reevaluation.
- Item wildcards are intentionally eager. MSBuild's opt-in
  `MsBuildSkipEagerWildCardEvaluationRegexes` lazy representation, synthetic
  `MSBuildItemGlob` items, and `GetAllGlobs` reporting remain deferred. Repeated
  identical eager patterns are cached within one evaluation.
- Per-item .NET string functions intentionally expose only the deterministic
  allowlist documented above. Culture-sensitive `ToUpper`, `ToLower`,
  comparison/search overloads, and every unlisted member remain pruned rather
  than being approximated.
- Native property functions deliberately remain a supported subset of
  MSBuild's legal .NET receiver surface. Regex, URI/culture/time-span,
  directory/file enumeration, ToolLocationHelper, broad numeric/enum
  overloads, current-culture String comparison/search/casing, and full
  culture-sensitive .NET formatting remain deferred to additional exact native
  entries or the untouched late-stage CoreCLR plan. Removed members are not
  approximated by ordinal Rust operations.
- Ordinary installed `Microsoft.NET.Sdk` projects now complete through both
  workload locator SDKs, with direct semantic and preprocess-tree parity for
  reserved/SDK properties and `KnownFrameworkReference` identities. Versioned
  third-party SDK acquisition, missing-workload `MissingWorkloadPack` resolver
  item injection, and the full NuGet TFM compatibility surface remain deferred.
- Item operations still execute in the loader's source-position pass rather
  than MSBuild's separate property/item passes. Consequently, an SDK default
  item glob whose enablement properties are assigned later can be absent even
  though installed-SDK evaluation completes; the SDK fixture intentionally
  gates SDK-defined `KnownFrameworkReference` identities instead of claiming
  implicit `Compile` parity.
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
