# syntax=docker/dockerfile:1
#
# codesearch federation cloud image.
#
# Multi-stage:
#   1. builder   — compile the release binary
#   2. warmer    — pre-download the fastembed model into the image (fast, offline
#                  cold starts; no HuggingFace dependency at runtime)
#   3. runtime   — slim Debian + git + azcopy + the binary + the cached model
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
# 2. Warmer — bake the default embedding model into the image
# ---------------------------------------------------------------------------
FROM builder AS warmer
ENV HOME=/home/app
RUN mkdir -p /home/app
# Indexing a tiny throwaway repo forces fastembed to download the default model
# into ~/.codesearch/models. We discard the index; we only want the model cache.
RUN set -eux; \
    mkdir -p /tmp/warm; \
    printf '# warmup\nhello world\n' > /tmp/warm/README.md; \
    LD_LIBRARY_PATH=/out/lib codesearch index add /tmp/warm || true; \
    rm -rf /tmp/warm/.codesearch.db

# ---------------------------------------------------------------------------
# 3. Runtime
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
COPY --from=warmer  /home/app/.codesearch /home/app/.codesearch
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
