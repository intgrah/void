FROM rustlang/rust:nightly-slim AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock* ./
COPY src ./src

RUN apt-get update && apt-get install -y cmake && rm -rf /var/lib/apt/lists/*
RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/void /usr/local/bin/void

EXPOSE 25

VOLUME ["/data"]

CMD ["void", "serve"]
