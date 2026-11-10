# The engine image, compiled from source with the repo root as the context:
#   docker build -f deploy/docker/Dockerfile -t memstore .
# cargo-chef keeps the dependency build in its own layer (the `deps` target), so with a layer cache a source
# change recompiles only memstore. The embedding model is baked in, so the image needs no egress to fetch it.
# Unlike deploy/spcs/Dockerfile (which packages a binary built elsewhere) the runtime has a shell (busybox),
# for jobs that run a script.
# trixie, not bookworm: ort's prebuilt ONNX Runtime needs GCC 13+'s libstdc++.
FROM rust:1.99-trixie AS chef
# libprotobuf-dev: the well-known .proto files (google/protobuf/empty.proto) Lance imports.
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler libprotobuf-dev \
    && rm -rf /var/lib/apt/lists/*
RUN cargo install cargo-chef --locked
WORKDIR /src

FROM chef AS plan
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# The compiled dependencies: the only layers a later commit reuses, so the ones worth exporting to a cache.
FROM chef AS deps
COPY --from=plan /src/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json

FROM deps AS build
COPY . .
RUN cargo build --release && strip target/release/memstore
# The embedding model, baked in as for SPCS (hf-hub cache layout; -L turns its symlinks into files).
RUN MEMSTORE_MODEL_DIR=/fetch target/release/memstore embed warmup >/dev/null \
    && cp -rL /fetch /models && find /models -name '*.lock' -delete

FROM cgr.dev/chainguard/glibc-dynamic:latest-dev
COPY --from=build /src/target/release/memstore /usr/local/bin/memstore
COPY --from=build /models /models
ENV MEMSTORE_MODEL_DIR=/models
USER nonroot
ENTRYPOINT ["/usr/local/bin/memstore"]
