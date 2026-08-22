# Build the binary against the same source the release builds, so an image
# tagged for a version contains that version rather than whatever the base
# image happened to have. release-targets.json owns this exact MSRV; its
# validator keeps the Docker mirror synchronized.
ARG RUST_VERSION=1.90.0
FROM rust:${RUST_VERSION}-slim-bookworm AS build
WORKDIR /src

# Dependencies first, from the manifests alone, so editing sources does not
# rebuild the dependency graph. The dummy targets exist only to give cargo
# something to compile; they are replaced by the real sources below.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
 && echo 'fn main() {}' > src/main.rs \
 && echo '' > src/lib.rs \
 && cargo build --release --locked \
 && rm -rf src

COPY src ./src
COPY examples ./examples
# cargo skips a rebuild when mtimes look unchanged, and the dummy build just
# wrote artifacts for these exact target names.
RUN touch src/main.rs src/lib.rs \
 && cargo build --release --locked \
 && strip target/release/mcp-repl

# ca-certificates is the only runtime dependency: --http talks TLS. Everything
# else the REPL needs is in the binary.
FROM debian:bookworm-slim AS runtime
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# An unprivileged user by default. Nothing here needs root, and a container
# people are told to pipe untrusted server output through should not run as it.
RUN useradd --create-home --uid 10001 mcp
USER mcp
WORKDIR /home/mcp

COPY --from=build /src/target/release/mcp-repl /usr/local/bin/mcp-repl

# No CMD: with no arguments mcp-repl starts a disconnected prompt, which is a
# reasonable landing spot, and `docker run ... --demo` or `... --http URL`
# appends to the entrypoint rather than replacing a default.
ENTRYPOINT ["mcp-repl"]
