# syntax=docker/dockerfile:1

# hpds and R are both pinned when this template is rendered.
FROM ghcr.io/stanfordhpds/hpds-cli:{{hpds_version}} AS hpds
FROM rocker/r-ver:{{r_version}}

LABEL org.opencontainers.image.title="{{project}}"

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        git \
    && rm -rf /var/lib/apt/lists/*

COPY --from=hpds /hpds /usr/local/bin/hpds

WORKDIR /project

# Copy the complete renv bootstrap before the project source so the package
# restore layer changes only when the locked environment changes.
RUN mkdir -p renv
COPY renv.lock .Rprofile ./
COPY renv/activate.R renv/settings.json renv/

ENV RENV_PATHS_CACHE=/project/.cache/renv \
    RENV_CONFIG_CACHE_SYMLINKS=FALSE

RUN --mount=type=cache,target=/project/.cache/renv \
    R -s -e 'renv::restore()'

COPY . .

CMD ["R"]
