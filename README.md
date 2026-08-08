# MSBuild-RS

[![CI](https://github.com/your-username/msbuild-rs/workflows/CI/badge.svg)](https://github.com/your-username/msbuild-rs/actions/workflows/ci.yml)
[![Security](https://github.com/your-username/msbuild-rs/workflows/Security/badge.svg)](https://github.com/your-username/msbuild-rs/actions/workflows/security.yml)
[![codecov](https://codecov.io/gh/your-username/msbuild-rs/branch/main/graph/badge.svg)](https://codecov.io/gh/your-username/msbuild-rs)

A MSBuild project reader and executor written in Rust.

## Features

- **XML Parsing**: Reads MSBuild project files (.proj, .csproj, etc.) and parses their structure
- **Object Model**: Maintains properties (name=value pairs) and items (type=name pairs with metadata)
- **Expression Evaluation**: Supports `$(PropertyName)` and `@(ItemType)` syntax for property and item references
- **Conditional Evaluation**: Supports `Condition` attributes on elements for conditional processing
- **Target Dependencies**: Executes targets in dependency order using `DependsOnTargets`
- **Import Support**: Processes `<Import>` elements to include other project files
- **SDK Imports**: Resolves the active .NET SDK through the `dotnet` host and
  supports `Project@Sdk` plus top-level `<Sdk Name="..." Version="..." />`
- **Built-in Tasks**:
  - `<Message>` - Logs messages to output
  - `<Copy>` - Copies files from source to destination
  - `<Error>` - Logs errors and fails the build
- **Logging**: Configurable logging with stdout output by default

## Usage

### Command Line Options

```bash
# Run a specific project and target
msbuild-rs --project path/to/project.proj --target Build

# Run with verbose logging
msbuild-rs --project path/to/project.proj --target Build --verbose

# Run demonstration with sample projects
msbuild-rs --demo

# Load and write an evaluated project without executing targets
msbuild-rs --project path/to/project.proj --preprocess out.xml

# Query evaluated properties and item identities/metadata without executing targets
msbuild-rs --project path/to/project.proj --get-property Configuration --get-item Compile

# Supply immutable global properties (repeat --property as needed)
msbuild-rs --project path/to/project.proj --property Configuration=Release --get-property Configuration
```

### Project File Format

MSBuild-RS supports standard MSBuild XML syntax:

```xml
<?xml version="1.0" encoding="utf-8"?>
<Project DefaultTargets="Build">

  <!-- Property definitions -->
  <PropertyGroup>
    <Configuration Condition="'$(Configuration)' == ''">Debug</Configuration>
    <OutputPath>bin/$(Configuration)/</OutputPath>
  </PropertyGroup>

  <!-- Item definitions -->
  <ItemGroup>
    <Compile Include="Program.cs" />
    <Compile Include="Utils.cs" />
  </ItemGroup>

  <!-- Target definitions -->
  <Target Name="Build" DependsOnTargets="Compile">
    <Message Text="Build completed for $(Configuration)" />
  </Target>

  <Target Name="Compile">
    <Message Text="Compiling @(Compile) to $(OutputPath)" />
    <Copy SourceFiles="@(Compile)" DestinationFolder="$(OutputPath)" />
  </Target>

</Project>
```

### Supported Elements

#### PropertyGroup
Defines properties that can be referenced elsewhere:
```xml
<PropertyGroup>
  <Configuration>Debug</Configuration>
  <Platform>x64</Platform>
</PropertyGroup>
```

#### ItemGroup
Defines items with optional metadata:
```xml
<ItemGroup>
  <Compile Include="Program.cs" />
  <Content Include="readme.txt" />
</ItemGroup>
```

#### Target
Defines build targets with dependencies:
```xml
<Target Name="Build" DependsOnTargets="Compile" Condition="'$(Configuration)' == 'Debug'">
  <Message Text="Building..." />
</Target>
```

#### Import
Includes other project files:
```xml
<Import Project="common.props" Condition="Exists('common.props')" />
```

#### Tasks
Built-in tasks for common operations:

**Message Task:**
```xml
<Message Text="Hello $(Configuration)!" />
```

**Copy Task:**
```xml
<Copy SourceFiles="source.txt" DestinationFolder="output/" />
```

**Error Task:**
```xml
<Error Text="Build failed!" Condition="'$(Configuration)' == 'Invalid'" />
```

### Expression Syntax

- **Property References**: `$(PropertyName)` - Expands to the property value
- **Item References**: `@(ItemType)` - Expands to semicolon-separated list of item names
- **Conditions**: Support basic equality comparisons like `'$(Prop)' == 'Value'`

MSBuild also permits property functions that reference .NET types, such as:

```xml
$([System.Text.RegularExpressions.Regex]::IsMatch('%(FullPath)', '.+\.css\.aspx'))
```

The compatibility plan uses an explicit, correctness-tested native Rust
allowlist and, later, an in-process CoreCLR fallback for other legal MSBuild
property functions. The native tier preserves typed overloads, null, params
arrays, Char, and array-result boundaries across exact nested calls. On
Windows, environment lookup uses Unicode ordinal-ignore-case keys, while
read-only registry intrinsics preserve DWORD/QWORD numbers, embedded string
NULs, and multi-string/byte arrays through member chains; `REG_NONE` uses the
same byte-list model as `REG_BINARY`. Culture-sensitive floating MSBuild
arithmetic and Math/Double/Convert overloads, Guid X parsing (X formatting
remains), current-culture String members, broad CLR formatting, NuGet TFM
helpers, and OS-bitness queries are deliberately pruned rather than
approximated. These
legal calls require a future exact native implementation or the currently
excluded managed fallback; see the compatibility worklist.
CoreCLR will load lazily so it does not affect projects that stay on native fast
paths.

### Evaluation Order

1. **Properties and imports**: Evaluated in expanded XML document order. Each
   assignment sees the state immediately before it; a before-set reference is empty.
2. **Items**: Added at their source position using the property/item state then visible.
3. **Targets**: Collected in document order and executed by dependency order.

## Building

```bash
# Build the project
cargo build --release

# Run tests
cargo test

# Run with sample projects
cargo run -- --demo

# Compare preprocessing startup and evaluation performance
cargo build --release
./scripts/compare-preprocess.ps1 -Project ./sample_projects/simple.proj

# Also retain raw and normalized preprocess output and report the first mismatch
./scripts/compare-preprocess.ps1 -Project ./sample_projects/simple.proj -CompareOutput

# Compare a semantic evaluation fixture with dotnet msbuild without running targets
cargo build
./scripts/compare-evaluation.ps1 -Fixture ./fixtures/evaluation/basic/fixture.json
```

`global.json` pins the .NET SDK used by the comparison scripts and CI. The
semantic runner writes raw tool output and deterministic, path-normalized JSON
to `benchmark-results/evaluation`; fixtures with `expectedFailure` compare
controlled rejection diagnostics and require both evaluators to fail.

## Sample Projects

The `sample_projects/` directory contains example MSBuild projects:

- `simple.proj` - Basic project with properties, items, and targets
- `conditional.proj` - Demonstrates conditional evaluation
- `with_imports.proj` - Shows import functionality
- `common.props` - Shared properties file for import example

## Architecture

The project is organized into several modules:

- **`loader`** - Order-preserving XML/import/SDK evaluation and preprocessing
- **`object_model`** - Finalized properties, items, and targets
- **`properties`** - Indexed case-insensitive property and reserved-path support
- **`expression`** - Property and item reference evaluation
- **`evaluation`** - Project loading and target execution orchestration
- **`tasks`** - Built-in task implementations
- **`logger`** - Logging configuration

## Limitations

This is a simplified MSBuild implementation focused on core functionality:

- Limited condition expression support (basic equality only)
- SDK evaluation is limited to the implemented expression/item surface; versioned
  third-party SDK acquisition through NuGet is not yet supported
- No advanced MSBuild features like item transformations
- Limited task ecosystem (only Message, Copy, Error built-in)
- No parallel target execution
- No incremental build support

## Future Enhancements

- More sophisticated condition parsing
- Additional built-in tasks (Csc, Exec, etc.)
- Broader SDK-style project evaluation
- Item transformation syntax
- Parallel execution
- Incremental builds
- Plugin system for custom tasks

## Development Environment

### Using Dev Containers (Recommended)

The easiest way to get started with MSBuild-RS development is using the provided dev container:

1. **Prerequisites**: Install [VS Code](https://code.visualstudio.com/) and [Docker](https://www.docker.com/)
2. **Install Extension**: Add the [Dev Containers](https://marketplace.visualstudio.com/items?itemName=ms-vscode-remote.remote-containers) extension
3. **Open Project**: Open the project folder in VS Code
4. **Start Container**: When prompted, click "Reopen in Container"
5. **Wait for Setup**: The container will build and configure automatically

The dev container includes:
- **Rust toolchain** with rustfmt, clippy, and rust-analyzer
- **Development tools** like cargo-watch, cargo-audit, cargo-outdated
- **VS Code extensions** for Rust development, debugging, and testing
- **Shell enhancements** with Zsh, Oh My Zsh, and useful aliases
- **Cross-compilation** targets for Windows and ARM64

### Manual Setup

If you prefer a local development environment:

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install development tools
cargo install cargo-watch cargo-edit cargo-outdated cargo-audit

# Clone and build
git clone <repository-url>
cd msbuild-rs
cargo build
```

### Quick Development Commands

```bash
# Build and test
cargo build
cargo test

# Run demo mode
cargo run -- --demo

# Run specific project
cargo run -- --project sample_projects/simple.proj --target Build

# Watch for changes
cargo watch -x build

# Lint and format
cargo clippy
cargo fmt

# Security audit
cargo audit
```

## Continuous Integration

MSBuild-RS uses GitHub Actions for continuous integration and deployment:

### CI Pipeline
- **Multi-platform testing**: Linux, Windows, macOS
- **Multiple Rust versions**: Stable, beta, nightly, and MSRV (1.70.0)
- **Code quality**: Formatting, linting with Clippy
- **Security scanning**: Vulnerability audits and dependency checks
- **Documentation**: Automated doc generation and validation
- **Integration testing**: Sample project execution and parsing validation

### Release Process
- **Automated releases**: Triggered by version tags
- **Cross-platform binaries**: Linux (glibc/musl), Windows, macOS (x64/ARM64)
- **Changelog generation**: Automatic from git commits
- **Crates.io publishing**: Automated package publishing

### Security
- **Daily security scans**: Automated vulnerability detection
- **Dependency review**: License and security validation for PRs
- **Supply chain security**: cargo-deny configuration for dependency policies

### Dependency Management
- **Renovate Bot**: Automated dependency updates with intelligent scheduling
- **Security-first**: Immediate updates for security vulnerabilities
- **Grouped updates**: Related dependencies updated together
- **Release confidence**: Updates include adoption and compatibility metrics
- **Lock file maintenance**: Automated Cargo.lock updates

### Development Workflow
```bash
# All checks run automatically on PR
git checkout -b feature/my-feature
# ... make changes ...
git push origin feature/my-feature
# Create PR - CI will run automatically

# Release process
git tag v0.2.0
git push origin v0.2.0
# Release workflow runs automatically
```
