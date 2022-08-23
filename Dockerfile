ARG IMAGE_VERSION=1.63.0
FROM rust:${IMAGE_VERSION} as planner
WORKDIR app
# We only pay the installation cost once,
# it will be cached from the second build onwards
# To ensure a reproducible build consider pinning
# the cargo-chef version with `--version X.X.X`
RUN cargo install cargo-chef
COPY . .
RUN cargo chef prepare  --recipe-path recipe.json

FROM rust:${IMAGE_VERSION} as cacher
WORKDIR app
RUN cargo install cargo-chef
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

FROM rust:${IMAGE_VERSION} as builder
WORKDIR app
COPY . .
# Copy over the cached dependencies
COPY --from=cacher /app/target target
COPY --from=cacher /usr/local/cargo /usr/local/cargo
RUN cargo build --release --bin rust-discord-record

FROM rust:${IMAGE_VERSION} as runtime
RUN apt-get update && apt-get install -y libopus0 && rm -rf /var/lib/apt/lists/*
WORKDIR app
COPY --from=builder /app/target/release/rust-discord-record /usr/local/bin
ENV DISCORD_TOKEN=""
ENTRYPOINT ["/usr/local/bin/rust-discord-record"]