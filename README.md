# MSBuild-RS

A MSBuild project reader and executor written in Rust.

## Features

- **XML Parsing**: Reads MSBuild project files (.proj, .csproj, etc.) and parses their structure
- **Object Model**: Maintains properties (name=value pairs) and items (type=name pairs with metadata)
- **Expression Evaluation**: Supports `$(PropertyName)` and `@(ItemType)` syntax for property and item references
- **Conditional Evaluation**: Supports `Condition` attributes on elements for conditional processing
- **Target Dependencies**: Executes targets in dependency order using `DependsOnTargets`
- **Import Support**: Processes `<Import>` elements to include other project files
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

### Evaluation Order

1. **Properties**: All properties are evaluated first, allowing forward references
2. **Items**: Items are evaluated after properties and can reference properties
3. **Targets**: Targets are executed based on dependency order and conditions

## Building

```bash
# Build the project
cargo build --release

# Run tests
cargo test

# Run with sample projects
cargo run -- --demo
```

## Sample Projects

The `sample_projects/` directory contains example MSBuild projects:

- `simple.proj` - Basic project with properties, items, and targets
- `conditional.proj` - Demonstrates conditional evaluation
- `with_imports.proj` - Shows import functionality
- `common.props` - Shared properties file for import example

## Architecture

The project is organized into several modules:

- **`parser`** - XML parsing and project file loading
- **`object_model`** - Data structures for properties, items, and targets
- **`expression`** - Property and item reference evaluation
- **`evaluation`** - Project loading and target execution orchestration
- **`tasks`** - Built-in task implementations
- **`logger`** - Logging configuration

## Limitations

This is a simplified MSBuild implementation focused on core functionality:

- Limited condition expression support (basic equality only)
- No SDK-style projects or automatic imports
- No advanced MSBuild features like item transformations
- Limited task ecosystem (only Message, Copy, Error built-in)
- No parallel target execution
- No incremental build support

## Future Enhancements

- More sophisticated condition parsing
- Additional built-in tasks (Csc, Exec, etc.)
- SDK-style project support
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
