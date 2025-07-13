# Product Requirements Document (PRD): MSBuild-RS

## Overview

MSBuild-RS is a Rust implementation of Microsoft's MSBuild project reader and executor. It provides cross-platform support for parsing, evaluating, and executing MSBuild project files with a focus on security, performance, and maintainability.

## Vision

Create a fast, secure, and lightweight MSBuild-compatible build system that enables concurrent builds and follows proper security principles while maintaining compatibility with standard MSBuild project syntax.

## Core Requirements

### 1. Project File Parsing
- **XML Support**: Parse standard MSBuild XML files (.proj, .csproj, .vbproj, etc.)
- **Elements Supported**:
  - `<Project>` - Root element with DefaultTargets attribute
  - `<PropertyGroup>` - Property definitions with conditional support
  - `<ItemGroup>` - Item collections with metadata support
  - `<Target>` - Build targets with dependencies and conditions
  - `<Import>` - External project file inclusion
  - `<UsingTask>` - Custom task registration (planned)
- **Attribute Support**: All standard MSBuild attributes including `Condition`, `DependsOnTargets`, `Include`, etc.

### 2. Expression Evaluation System
- **Property References**: `$(PropertyName)` syntax with nested evaluation
- **Item References**: `@(ItemType)` syntax with semicolon-separated expansion
- **Conditional Expressions**: Support for `'$(Prop)' == 'Value'` style conditions
- **Forward References**: Properties can reference other properties defined later
- **Security**: Expression evaluation is sandboxed and cannot access system resources

### 3. Execution Model
- **Target Dependencies**: Automatic dependency resolution and execution ordering
- **Conditional Execution**: Target and task conditions evaluated at execution time
- **Project Directory Context**: All tasks execute with proper project directory context
- **Concurrent Build Support**: Multiple projects can be built simultaneously without interference

### 4. Task System Architecture
- **Security Model**: Tasks only access explicitly passed attribute values
- **Execution Context**: Tasks receive `TaskExecutionContext` containing:
  - Pre-evaluated attribute values
  - Project directory path
  - Extensible for future parameters (environment, configuration, etc.)
- **Built-in Tasks**:
  - `Message` - Logging and informational output
  - `Copy` - File copying with relative path support
  - `Error` - Build failure with error messages
- **Extensible Registry**: Plugin architecture for custom tasks (planned)

### 5. Import System
- **File Inclusion**: Support for `<Import Project="..." />` elements
- **Conditional Imports**: Imports can have conditions like `Condition="Exists('file.props')"`
- **Relative Path Resolution**: Import paths resolved relative to importing project
- **Property Merging**: Imported properties merged into main project model

## Technical Architecture

### Module Structure
```
src/
├── main.rs           # CLI entry point and demo mode
├── evaluation.rs     # Project loading and target execution orchestration
├── object_model.rs   # Core data structures (ProjectModel, Target, Task, Item)
├── parser.rs         # XML parsing and project file loading
├── expression.rs     # Property/item reference evaluation and conditions
├── tasks.rs          # Task execution system and built-in tasks
├── logger.rs         # Logging configuration and setup
└── tests.rs          # Integration tests
```

### Key Data Structures

#### ProjectModel
```rust
pub struct ProjectModel {
    pub properties: IndexMap<String, String>,
    pub items: IndexMap<String, Vec<Item>>,
    pub targets: IndexMap<String, Target>,
    pub imports: Vec<Import>,
    pub using_tasks: HashMap<String, String>,
    pub project_file_path: Option<PathBuf>,
}
```

#### TaskExecutionContext
```rust
pub struct TaskExecutionContext {
    pub attributes: HashMap<String, String>,  // Pre-evaluated attribute values
    pub project_directory: PathBuf,           // Project directory for relative paths
}
```

### Security Principles
1. **Task Isolation**: Tasks cannot access the full ProjectModel, only passed attributes
2. **Expression Sandboxing**: Property/item evaluation cannot execute arbitrary code
3. **Path Security**: All file operations are restricted to project directory context
4. **No System Access**: Tasks cannot directly access environment variables or system APIs

### Performance Characteristics
- **Single Pass Parsing**: XML parsed once with intelligent element handling
- **Lazy Evaluation**: Expressions evaluated only when needed
- **Minimal Memory**: Efficient data structures using IndexMap for ordered properties
- **Concurrent Safe**: Architecture supports multiple simultaneous builds

## Dependencies

### Core Dependencies
- `quick-xml = "0.31"` - Fast XML parsing
- `clap = { version = "4.4", features = ["derive"] }` - CLI argument parsing
- `anyhow = "1.0"` - Error handling
- `log = "0.4"` + `env_logger = "0.10"` - Configurable logging
- `regex = "1.10"` - Expression pattern matching
- `indexmap = "2.0"` - Ordered hash maps for properties
- `serde = { version = "1.0", features = ["derive"] }` - Serialization support

### Development Dependencies
- `tempfile = "3.8"` - Temporary file creation for tests

## Usage Scenarios

### Command Line Interface
```bash
# Execute specific project and target
msbuild-rs --project MyApp.csproj --target Build

# Verbose logging
msbuild-rs --project MyApp.csproj --target Clean --verbose

# Demo mode with sample projects
msbuild-rs --demo
```

### Project File Example
```xml
<?xml version="1.0" encoding="utf-8"?>
<Project DefaultTargets="Build">
  <PropertyGroup>
    <Configuration Condition="'$(Configuration)' == ''">Debug</Configuration>
    <OutputPath>bin/$(Configuration)/</OutputPath>
  </PropertyGroup>

  <ItemGroup>
    <Compile Include="**/*.cs" />
  </ItemGroup>

  <Target Name="Build" DependsOnTargets="Compile">
    <Message Text="Build completed for $(Configuration)" />
  </Target>

  <Target Name="Compile">
    <Copy SourceFiles="@(Compile)" DestinationFolder="$(OutputPath)" />
  </Target>
</Project>
```

## Testing Strategy

### Test Categories
1. **Unit Tests**: Individual module functionality
2. **Integration Tests**: End-to-end project execution
3. **Sample Projects**: Real-world project examples

### Sample Projects
- `simple.proj` - Basic properties, items, and targets
- `conditional.proj` - Conditional evaluation scenarios
- `with_imports.proj` - Import functionality testing
- `common.props` - Shared properties for import testing

### Test Coverage Requirements
- All expression evaluation scenarios
- Target dependency resolution
- Conditional logic (properties, items, targets, tasks)
- File path resolution (absolute vs relative)
- Error handling and edge cases

## Compatibility

### MSBuild Compatibility
- **Supported**: Core MSBuild syntax and semantics
- **Evaluation Order**: Properties → Items → Targets (following MSBuild model)
- **Condition Evaluation**: Runtime evaluation during execution phase
- **Path Handling**: MSBuild-compatible relative path resolution

### Platform Support
- **Windows**: Primary development platform with PowerShell integration
- **Linux/macOS**: Cross-platform Rust ensures compatibility
- **File Paths**: Platform-agnostic path handling using `std::path`

## Limitations & Future Considerations

### Current Limitations
1. **Task Scope**: Limited to built-in tasks (Message, Copy, Error)
2. **Import Complexity**: Simplified import processing vs full MSBuild
3. **Custom Tasks**: No custom task assembly loading yet
4. **Property Functions**: No MSBuild property function support
5. **Wildcard Items**: Basic item inclusion, no advanced wildcards

### Future Enhancements
1. **Custom Task Loading**: Assembly loading and task discovery
2. **Property Functions**: String manipulation and utility functions
3. **Advanced Wildcards**: Complex file pattern matching
4. **NuGet Integration**: Package reference support
5. **IDE Integration**: Language server protocol support
6. **Performance Optimizations**: Incremental builds and caching

## Success Metrics

### Functional Metrics
- ✅ Parse and execute all sample projects correctly
- ✅ Handle conditional logic properly
- ✅ Support target dependencies and execution order
- ✅ Maintain security model (tasks cannot access full project)
- ✅ Provide proper project directory context

### Quality Metrics
- ✅ Zero compilation warnings
- ✅ 100% test pass rate
- ✅ Clean error handling with meaningful messages
- ✅ Proper logging throughout execution

### Performance Metrics
- ✅ Fast startup time (< 1 second for demo mode)
- ✅ Efficient memory usage with IndexMap
- ✅ Support for concurrent builds

## Maintenance Guidelines

### Code Quality Standards
1. **Error Handling**: Use `anyhow::Result` consistently
2. **Logging**: Use `log` crate with appropriate levels
3. **Documentation**: Public APIs must have doc comments
4. **Testing**: All new features require tests
5. **Security**: Follow task isolation principles

### Architectural Principles
1. **Separation of Concerns**: Keep parsing, evaluation, and execution separate
2. **Security First**: Tasks only access passed parameters
3. **MSBuild Compatibility**: Follow MSBuild semantics where possible
4. **Extensibility**: Design for future enhancement
5. **Performance**: Optimize for build speed and memory usage

### Adding New Features
1. **Tasks**: Implement `TaskExecutor` trait with security model
2. **Elements**: Extend parser and object model consistently
3. **Expressions**: Add new syntax to `ExpressionEvaluator`
4. **Tests**: Always include integration tests with sample projects

This PRD serves as the definitive guide for understanding, maintaining, and extending MSBuild-RS while preserving its core design principles and security model.
