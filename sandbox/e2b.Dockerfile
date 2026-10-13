# Sandbox image the agent works in. Build and register it with the E2B CLI:
#   e2b template create torvald-dev -d sandbox/e2b.Dockerfile
# then run Torvald with E2B_TEMPLATE=torvald-dev.
# Add whatever toolchains your projects need: the agent can only build and test
# what this image can.
FROM e2bdev/base

RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential pkg-config libssl-dev git curl ca-certificates jq ripgrep \
        python3 python3-pip python3-venv \
    && rm -rf /var/lib/apt/lists/*

# Node.js LTS
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && npm install -g pnpm \
    && rm -rf /var/lib/apt/lists/*

# Rust, installed system-wide so the sandbox user can use it
ENV RUSTUP_HOME=/usr/local/rustup CARGO_HOME=/usr/local/cargo PATH=/usr/local/cargo/bin:$PATH
RUN curl -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal --component clippy,rustfmt \
    && chmod -R a+rwX /usr/local/rustup /usr/local/cargo
