# syntax=docker/dockerfile:1

# hpds and R are both pinned when this template is rendered.
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

WORKDIR /project

# Copy the complete renv bootstrap before the project source so the package
# restore layer changes only when the locked environment changes.
RUN mkdir -p renv
COPY renv.lock .Rprofile ./
COPY renv/activate.R renv/settings.json renv/
{{renv_cellar_docker_copy}}

ENV RENV_PATHS_CACHE=/project/.cache/renv \
    RENV_CONFIG_CACHE_SYMLINKS=FALSE

RUN --mount=type=cache,target=/project/.cache/renv \
    R -s -e 'renv::restore()'

COPY . .

CMD ["R"]
