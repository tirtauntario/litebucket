# syntax=docker/dockerfile:1
# Multi-stage build. The runtime image is distroless (glibc + CA certs, no
# shell or package manager), runs as a non-root user, and has one writable
# volume. TLS uses rustls/ring, so no OpenSSL is needed at runtime.
FROM rust:1.97.1-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src src
COPY migrations migrations
# Configuration templates embedded by `storlite config template`.
COPY docs/examples/config.example.toml docs/examples/
COPY deploy/config.toml deploy/
RUN cargo build --release --locked \
 && install -d -m 0700 /out/data

FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /src/target/release/storlite /usr/local/bin/storlite
COPY --from=build --chown=65532:65532 /out/data /data
USER 65532:65532
VOLUME ["/data"]
EXPOSE 9000
HEALTHCHECK --interval=30s --timeout=5s CMD ["/usr/local/bin/storlite", "healthcheck", "--url", "http://127.0.0.1:9001/readyz"]
ENTRYPOINT ["/usr/local/bin/storlite"]
CMD ["serve", "--config", "/etc/storlite/config.toml"]
