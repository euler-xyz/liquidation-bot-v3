# ---- Chef Stage (shared base with cargo-chef pre-installed) ----
FROM lukemathwalker/cargo-chef:latest-rust-1.95-bookworm AS chef
WORKDIR /app

# ---- Planner Stage ----
FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- Builder Stage ----
FROM chef AS builder
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json
COPY . .
RUN cargo build --release

# ---- Foundry Stage (download prebuilt binaries) ----
FROM debian:bookworm-slim AS foundry
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/*
# Pin a release for reproducibility — bump as needed
ARG FOUNDRY_VERSION=stable
RUN curl -L https://foundry.paradigm.xyz | bash \
    && /root/.foundry/bin/foundryup --install ${FOUNDRY_VERSION}

# ---- Runtime Stage ----
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        gnupg \
        apt-transport-https \
        git \
    && curl -sLf --retry 3 --tlsv1.2 --proto "=https" \
        'https://packages.doppler.com/public/cli/gpg.DE2A7741A397C129.key' \
        | gpg --dearmor -o /usr/share/keyrings/doppler-archive-keyring.gpg \
    && echo "deb [signed-by=/usr/share/keyrings/doppler-archive-keyring.gpg] https://packages.doppler.com/public/cli/deb/debian any-version main" \
        > /etc/apt/sources.list.d/doppler-cli.list \
    && apt-get update && apt-get install -y --no-install-recommends doppler \
    && apt-get purge -y gnupg apt-transport-https \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

# Copy Foundry binaries from the foundry stage
COPY --from=foundry /root/.foundry/bin/forge /usr/local/bin/forge
COPY --from=foundry /root/.foundry/bin/cast /usr/local/bin/cast
COPY --from=foundry /root/.foundry/bin/anvil /usr/local/bin/anvil
COPY --from=foundry /root/.foundry/bin/chisel /usr/local/bin/chisel

# Copy the compiled binary from the builder
COPY --from=builder /app/target/release/liquidation-bot-v3 /usr/local/bin/liquidation-bot-v3
COPY --from=builder /app/configs/ ./

ENV DOPPLER_PROJECT=""
ENV DOPPLER_CONFIG=""

RUN useradd --create-home appuser
USER appuser

ENTRYPOINT ["doppler", "run", "--"]
CMD ["liquidation-bot-v3"]
