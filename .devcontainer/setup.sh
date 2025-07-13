#!/bin/bash

# MSBuild-RS Development Environment Setup Script
# This script is automatically run when the devcontainer is created

set -e

echo "🚀 Setting up MSBuild-RS development environment..."

# Update package lists
echo "📦 Updating package lists..."
sudo apt-get update

# Verify Rust installation
echo "🦀 Verifying Rust installation..."
rustc --version
cargo --version

# Install Rust components if not already installed
echo "🔧 Installing Rust components..."
rustup component add rustfmt clippy rust-analyzer 2>/dev/null || echo "Components already installed"

# Install additional Rust tools for development
echo "🛠️  Installing Rust development tools..."
cargo install --quiet cargo-watch cargo-edit cargo-outdated cargo-audit cargo-tree 2>/dev/null || echo "Tools may already be installed"

# Install code coverage tool
echo "📊 Installing code coverage tools..."
cargo install --quiet cargo-tarpaulin 2>/dev/null || echo "Tarpaulin may already be installed"

# Install useful CLI utilities
echo "🔨 Installing CLI utilities..."
cargo install --quiet exa bat fd-find ripgrep xh 2>/dev/null || echo "CLI tools may already be installed"

# Set up git configuration
echo "🔗 Configuring git..."
git config --global --add safe.directory /workspace
git config --global init.defaultBranch main
git config --global pull.rebase false

# Create useful aliases in .zshrc if not already present
echo "⚙️  Setting up shell aliases..."
if ! grep -q "MSBuild-RS aliases" ~/.zshrc; then
    cat >> ~/.zshrc << 'EOF'

# MSBuild-RS aliases
alias ll="exa -la"
alias la="exa -la"
alias tree="exa --tree"
alias cat="bat"
alias find="fd"
alias grep="rg"
alias http="xh"

# MSBuild-RS development shortcuts
alias msbuild-demo="cargo run -- --demo"
alias msbuild-simple="cargo run -- --project sample_projects/simple.proj --target Build"
alias msbuild-conditional="cargo run -- --project sample_projects/conditional.proj --target Test"
alias msbuild-imports="cargo run -- --project sample_projects/with_imports.proj --target Build"

# Cargo shortcuts
alias cb="cargo build"
alias ct="cargo test"
alias cc="cargo clippy"
alias cf="cargo fmt"
alias cw="cargo watch"
alias cr="cargo run"

export RUST_BACKTRACE=1
export RUST_LOG=info
EOF
    echo "Shell aliases added to ~/.zshrc"
fi

# Verify that sample projects exist
echo "📋 Verifying sample projects..."
if [ -d "/workspace/sample_projects" ]; then
    echo "✅ Sample projects found"
    ls -la /workspace/sample_projects/
else
    echo "⚠️  Sample projects directory not found - this is normal for a fresh clone"
fi

# Build the project to ensure everything works
echo "🔨 Building the project..."
cd /workspace
if cargo build; then
    echo "✅ Build successful!"
else
    echo "❌ Build failed - check dependencies"
    exit 1
fi

# Run tests to verify functionality
echo "🧪 Running tests..."
if cargo test; then
    echo "✅ All tests passed!"
else
    echo "❌ Some tests failed - this may be expected for a development environment"
fi

# Check for security vulnerabilities
echo "🔍 Running security audit..."
cargo audit || echo "⚠️  Security audit found issues or cargo-audit not available"

# Check for outdated dependencies
echo "📦 Checking for outdated dependencies..."
cargo outdated || echo "⚠️  Some dependencies may be outdated or cargo-outdated not available"

# Display helpful information
echo ""
echo "🎉 MSBuild-RS development environment is ready!"
echo ""
echo "📚 Quick start commands:"
echo "  cargo run -- --demo                    # Run demo mode"
echo "  cargo test                             # Run all tests"
echo "  cargo clippy                           # Run linter"
echo "  cargo watch -x build                   # Watch for changes"
echo ""
echo "🔗 Useful aliases:"
echo "  msbuild-demo                           # Run demo mode"
echo "  msbuild-simple                         # Run simple.proj"
echo "  cb, ct, cc, cf                         # Cargo shortcuts"
echo ""
echo "📁 Project structure:"
echo "  src/                                   # Source code"
echo "  sample_projects/                       # Sample MSBuild projects"
echo "  .devcontainer/                         # Development container config"
echo ""
echo "Happy coding! 🚀"
