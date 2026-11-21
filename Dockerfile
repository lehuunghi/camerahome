FROM rust:1-bookworm AS build
WORKDIR /build
COPY Cargo.toml ./
COPY src ./src
COPY web ./web
RUN cargo build --release
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates rclone && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/target/release/camerahome /usr/local/bin/camerahome
ENTRYPOINT ["/usr/local/bin/camerahome"]
