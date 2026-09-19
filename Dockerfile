# --- Builder ---
FROM rust:1.98-alpine AS builder
RUN apk add --no-cache build-base
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY templates ./templates
COPY static ./static
COPY migrations ./migrations
RUN cargo build --release

# --- Runtime ---
FROM alpine:3.20
RUN apk add --no-cache ca-certificates tzdata
COPY --from=builder /app/target/release/vigilant-vole /usr/local/bin/vigilant-vole
RUN mkdir -p /data
VOLUME /data
EXPOSE 3001
ENV DATA_DIR=/data
ENTRYPOINT ["/usr/local/bin/vigilant-vole"]
