FROM rust:bookworm AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock build.rs ./
COPY src src
COPY crates crates
COPY tools tools
COPY LICENSE LICENSE
RUN cargo build --release --locked --bin ninerouter

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates ffmpeg \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 1000 --user-group --create-home ninerouter \
    && install -d -o ninerouter -g ninerouter /app/data
COPY --from=build /app/target/release/ninerouter /usr/local/bin/9router
USER ninerouter
ENV DATA_DIR=/app/data
EXPOSE 20128
ENTRYPOINT ["/usr/local/bin/9router"]
CMD ["--host", "0.0.0.0", "--no-browser", "--log"]
