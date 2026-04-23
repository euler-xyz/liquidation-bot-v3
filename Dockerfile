# ---- Planner Stage ----
FROM rust:1.95-bullseye AS planner

RUN cargo install cargo-chef
WORKDIR /app
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ---- Builder Stage ----
FROM rust:1.95-bullseye AS builder

RUN cargo install cargo-chef
WORKDIR /app

# Build dependencies from the recipe (cached independently of source changes)
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

# Build the actual application
COPY . .
RUN cargo build --release

# ---- Runtime Stage ----
FROM debian:bullseye-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        gnupg \
        apt-transport-https \
    && curl -sLf --retry 3 --tlsv1.2 --proto "=https" \
        'https://packages.doppler.com/public/cli/gpg.DE2A7741A397C129.key' \
        | gpg --dearmor -o /usr/share/keyrings/doppler-archive-keyring.gpg \
    && echo "deb [signed-by=/usr/share/keyrings/doppler-archive-keyring.gpg] https://packages.doppler.com/public/cli/deb/debian any-version main" \
        > /etc/apt/sources.list.d/doppler-cli.list \
    && apt-get update && apt-get install -y --no-install-recommends doppler \
    && apt-get purge -y gnupg apt-transport-https \
    && apt-get autoremove -y \
    && rm -rf /var/lib/apt/lists/*

# Copy the compiled binary from the builder
COPY --from=builder /app/target/release/liquidation-bot-v3 /usr/local/bin/liquidation-bot-v3
# Copy the configuration files for the chains to the working directory.
COPY --from=builder /app/configs/ ./

# Doppler config — override at runtime via env vars or `docker run -e`
ENV DOPPLER_PROJECT=""
ENV DOPPLER_CONFIG=""

# Use a non-root user
RUN useradd --create-home appuser
USER appuser

# Doppler wraps the binary and injects secrets as env vars
ENTRYPOINT ["doppler", "run", "--"]
CMD ["liquidation-bot-v3"]
