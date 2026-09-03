ARG BUILD_FROM=ghcr.io/home-assistant/base:3.23

FROM --platform=$BUILDPLATFORM golang:1.26-alpine3.23 AS builder
ARG TARGETARCH
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=0 GOOS=linux GOARCH=${TARGETARCH} \
    go build -trimpath -ldflags="-s -w" -o /out/home-maintenance ./cmd/home-maintenance

FROM ${BUILD_FROM}
COPY --from=builder /out/home-maintenance /usr/bin/home-maintenance
COPY rootfs /

HEALTHCHECK CMD wget -q --spider http://127.0.0.1:8099/health || exit 1

LABEL \
    org.opencontainers.image.title="Home Assistant App: Home Maintenance" \
    org.opencontainers.image.description="Fast physical home maintenance backed by one private catalog" \
    org.opencontainers.image.source="https://github.com/jessfraz/home-maintenance" \
    org.opencontainers.image.licenses="MIT"
