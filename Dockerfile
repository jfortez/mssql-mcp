# syntax=docker/dockerfile:1

FROM rust:1.88.0-bookworm AS builder

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY README.md ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates libgcc-s1 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder --chown=10001:10001 /src/target/release/mssql-mcp /usr/local/bin/mssql-mcp

USER 10001:10001
EXPOSE 8765
ENTRYPOINT ["/usr/local/bin/mssql-mcp"]
CMD ["--transport", "http", "--http-listen", "0.0.0.0:8765"]
