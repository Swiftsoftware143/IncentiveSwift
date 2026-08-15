# ============================================================
# IncentiveSwift — PRODUCTION Dockerfile (canonical deploy path)
# ============================================================
# HOST-BUILD the binary, then COPY into a minimal Ubuntu image.
#   1. /root/.cargo/bin/cargo build --release   # produces target/release/incentiveswift-api
#   2. docker build -t incentiveswift:latest .  # in deploy context with binary + migrations staged
# Authoritative deploy context: /opt/swift/docker/incentiveswift/
# ============================================================
FROM ubuntu:24.04
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 curl && rm -rf /var/lib/apt/lists/*
RUN groupadd -r incentiveswift && useradd -r -g incentiveswift incentiveswift
WORKDIR /app
COPY incentiveswift-api /app/incentiveswift
COPY migrations /app/migrations
RUN chmod +x /app/incentiveswift && chown -R incentiveswift:incentiveswift /app
USER incentiveswift
EXPOSE 8083
CMD ["/app/incentiveswift"]
