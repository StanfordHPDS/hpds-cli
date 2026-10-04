# syntax=docker/dockerfile:1

# Start from the resolved R release. uv provisions the Python version declared
# by the project, avoiding an R-by-Python base-image matrix.
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
FROM rocker/r-ver:{{r_version}}

LABEL org.opencontainers.image.title="{{project}}"

# R packages may need project-specific system libraries. Before building, run
# renv::sysreqs(report = TRUE, collapse = TRUE) inside this base image, then
# add the reported apt packages below so they are present before renv::restore():
# https://rstudio.github.io/renv/articles/docker.html#system-dependencies
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
        make \
    && rm -rf /var/lib/apt/lists/*

COPY --from=hpds /hpds /usr/local/bin/hpds
COPY --from=ghcr.io/astral-sh/uv:{{uv_version}} /uv /uvx /usr/local/bin/

WORKDIR /project

ENV RENV_PATHS_CACHE=/project/.cache/renv \
    RENV_CONFIG_CACHE_SYMLINKS=FALSE \
    UV_PROJECT_ENVIRONMENT=/project/.venv \
    UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy \
    UV_CACHE_DIR=/project/.cache/uv \
    PATH="/project/.venv/bin:${PATH}"

RUN mkdir -p renv
COPY renv.lock .Rprofile ./
COPY renv/activate.R renv/settings.json renv/
{{renv_cellar_docker_copy}}
RUN --mount=type=cache,target=/project/.cache/renv \
    R -s -e 'renv::restore()'

COPY pyproject.toml uv.lock{{python_version_docker_source}} ./
RUN --mount=type=cache,target=/project/.cache/uv \
    uv sync --locked --no-install-project

COPY . .
RUN --mount=type=cache,target=/project/.cache/uv \
    uv sync --locked

CMD ["bash"]
