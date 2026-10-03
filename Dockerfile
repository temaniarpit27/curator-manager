# Both stages use Debian trixie, so the binary runs against the same glibc
# it was built with.

# Build stage: compile a release binary with the latest stable Rust.
FROM rust:1-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

# Run stage: just the binary and the supplied data.
FROM debian:trixie-slim
WORKDIR /app
COPY --from=build /src/target/release/curator-manager /usr/local/bin/curator-manager
COPY data ./data
# With no arguments it reads data/events.jsonl and data/policy.json. To use
# other files, mount them and pass --events and --policy.
ENTRYPOINT ["curator-manager"]
