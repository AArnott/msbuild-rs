# MSBuild-RS Development Container

This devcontainer provides a complete Rust development environment for the MSBuild-RS project with all necessary tools and dependencies pre-installed.

## Features

### Rust Toolchain
- **Rust 1.75** - Latest stable Rust compiler
- **rustfmt** - Code formatting
- **clippy** - Linting and code analysis
- **rust-analyzer** - Language server for IDE support

### Development Tools
- **cargo-watch** - Automatic rebuild on file changes
- **cargo-edit** - Easy dependency management
- **cargo-outdated** - Check for outdated dependencies
- **cargo-audit** - Security vulnerability scanning
- **cargo-tree** - Dependency tree visualization
- **cargo-tarpaulin** - Code coverage analysis

### System Utilities
- **exa** - Modern replacement for `ls`
- **bat** - Syntax-highlighted `cat` replacement
- **fd** - Fast file finder
- **ripgrep** - Fast text search
- **jq** - JSON processor
- **xh** - HTTP client

### VS Code Extensions
- **rust-analyzer** - Rust language support
- **vscode-lldb** - Debugging support
- **crates** - Cargo.toml dependency management
- **even-better-toml** - Enhanced TOML support
- **rust-test-adapter** - Test explorer integration

## Quick Start

### Using VS Code
1. Install the **Dev Containers** extension
2. Open the project folder in VS Code
3. When prompted, click "Reopen in Container"
4. Wait for the container to build (first time only)

### Manual Usage
```bash
# Build the container
docker build -t msbuild-rs-dev .devcontainer

# Run the container
docker run -it -v $(pwd):/workspace msbuild-rs-dev
```

## Development Workflow

### Building and Testing
```bash
# Build the project
cargo build

# Run tests
cargo test

# Run with clippy linting
cargo clippy

# Format code
cargo fmt

# Watch for changes and rebuild
cargo watch -x build

# Run demo mode
cargo run -- --demo

# Check for outdated dependencies
cargo outdated

# Security audit
cargo audit
```

### Useful Aliases
The container includes helpful aliases:
- `ll` / `la` - Enhanced directory listing with `exa`
- `tree` - Directory tree view
- `cat` - Syntax highlighted file viewing with `bat`
- `find` - Fast file finding with `fd`
- `grep` - Fast text search with `ripgrep`

### Environment Variables
- `RUST_BACKTRACE=1` - Enable detailed error backtraces
- `RUST_LOG=debug` - Enable debug logging by default

## Cross-Compilation Support

The container includes targets for cross-compilation:
- `x86_64-unknown-linux-musl` - Static Linux binaries
- `x86_64-pc-windows-gnu` - Windows binaries from Linux
- `aarch64-unknown-linux-gnu` - ARM64 Linux binaries

Example:
```bash
# Build for Windows
cargo build --target x86_64-pc-windows-gnu

# Build static Linux binary
cargo build --target x86_64-unknown-linux-musl
```

## Container Specifications

- **Base Image**: `rust:1.75`
- **User**: `vscode` (non-root)
- **Shell**: Zsh with Oh My Zsh
- **Working Directory**: `/workspace`
- **Ports**: 3000, 8000, 8080 (available for development servers)

## Troubleshooting

### Container Build Issues
- Ensure Docker has sufficient memory (4GB+ recommended)
- Check internet connectivity for downloading dependencies
- Clear Docker cache if builds fail: `docker system prune`

### Permission Issues
- The container runs as the `vscode` user to avoid permission conflicts
- Files created in the container will have proper ownership

### Performance
- Use volume mounts for better file system performance
- Consider using Docker Desktop's file sharing optimization
- The container uses bind mounts with `cached` consistency for better performance

## Customization

### Adding More Tools
Edit the `Dockerfile` to install additional system packages or Rust crates.

### VS Code Settings
Modify the `devcontainer.json` to add more extensions or change VS Code settings.

### Shell Configuration
The container uses Zsh with Oh My Zsh. Customize `~/.zshrc` for additional shell enhancements.
