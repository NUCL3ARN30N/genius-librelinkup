# Build from the repository root:  docker build -f server/Dockerfile -t gllu-server .
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build -p gllu-server --release --locked

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data gllu \
 && mkdir -p /data && chown gllu:gllu /data && chmod 0700 /data
COPY --from=build /src/target/release/gllu-server /usr/local/bin/gllu-server
ENV GLLU_DATA_DIR=/data \
    GLLU_LISTEN=0.0.0.0:8443
VOLUME /data
EXPOSE 8443
USER gllu
ENTRYPOINT ["tini", "--", "gllu-server"]
CMD ["serve"]
