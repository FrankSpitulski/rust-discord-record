ARG IMAGE_VERSION=1.77.1

FROM rust:${IMAGE_VERSION} as base
RUN apt-get update && apt-get install -y cmake && rm -rf /var/lib/apt/lists/*
WORKDIR app
RUN cargo install cargo-chef

FROM base as planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM base as cacher
COPY --from=planner /app/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

FROM base as builder
COPY . .
# Copy over the cached dependencies
COPY --from=cacher /app/target target
COPY --from=cacher /usr/local/cargo /usr/local/cargo
RUN cargo build --release --bin rust-discord-record

FROM ubuntu:22.04 as runtime
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
WORKDIR app
COPY --from=builder /app/target/release/rust-discord-record /usr/local/bin
ENV DISCORD_TOKEN=""
ENTRYPOINT ["/usr/local/bin/rust-discord-record"]