# Build from the repository root:  docker build -f server/Dockerfile -t bllu-server .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build -p bllu-server --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data bllu \
 && mkdir -p /data && chown bllu:bllu /data && chmod 0700 /data
COPY --from=build /src/target/release/bllu-server /usr/local/bin/bllu-server
ENV BLLU_DATA_DIR=/data \
    BLLU_LISTEN=0.0.0.0:8443
VOLUME /data
EXPOSE 8443
USER bllu
ENTRYPOINT ["tini", "--", "bllu-server"]
CMD ["serve"]
