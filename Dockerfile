# syntax=docker/dockerfile:1
#
# codesearch federation cloud image.
#
# Multi-stage:
#   1. builder   — compile the release binary AND pre-download the fastembed
#                  model into the image (fast, offline cold starts; no
#                  HuggingFace dependency at runtime). The warm-up lives here,
#                  not in a separate stage: ACR's classic builder cannot
#                  reliably COPY --from a chained (FROM builder) stage.
#   2. runtime   — slim Debian + git + azcopy + the binary + the cached model
#
# Runs `docker/entrypoint.sh`, which syncs the source corpus from Azure Blob
# (SAS URL) into /data and serves it. See docs/federation-cloud-deployment.md.

# ---------------------------------------------------------------------------
# 1. Builder
# ---------------------------------------------------------------------------
# trixie (glibc 2.41), NOT bookworm (2.36): the prebuilt onnxruntime pulled by
# `ort` references glibc-2.38+ C23 symbols (__isoc23_strtoll), so linking on
# bookworm fails. Runtime stage matches (trixie) so the binary loads at runtime.
FROM rust:1-trixie AS builder
WORKDIR /src

# System deps for the build: onnxruntime (ort/fastembed) + TLS for reqwest.
RUN apt-get update && apt-get install -y --no-install-recommends \
        pkg-config libssl-dev cmake \
    && rm -rf /var/lib/apt/lists/*

# Cache dependencies separately from source for faster rebuilds.
COPY Cargo.toml Cargo.lock build.rs ./
COPY src ./src
# Hook scripts are embedded at compile time via include_str! in
# src/cli/claude_hooks.rs (path ../../integrations/claude-code/hooks/*). They
# MUST be present in the build context or the release compile fails with
# "couldn't read ... No such file or directory". Only this subtree is needed.
COPY integrations/claude-code/hooks ./integrations/claude-code/hooks
# build.rs sets CARGO_PKG_VERSION_FULL (consumed by env!() in main.rs/cli). It
# shells out to git for the commit count/hash but falls back to "0"/"unknown"
# when .git is absent (it is — excluded by .dockerignore), so the build is
# reproducible without the repo history.
# Build only the main binary (the C# helper is not needed for docs federation).
# NOTE: no BuildKit `--mount=type=cache` here — ACR Tasks uses the classic
# builder, which rejects `--mount`. ACR builds fresh each run anyway.
RUN cargo build --release --bin codesearch \
    && cp /src/target/release/codesearch /usr/local/bin/codesearch \
    # Stage any onnxruntime shared lib emitted next to the binary so the
    # runtime image can load it (ort dynamic-link layout).
    && mkdir -p /out/lib \
    && (find /src/target/release -maxdepth 2 -name 'libonnxruntime*.so*' -exec cp {} /out/lib/ \; || true)

# ---------------------------------------------------------------------------
# 2. Warm the embedding model INTO the builder stage
# ---------------------------------------------------------------------------
# Bake the default embedding model into the image for fast, offline cold starts
# (no HuggingFace dependency at runtime). The model is downloaded here in the
# builder stage (which has the binary + onnxruntime lib), then packed into a
# SINGLE tarball and transferred to the runtime stage. See the tar step below
# for why a direct directory `COPY --from` of the cache cannot be used.
ENV HOME=/home/app
RUN set -eux; \
    mkdir -p /home/app /tmp/warm; \
    printf '# warmup\nhello world\n' > /tmp/warm/README.md; \
    # Indexing a throwaway repo forces fastembed to download the default model
    # into ~/.codesearch/models. Silence index-add's decorative U+2795 (➕)
    # output — it crashes `az acr build`'s cp1252 log streamer (colorama
    # UnicodeEncodeError). Tolerate index-add's own exit code, but then
    # HARD-VERIFY the model cache actually populated, so a failed download fails
    # the build loudly HERE.
    LD_LIBRARY_PATH=/out/lib codesearch index add /tmp/warm > /dev/null 2>&1 || true; \
    test -d /home/app/.codesearch/models && [ -n "$(ls -A /home/app/.codesearch/models)" ] \
        || { echo 'ERROR: warmup did not populate /home/app/.codesearch/models' >&2; exit 1; }; \
    # Pack the model cache into a SINGLE tarball. The fastembed/HuggingFace cache
    # is a symlink tree (snapshots/ -> blobs/); ACR's classic builder cannot
    # export a cross-stage `COPY --from` of a symlinked directory tree — it fails
    # at export with "failed to get layer <sha>: layer does not exist" (a single
    # regular file like the binary copies fine — that is Step "COPY codesearch").
    # tar preserves the symlinks inside the archive; runtime copies the one file
    # and untars it.
    tar czf /models.tar.gz -C /home/app/.codesearch models; \
    rm -rf /tmp/warm

# ---------------------------------------------------------------------------
# 2. Runtime
# ---------------------------------------------------------------------------
FROM debian:trixie-slim AS runtime
ENV HOME=/home/app \
    LD_LIBRARY_PATH=/usr/local/lib \
    CODESEARCH_SERVE_PORT=39725 \
    DATA_DIR=/data

# Runtime deps: TLS roots, git (KB pull), libgomp (onnxruntime), curl (probe loop).
# No libssl: reqwest uses rustls (Cargo.toml), so no OpenSSL at runtime.
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates git libgomp1 curl \
    && rm -rf /var/lib/apt/lists/*

# azcopy (single static binary from Microsoft).
RUN set -eux; \
    arch="$(dpkg --print-architecture)"; \
    case "$arch" in \
      amd64) azurl="https://aka.ms/downloadazcopy-v10-linux" ;; \
      arm64) azurl="https://aka.ms/downloadazcopy-v10-linux-arm64" ;; \
      *) echo "unsupported arch: $arch" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "$azurl" -o /tmp/azcopy.tgz; \
    tar -xzf /tmp/azcopy.tgz -C /tmp; \
    cp /tmp/azcopy_linux_*/azcopy /usr/local/bin/azcopy; \
    chmod +x /usr/local/bin/azcopy; \
    rm -rf /tmp/azcopy*

# Non-root user.
RUN useradd --create-home --home-dir /home/app --shell /usr/sbin/nologin app

# Binary + onnxruntime lib + pre-warmed model cache + entrypoint.
COPY --from=builder /usr/local/bin/codesearch /usr/local/bin/codesearch
COPY --from=builder /out/lib/ /usr/local/lib/
# Restore ONLY the model cache (model weights + embedding cache), NOT the whole
# ~/.codesearch — the warmup also writes a repos.json registering "/tmp/warm",
# which would otherwise bake a stale "warm" repo into the runtime image. The
# cache ships as a single tarball (see the builder stage): a directory
# `COPY --from` of its symlink tree breaks ACR's classic builder at export.
COPY --from=builder /models.tar.gz /tmp/models.tar.gz
RUN mkdir -p /home/app/.codesearch \
    && tar xzf /tmp/models.tar.gz -C /home/app/.codesearch \
    && rm /tmp/models.tar.gz
COPY docker/entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh \
    && mkdir -p /data \
    && chown -R app:app /home/app /data

USER app
WORKDIR /home/app
EXPOSE 39725

# Liveness probe target (also configured on the ACA app):
#   GET /healthz -> 200 {"status":"ok"} (unauthenticated)
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
