# Public synthetic runtime. Add a reviewed harness in a separate image before live use.
ARG RUST_IMAGE=rust:1.97.1-bookworm
FROM ${RUST_IMAGE} AS tools
RUN cargo install --git https://github.com/rtk-ai/rtk.git --rev fde0a8f185945556f51718de0f4c430bb62b3df6 --locked rtk

FROM tools AS build
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN rtk proxy cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git python3 && rm -rf /var/lib/apt/lists/*
COPY --from=build /source/target/release/codex-relay /usr/local/bin/codex-relay
COPY --from=build /usr/local/cargo/bin/rtk /usr/local/bin/rtk
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/codex-relay"]
