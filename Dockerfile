FROM rust:1.99.0-bookworm AS build

RUN apt-get update \
    && apt-get install --no-install-recommends -y cmake pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY migrations ./migrations
COPY web ./web
RUN cargo build --locked --release --bin home-maintenance

FROM debian:bookworm-slim

ARG BUILD_VERSION=dev
LABEL org.opencontainers.image.title="Maintenance" \
      io.hass.name="Maintenance" \
      org.opencontainers.image.source="https://github.com/jessfraz/ha-maintenance" \
      org.opencontainers.image.licenses="MIT" \
      io.hass.version="${BUILD_VERSION}" \
      io.hass.type="app" \
      io.hass.arch="aarch64|amd64"

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates gosu \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 maintenance \
    && useradd --uid 10001 --gid maintenance --no-create-home --shell /usr/sbin/nologin maintenance \
    && install -d -o maintenance -g maintenance /data

COPY --from=build /src/target/release/home-maintenance /usr/local/bin/home-maintenance
COPY docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh

ENV MAINTENANCE_BIND=0.0.0.0:8080 \
    MAINTENANCE_DATABASE=/data/maintenance.sqlite3

WORKDIR /data
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/docker-entrypoint.sh"]
