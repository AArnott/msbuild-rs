# Evaluation Compatibility Matrix

This matrix links each worklist area (except the deliberately late CoreCLR
section) to the upstream `dotnet/msbuild` test that informed it. “Representative”
means the fixture intentionally covers a small supported subset; it does not
claim full upstream parity.

| Area | Upstream test file + method | Local port / fixture | Status | Notes |
| --- | --- | --- | --- | --- |
| Basic property expansion | — | `fixtures/evaluation/basic` (`Base`, `Derived`) | partial | Covers only a property that refers to an earlier assignment. `UsePropertyBeforeSet` and document-order semantics remain backlog. |
| Property conditions | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `UsePropertyInCondition` | `fixtures/evaluation/basic` (`ConditionalValue`) | representative | Exercises a true property condition. |
| Last assignment / document order | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `EmptyPropertyIsThenSet` | — | backlog | Needs a parity fixture after evaluator ordering is completed. |
| Recursive properties and cycles | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `SetPropertyToItself` | — | backlog | Query projection expands supported references; cycle handling remains incomplete. |
| Global-property precedence | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `VerifyGlobalPropertyOverridesIfNoTreatAsLocalProperty` | — | backlog | Command-line global properties are not implemented. |
| Environment properties | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `EmptyPropertyIsThenSetEnvironmentVariableNotSet` | — | backlog | No environment-property projection yet. |
| Reserved current-file properties | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `MSBuildThisFileProperties` | — | representative | Preprocessor supplies core path properties; evaluator parity remains broader work. |
| Path functions | `src/Build.UnitTests/Evaluation/Expander_Tests.cs` — `PropertyFunctionStaticMethodGetPathOfFileAbove` | `src/expression.rs` tests | ported | Native preprocessing path-function coverage. |
| Registry properties | `src/Build.UnitTests/Evaluation/Expander_Tests.cs` — `RegistryPropertyString` | — | backlog | Platform-dependent behavior. |
| Boolean condition evaluation | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `VerifyConditionsInsideOutsideTargets` | `src/expression.rs` tests | ported | Includes grouping and `And`/`Or` precedence. |
| Condition diagnostics | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `BadConditional` | `src/expression.rs` tests | ported | Malformed conditions are errors. |
| `Exists` and `HasTrailingSlash` | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `UsePropertyInCondition` | `src/expression.rs` tests | ported | `IsOsPlatform` remains backlog. |
| Relational/version comparisons | `src/Build.UnitTests/Evaluation/Expander_Tests.cs` — `PropertyFunctionVersionComparisons` | — | backlog | Only supported SDK-version checks are present. |
| Basic item include and identity | `src/Build.UnitTests/Evaluation/Expander_Tests.cs` — `ItemIncludeContainsMultipleItemReferences` | `fixtures/evaluation/basic` (`Compile`, `Content`) | representative | Preserves item identity and order. |
| Basic custom metadata projection | `src/Build.UnitTests/Evaluation/Expander_Tests.cs` — `HasMetadata` | `fixtures/evaluation/basic` (`Compile.Kind`) | representative | Does not imply well-known metadata parity. |
| Item transforms and separators | `src/Build.UnitTests/Evaluation/Expander_Tests.cs` — `ExpandItemVectorFunctionsItemSpecModifier` | — | backlog | No transform or custom separator support. |
| `Exclude`, `Remove`, and `Update` | `src/Build.UnitTests/Evaluation/ItemEvaluation_Tests.cs` — `RemoveRespectsItemTransform` | — | backlog | Operations are not implemented. |
| Wildcards and recursive globs | `src/Build.UnitTests/Evaluation/ItemGlobs_Tests.cs` — `DocumentOrderIsPreserved` | — | backlog | Escaping and lazy glob behavior remain. |
| Item definition ordering | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `ItemDefinitionPredecessorToItem` | — | backlog | Item definitions are not evaluated. |
| Relative import resolution | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `VerifyLoadingImportScenarios` | `fixtures/evaluation/basic/values.props` | partial | Covers loading one relative import only; source-location ordering and conditions that depend on parent project state remain backlog. |
| Import duplicates/cycles | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `RejectCircularImportsWithCircularImports` | — | backlog | Preprocessor detects cycles; evaluator diagnostics need parity. |
| Conditional import and import glob | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `ImportWildcardsRelative` | `src/preprocess.rs` tests | partial | Deterministic preprocessing only; evaluator import ordering and parent-state conditions remain backlog. |
| `ImportGroup` conditions | `src/Build.UnitTests/Evaluation/Preprocessor_Tests.cs` — `ImportGroupDoubleChildPlusCondition` | — | backlog | Not yet honored. |
| `Choose` / `When` / `Otherwise` | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `VerifyConditionsInsideOutsideTargets` | — | backlog | Project-structure selection is not implemented. |
| Implicit SDK imports | `src/Build.UnitTests/Evaluation/ProjectSdkImplicitImport_Tests.cs` — `SdkImportsAreInLogicalProject` | — | backlog | The preprocessor has focused synthetic tests; an installed-SDK parity fixture is deferred because current SDK props use unsupported property methods. |
| Preprocessed aggregate source | `src/Build.UnitTests/Evaluation/Preprocessor_Tests.cs` — `Single` | `scripts/compare-preprocess.ps1` | representative | Raw and line-normalized outputs are retained. |
| Percent escaping | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `EscapableCharactersInImportPath` | — | backlog | No MSBuild-compatible escaping layer. |
| XML text, CDATA, and entities | `src/Build.UnitTests/Evaluation/Preprocessor_Tests.cs` — `CData` | `src/preprocess.rs` tests | representative | XML handling is not yet full semantic parity. |
| Case-insensitive lookup | `src/Build.UnitTests/Evaluation/Evaluator_Tests.cs` — `ItemPredecessorToItemWithCaseChange` | `src/object_model.rs` tests | representative | Properties and queried item types are case-insensitive. |
| Semicolon splitting and empty values | `src/Build.UnitTests/Evaluation/ExpressionShredder_Tests.cs` — `EmptyEntriesRemoved` | `src/parser.rs` tests | representative | Full escaping behavior remains backlog. |
