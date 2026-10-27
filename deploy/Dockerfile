# syntax=docker/dockerfile:1
# Multi-stage build. The runtime image is distroless (glibc + CA certs, no
# shell or package manager), runs as a non-root user, and has one writable
# volume. TLS uses rustls/ring, so no OpenSSL is needed at runtime.
FROM rust:1.99.0-slim-trixie AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY src src
COPY migrations migrations
# Configuration templates embedded by `litebucket config template`.
COPY docs/examples/config.example.toml docs/examples/
COPY deploy/config.toml deploy/
RUN cargo build --release --locked \
 && install -d -m 0700 /out/data

FROM gcr.io/distroless/cc-debian13:nonroot
LABEL org.opencontainers.image.title="litebucket" \
      org.opencontainers.image.description="Single-host S3-compatible object storage" \
      org.opencontainers.image.source="https://github.com/tirtauntario/litebucket" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
COPY --from=build /src/target/release/litebucket /usr/local/bin/litebucket
COPY --from=build --chown=65532:65532 /out/data /data
USER 65532:65532
# Lets `docker compose exec litebucket litebucket admin ...` find the config.
ENV LITEBUCKET_CONFIG=/etc/litebucket/config.toml
VOLUME ["/data"]
EXPOSE 9000
HEALTHCHECK --interval=30s --timeout=5s CMD ["/usr/local/bin/litebucket", "healthcheck", "--url", "http://127.0.0.1:9001/readyz"]
ENTRYPOINT ["/usr/local/bin/litebucket"]
CMD ["serve"]
