# The Rust WrkzCoin node, its import tool and the wallets, built with the
# RocksDB engine (the build stage has the libclang that needs).
#
#   docker build -t wrkz-rust .
#   docker run -d --name wrkz -v wrkz-data:/data \
#       -p 17855:17855 -p 127.0.0.1:17856:17856 wrkz-rust
#
# Inside the container the RPC listens on 0.0.0.0 so that a port mapping can
# reach it at all. Publish it on 127.0.0.1 as above, or give it a token:
# anything after the image name is appended to the daemon's command line, e.g.
#   docker run ... wrkz-rust --rpc-access-token "$(openssl rand -hex 16)"
# A Wrkzd configuration file works too: mount it and pass -c /data/wrkz.json.
#
# The chain state lives in the /data volume. To bring it up from a C++
# database instead of syncing from peers, run the import tool in the image:
#   docker run --rm -v wrkz-data:/data -v /path/to/copy/of/DB:/cpp:ro \
#       --entrypoint wrkz-replay wrkz-rust --store-raw --db /cpp --state /data/state

FROM rust:1-bookworm AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends clang libclang-dev cmake \
 && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY . .
ARG WRKZ_GIT_COMMIT=""
ENV WRKZ_GIT_COMMIT=${WRKZ_GIT_COMMIT}
RUN cargo build --release --locked --features rocksdb -p wrkz-node --bin wrkz-node \
 && cargo build --release --locked --features rocksdb -p wrkz-chain --bin wrkz-replay \
 && cargo build --release --locked -p wrkz-wallet --bins \
 && mkdir /out \
 && for b in wrkz-node wrkz-replay wrkz-wallet wrkz-wallet-api wrkz-wallet-sync wrkz-wallet-send; do \
        cp "target/release/$b" /out/; \
    done \
 && strip /out/*

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data --create-home wrkz
COPY --from=build /out/ /usr/local/bin/
USER wrkz
VOLUME /data
EXPOSE 17855 17856
# SIGTERM is the clean shutdown: the engine flushes the chain state and writes
# the peer file. Give it time: `docker stop -t 120`.
STOPSIGNAL SIGTERM
ENTRYPOINT ["wrkz-node", "--data-dir", "/data", "--rpc-bind-ip", "0.0.0.0"]
