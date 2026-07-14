# syntax=docker/dockerfile:1

# Start from the resolved R release. uv provisions the Python version declared
# by the project, avoiding an R-by-Python base-image matrix.
FROM ghcr.io/stanfordhpds/hpds-cli:{{hpds_version}} AS hpds
FROM rocker/r-ver:{{r_version}}

LABEL org.opencontainers.image.title="{{project}}"

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
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
RUN --mount=type=cache,target=/project/.cache/renv \
    R -s -e 'renv::restore()'

COPY pyproject.toml uv.lock ./
RUN --mount=type=cache,target=/project/.cache/uv \
    uv sync --locked --no-install-project

COPY . .
RUN --mount=type=cache,target=/project/.cache/uv \
    uv sync --locked

CMD ["bash"]
