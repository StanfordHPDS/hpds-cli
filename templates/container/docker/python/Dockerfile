# syntax=docker/dockerfile:1

# hpds is copied from the exact release that generated this file.
FROM ghcr.io/stanfordhpds/hpds-cli:{{hpds_version}} AS hpds

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
COPY pyproject.toml uv.lock ./
RUN --mount=type=cache,target=/project/.cache/uv \
    uv sync --locked --no-install-project

COPY . .
RUN --mount=type=cache,target=/project/.cache/uv \
    uv sync --locked

CMD ["python"]
