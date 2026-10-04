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
FROM debian:trixie-slim

LABEL org.opencontainers.image.title="{{project}}"

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates git make \
    && rm -rf /var/lib/apt/lists/*

COPY --from=hpds /hpds /usr/local/bin/hpds
COPY --from=ghcr.io/astral-sh/uv:{{uv_version}} /uv /uvx /usr/local/bin/

WORKDIR /project

ENV UV_PROJECT_ENVIRONMENT=/project/.venv \
    UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy \
    UV_CACHE_DIR=/project/.cache/uv \
    PATH="/project/.venv/bin:${PATH}"

# Restore dependencies before copying the source so this layer changes only
# when pyproject.toml or uv.lock changes.
COPY pyproject.toml uv.lock{{python_version_docker_source}} ./
RUN --mount=type=cache,target=/project/.cache/uv \
    uv sync --locked --no-install-project

COPY . .
RUN --mount=type=cache,target=/project/.cache/uv \
    uv sync --locked

CMD ["python"]
