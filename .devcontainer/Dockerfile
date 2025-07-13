# Use the official Rust image as the base
FROM rust:1.75

# Set the working directory
WORKDIR /workspace

# Install additional system dependencies
RUN apt-get update && apt-get install -y \
    # Essential build tools
    build-essential \
    cmake \
    pkg-config \
    # SSL/TLS support
    libssl-dev \
    # XML parsing support
    libxml2-dev \
    # Git and development tools
    git \
    curl \
    wget \
    unzip \
    # Process and file utilities
    htop \
    tree \
    jq \
    # Network tools for debugging
    net-tools \
    iputils-ping \
    # Text editors
    nano \
    vim \
    # Shell enhancements
    zsh \
    fish \
    && rm -rf /var/lib/apt/lists/*

# Install Rust components and tools
RUN rustup component add \
    # Code formatting
    rustfmt \
    # Linting
    clippy \
    # Language server
    rust-analyzer

# Install additional Rust tools for development
RUN cargo install \
    # Enhanced cargo commands
    cargo-watch \
    cargo-edit \
    cargo-outdated \
    cargo-audit \
    cargo-tree \
    # Testing tools
    cargo-tarpaulin \
    # Documentation tools
    cargo-readme \
    # Performance tools
    cargo-flamegraph \
    # Cross-compilation support
    cargo-cross

# Install useful development utilities
RUN cargo install \
    # Alternative to 'ls' with better formatting
    exa \
    # Alternative to 'cat' with syntax highlighting
    bat \
    # Fast file finder
    fd-find \
    # Fast grep alternative
    ripgrep \
    # JSON processor
    jq \
    # HTTP client
    xh

# Create a non-root user for development
RUN useradd -m -s /bin/zsh vscode \
    && usermod -aG sudo vscode \
    && echo "vscode ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

# Set up shell for the vscode user
USER vscode

# Install Oh My Zsh for better shell experience
RUN sh -c "$(curl -fsSL https://raw.github.com/ohmyzsh/ohmyzsh/master/tools/install.sh)" || true

# Configure the shell with useful aliases and environment
RUN echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc \
    && echo 'alias ll="exa -la"' >> ~/.zshrc \
    && echo 'alias la="exa -la"' >> ~/.zshrc \
    && echo 'alias tree="exa --tree"' >> ~/.zshrc \
    && echo 'alias cat="bat"' >> ~/.zshrc \
    && echo 'alias find="fd"' >> ~/.zshrc \
    && echo 'alias grep="rg"' >> ~/.zshrc \
    && echo 'alias http="xh"' >> ~/.zshrc \
    && echo 'export RUST_BACKTRACE=1' >> ~/.zshrc \
    && echo 'export RUST_LOG=debug' >> ~/.zshrc

# Set up Rust environment for the user
RUN echo 'source $HOME/.cargo/env' >> ~/.zshrc

# Install additional Rust targets for cross-compilation
RUN rustup target add \
    x86_64-unknown-linux-musl \
    x86_64-pc-windows-gnu \
    aarch64-unknown-linux-gnu

# Expose common development ports (can be customized per project)
EXPOSE 3000 8000 8080

# Switch back to root for final setup
USER root

# Set proper ownership for the workspace
RUN chown -R vscode:vscode /workspace

# Switch back to vscode user
USER vscode

# Set the default command
CMD ["/bin/zsh"]
