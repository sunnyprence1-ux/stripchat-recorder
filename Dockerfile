FROM debian:latest AS builder

LABEL maintainer="chantrail@chantrail.com" \
      version="0.3.5" \
      description="Stripchat Recorder Docker builder"

RUN sed -i 's/deb.debian.org/mirrors.ustc.edu.cn/g' /etc/apt/sources.list.d/debian.sources

RUN apt-get update && apt-get install -y \
    curl \
    git \
    pkg-config \
    build-essential \
    libssl-dev \
    cmake \
    perl \
    libclang-dev \
    python3 \
    && rm -rf /var/lib/apt/lists/*

RUN curl -fsSL https://deb.nodesource.com/setup_lts.x | bash - && \
    apt-get install -y nodejs && \
    rm -rf /var/lib/apt/lists/*

RUN npm config set registry https://registry.npmmirror.com

ENV RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static \
    RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup

RUN curl --proto '=https' --tlsv1.2 -sSf https://mirrors.ustc.edu.cn/misc/rustup-install.sh | sh -s -- -y && \
    . /root/.cargo/env && \
    rustup target add x86_64-unknown-linux-gnu

# Google Cloud SDK provides the Google account login flow for Google Drive.
# It uses Google's own OAuth client, so the user never creates a client ID.
# Pin the version so the --enable-gdrive-access login flag cannot disappear.
RUN curl -fsSL https://dl.google.com/dl/cloudsdk/channels/rapid/downloads/google-cloud-cli-594.0.0-linux-x86_64.tar.gz -o /tmp/google-cloud-sdk.tar.gz \
    && tar -xzf /tmp/google-cloud-sdk.tar.gz -C /opt \
    && /opt/google-cloud-sdk/install.sh --quiet --usage-reporting false --path-update false --command-completion false \
    && rm /tmp/google-cloud-sdk.tar.gz

# Optional crates.io mirror.  Leave empty for the official registry (GitHub
# Actions runners have direct access).  Example for builds in China:
#   --build-arg CARGO_REGISTRY_MIRROR=sparse+https://mirrors.ustc.edu.cn/crates.io-index/
ARG CARGO_REGISTRY_MIRROR=""

RUN mkdir -vp ${CARGO_HOME:-$HOME/.cargo} && \
    if [ -n "$CARGO_REGISTRY_MIRROR" ]; then \
      printf '%s\n' \
      '[source.crates-io]' \
      "replace-with = 'mirror'" \
      '' \
      '[source.mirror]' \
      "registry = \"$CARGO_REGISTRY_MIRROR\"" \
      | tee -a ${CARGO_HOME:-$HOME/.cargo}/config.toml; \
    fi

WORKDIR /app
COPY . /app

RUN . /root/.cargo/env && npm run build \
    && cargo test --manifest-path /app/modules/pp_utils/Cargo.toml --lib \
    && cargo test --manifest-path /app/modules/notify_telegram/Cargo.toml --bin notify_telegram_v050 \
    && python3 -m unittest discover -s /app/modules/notify_telegram/tests -p 'test_*.py'

# ── Runtime image ──────────────────────────────────────────────────────────────
FROM debian:latest

LABEL maintainer="chantrail@chantrail.com" \
      version="0.3.5" \
      description="Stripchat Recorder"

RUN sed -i 's/deb.debian.org/mirrors.ustc.edu.cn/g' /etc/apt/sources.list.d/debian.sources

RUN apt-get update && apt-get install -y \
    ffmpeg \
    ca-certificates \
    libssl3 \
    python3 \
    python3-venv \
    python3-pip \
    && rm -rf /var/lib/apt/lists/*

# WZGram is installed only in the CI-built image. The VPS never builds Python wheels.
RUN python3 -m venv /opt/wzgram-venv \
    && /opt/wzgram-venv/bin/pip install --no-cache-dir --upgrade pip \
    && /opt/wzgram-venv/bin/pip install --no-cache-dir \
        "wzgram==3.1.7" \
        "warpcrypto==2.1.0" \
        "python-socks[asyncio]==3.2.1" \
        "aiosqlite==0.22.1"

RUN mkdir -p /app/stripchat-recorder/logs \
             /app/stripchat-recorder/recordings \
             /app/stripchat-recorder/modules.default \
             /app/stripchat-recorder/modules \
             /app/stripchat-recorder/config
WORKDIR /app

COPY --from=builder /app/build/stripchat-recorder /app/stripchat-recorder/
COPY --from=builder /app/build/modules/ /app/stripchat-recorder/modules.default/
COPY --from=builder /opt/google-cloud-sdk /opt/google-cloud-sdk
COPY modules/notify_telegram/wzgram_upload.py /app/stripchat-recorder/wzgram_upload.py
COPY modules/upload_gdrive/gcloud_login.py /app/stripchat-recorder/gcloud_login.py

RUN chmod +x /app/stripchat-recorder/stripchat-recorder \
    && chmod +x /app/stripchat-recorder/gcloud_login.py \
    && chmod 0644 /app/stripchat-recorder/wzgram_upload.py \
    && python3 -m py_compile /app/stripchat-recorder/wzgram_upload.py \
    && python3 -m py_compile /app/stripchat-recorder/gcloud_login.py

ENV WZGRAM_PYTHON=/opt/wzgram-venv/bin/python
ENV PATH="/opt/google-cloud-sdk/bin:${PATH}"

RUN printf '%s\n' \
    '#!/bin/sh' \
    'set -eu' \
    '' \
    '# contact_sheet was removed; delete any binary left in a persistent modules volume.' \
    'rm -f /app/stripchat-recorder/modules/contact_sheet_v*' \
    '' \
    '# Remove older versions of built-in modules so a stale binary cannot win discovery.' \
    'for current in /app/stripchat-recorder/modules.default/*; do' \
    '    [ -f "$current" ] || continue' \
    '    base=$(basename "$current")' \
    '    case "$base" in' \
    '        *_v*) prefix=${base%_v*} ;;' \
    '        *) continue ;;' \
    '    esac' \
    '    for old in /app/stripchat-recorder/modules/${prefix}_v*; do' \
    '        [ -f "$old" ] || continue' \
    '        [ "$(basename "$old")" = "$base" ] || rm -f "$old"' \
    '    done' \
    'done' \
    '' \
    'cp -an /app/stripchat-recorder/modules.default/. /app/stripchat-recorder/modules/' \
    '' \
    '# LANGUAGE and PORT are handled by the backend from environment variables.' \
    '# This keeps Railway deployments working when the config directory is empty.' \
    'exec /app/stripchat-recorder/stripchat-recorder "$@"' \
    > /entrypoint.sh && chmod +x /entrypoint.sh

# Persistent storage: attach a Railway Volume at /app/stripchat-recorder/recordings (or a parent directory)

EXPOSE ${PORT:-3030}

ENTRYPOINT ["/entrypoint.sh"]
