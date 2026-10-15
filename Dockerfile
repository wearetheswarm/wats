# syntax=docker/dockerfile:1
#
# wats node image: a static-ish Rust binary on distroless, running as
# non-root, with its data directory on a volume. See docs/deploy-bootstrap.md.
#
#   docker build -t wats:dev .
#   docker run --rm -p 7842:7842/udp -v wats-data:/data wats:dev
#
# Base images are pinned by digest; bump them deliberately.
# Published images use Dockerfile.release instead, which packages the static
# musl binary built by the release workflow.

ARG RUST_IMAGE=rust:1.99-slim-trixie@sha256:2752b332db73fdbb7dc576f06c82ed1f312005784ef913d7e04a28f5f55dc581
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot@sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2

FROM ${RUST_IMAGE} AS build
WORKDIR /src
# Use the image's toolchain rather than downloading the one named in
# rust-toolchain.toml ("stable" plus rustfmt and clippy).
ENV RUSTUP_TOOLCHAIN=1.99.0 \
    CARGO_TERM_COLOR=never
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin wats \
    && install -D -m 0755 target/release/wats /out/usr/local/bin/wats \
    && install -d -m 0700 -o 65532 -g 65532 /out/data

FROM ${RUNTIME_IMAGE}
COPY --from=build /out/ /
# Data directory: keys, events, blobs. Mount a persistent volume on /data.
ENV WATS_DIR=/data/wats \
    WATS_PORT=7842 \
    RUST_LOG=wats=info
VOLUME ["/data"]
EXPOSE 7842/udp
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/wats"]
CMD ["node", "--quiet"]
