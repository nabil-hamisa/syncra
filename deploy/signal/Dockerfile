# Signal server (rendezvous + relay). Build context: the bundle made by
# deploy/signal/bundle.sh (workspace manifest + crates/proto + crates/signal).
FROM rust:1.90-slim-bookworm AS build
WORKDIR /src
COPY . .
# Portable binary: ignore the repo's target-cpu=native (not copied anyway).
ENV RUSTFLAGS=""
RUN cargo build --release -p signal && strip target/release/signal

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 signal
COPY --from=build /src/target/release/signal /usr/local/bin/signal
USER signal
ENV RUST_LOG=info
EXPOSE 7000/udp
ENTRYPOINT ["/usr/local/bin/signal"]
CMD ["--listen", "0.0.0.0:7000"]
