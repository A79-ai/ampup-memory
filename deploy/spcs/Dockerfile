# Context = a dir holding `bin/`, the stripped linux/amd64 binary (`ampup-memory`, and `memstore`, its name before the
# rename, a link to it, which a directory's copy keeps), `models/`, the embedding model's
# files, and `doc/`, the licenses (LICENSE, NOTICE, THIRD-PARTY-NOTICES.txt; see deploy/spcs/build.sh). The model is baked in: SPCS and Cloud Run have no egress
# to fetch it at runtime.
# Minimal base: glibc + CA certificates, no shell or package manager, runs as non-root (uid 65532).
FROM cgr.dev/chainguard/glibc-dynamic:latest
COPY bin/ /usr/local/bin/
COPY models /models
COPY doc /usr/share/doc/memstore
ENV AMPUP_MEMORY_MODEL_DIR=/models
USER nonroot
ENTRYPOINT ["/usr/local/bin/ampup-memory"]
