# Context = a dir holding the stripped linux/amd64 binary, `models/`, the embedding model's
# files, and `doc/`, the licenses (LICENSE, NOTICE, THIRD-PARTY-NOTICES.txt; see deploy/spcs/build.sh). The model is baked in: SPCS and Cloud Run have no egress
# to fetch it at runtime.
# Minimal base: glibc + CA certificates, no shell or package manager, runs as non-root (uid 65532).
FROM cgr.dev/chainguard/glibc-dynamic:latest
COPY memstore /usr/local/bin/memstore
COPY models /models
COPY doc /usr/share/doc/memstore
ENV MEMSTORE_MODEL_DIR=/models
USER nonroot
ENTRYPOINT ["/usr/local/bin/memstore"]
