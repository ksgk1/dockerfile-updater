FROM lukemathwalker/cargo-chef:latest-rust-1.98.0-alpine3.24 AS chef
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

FROM chef as builder
RUN apk add --no-cache musl musl-utils sccache  zstd && \
    rustup target add x86_64-unknown-linux-musl
COPY --from=planner /app/recipe.json recipe.json
ENV RUSTC_WRAPPER=/usr/bin/sccache
ENV BUILD_FLAGS="--release --target=x86_64-unknown-linux-musl"
RUN cargo chef cook $BUILD_FLAGS --recipe-path recipe.json
COPY . ./
RUN cargo build $BUILD_FLAGS

# Final minimal image
FROM scratch
ENTRYPOINT ["/dockerfile-updater"]
WORKDIR /
COPY --from=builder /app/target/x86_64-unknown-linux-musl/release/dockerfile-updater /.