# syntax=docker/dockerfile:1

# hpds is copied from the exact release that generated this file.
FROM debian:trixie-slim AS hpds
ARG TARGETARCH
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl \
    && case "$TARGETARCH" in \
        amd64) target=x86_64 ;; \
        arm64) target=aarch64 ;; \
        *) echo "unsupported TARGETARCH '$TARGETARCH'; hpds supports amd64 and arm64" >&2; exit 1 ;; \
    esac \
    && asset="hpds-${target}-unknown-linux-musl.tar.gz" \
    && release="https://github.com/StanfordHPDS/hpds-cli/releases/download/v{{hpds_version}}/$asset" \
    && curl --fail --location --retry 3 --output "$asset" "$release" \
    && curl --fail --location --retry 3 --output "$asset.sha256" "$release.sha256" \
    && sha256sum --check "$asset.sha256" \
    && tar -xzf "$asset" \
    && install -m 0755 "hpds-${target}-unknown-linux-musl/hpds" /hpds

# uv reads the project's Python requirement and installs that runtime, so the
# container does not need a separate base image for every Python version.
FROM debian:trixie-slim AS hpds-project

ARG HPDS_PROJECT_DIR=/project

LABEL org.opencontainers.image.title="{{project}}"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git make \
    && rm -rf /var/lib/apt/lists/*

COPY --from=hpds /hpds /usr/local/bin/hpds
COPY --from=ghcr.io/astral-sh/uv:{{uv_version}} /uv /uvx /usr/local/bin/

WORKDIR ${HPDS_PROJECT_DIR}

ENV UV_PROJECT_ENVIRONMENT=/opt/venv \
    UV_PYTHON_INSTALL_DIR=/opt/python \
    UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy \
    UV_CACHE_DIR=/opt/uv-cache \
    PATH="/opt/venv/bin:${PATH}"

# Restore dependencies before copying the source so this layer changes only
# when pyproject.toml or uv.lock changes.
COPY pyproject.toml uv.lock{{python_version_docker_source}} ./
RUN --mount=type=cache,target=/opt/uv-cache \
    uv sync --locked --no-install-project

COPY . .
RUN --mount=type=cache,target=/opt/uv-cache \
    uv sync --locked

FROM hpds-project AS hpds-dev
ARG HPDS_UID=1000
ARG HPDS_GID=1000
RUN case "${HPDS_UID}:${HPDS_GID}" in *[!0-9:]*|:*|*:) echo "HPDS_UID and HPDS_GID must be positive numeric IDs" >&2; exit 2 ;; esac \
    && if [ "${HPDS_UID}" -eq 0 ] || [ "${HPDS_GID}" -eq 0 ]; then echo "HPDS_UID and HPDS_GID must be nonzero" >&2; exit 2; fi \
    && apt-get update \
    && apt-get install -y --no-install-recommends curl openssh-client wget \
    && rm -rf /var/lib/apt/lists/* \
    && group_name="$(getent group "${HPDS_GID}" | cut -d: -f1 || true)" \
    && if [ -z "$group_name" ]; then groupadd --gid "${HPDS_GID}" hpds; elif [ "$group_name" != hpds ]; then groupmod --new-name hpds "$group_name"; fi \
    && user_name="$(getent passwd "${HPDS_UID}" | cut -d: -f1 || true)" \
    && if [ -z "$user_name" ]; then useradd --uid "${HPDS_UID}" --gid "${HPDS_GID}" --create-home --shell /bin/bash hpds; elif [ "$user_name" != hpds ]; then usermod --login hpds --home /home/hpds --move-home "$user_name"; fi \
    && usermod --gid "${HPDS_GID}" hpds \
    && mkdir -p /opt/uv-cache /home/hpds \
    && chown -R "${HPDS_UID}:${HPDS_GID}" /opt/venv /opt/python /opt/uv-cache /home/hpds
USER hpds
CMD ["sleep", "infinity"]

FROM hpds-project AS hpds-analysis
CMD ["python"]
