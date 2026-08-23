# The index digests keep both base images immutable across architectures.
ARG CARGO_AUDITABLE_VERSION=0.7.5
ARG RUST_BASE=docker.io/library/rust:1.90.0-slim-bookworm@sha256:64232e656c058f4468e8d024e990acff04f0fd5a5c0a88a574dc37773d7325c9
ARG RUNTIME_BASE=docker.io/library/debian:bookworm-slim@sha256:abd67ffcfa541b485a3dff59865ab629aa048a6c613e639d36e7456b0b229241

FROM ${RUST_BASE} AS build
ARG CARGO_AUDITABLE_VERSION
WORKDIR /src

RUN cargo install --locked --version "$CARGO_AUDITABLE_VERSION" cargo-auditable \
 && cargo install --list | \
      grep -Fx "cargo-auditable v$CARGO_AUDITABLE_VERSION:" > /dev/null \
 && command -v cargo-auditable > /dev/null

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
 && cargo auditable build --release --locked \
 && strip target/release/mcp-repl \
 && readelf -SW target/release/mcp-repl | \
      grep -Eq '[[:space:]]\.dep-v0[[:space:]]'

# The pinned Rust base already carries the CA bundle needed by --http. Copying
# that exact file removes an otherwise mutable apt index and package download
# from the release build.
FROM ${RUNTIME_BASE} AS runtime
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt

# Use only deterministic numeric ownership. Creating an account with useradd
# would write the build date into /etc/shadow and make the image vary by day.
RUN install -d -m 0755 -o 10001 -g 10001 /home/mcp
ENV HOME=/home/mcp
USER 10001:10001
WORKDIR /home/mcp

COPY --from=build /src/target/release/mcp-repl /usr/local/bin/mcp-repl

# No CMD: with no arguments mcp-repl starts a disconnected prompt, which is a
# reasonable landing spot, and `docker run ... --demo` or `... --http URL`
# appends to the entrypoint rather than replacing a default.
ENTRYPOINT ["mcp-repl"]
