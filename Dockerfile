# Image for the Torvald worker itself (runs `torvald serve`).
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rustfmt.toml ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* && useradd --system torvald
COPY --from=build /src/target/release/torvald /usr/local/bin/torvald
USER torvald
ENTRYPOINT ["torvald"]
CMD ["serve"]
