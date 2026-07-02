# Docker image for {{project}} (R + Python).
#
# Build:  docker build -t {{project}} .
# Run:    docker run --rm -it {{project}}

# The lab image is a rolling tag rebuilt on the lab's schedule; pinning a
# release here would fork projects off the shared image.
# hadolint ignore=DL3007
FROM stanfordhpds/base:latest

WORKDIR /project

# Package-manager configuration: R packages come as prebuilt Linux binaries
# from Posit Package Manager; uv compiles bytecode and copies (rather than
# hardlinks) packages across filesystem boundaries.
ENV RENV_CONFIG_REPOS_OVERRIDE=https://packagemanager.posit.co/cran/__linux__/noble/latest \
    UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy

# Restore each language's environment from its lockfile before copying the
# rest of the project, so these layers stay cached until a lockfile changes.
COPY renv.lock renv.lock
RUN R -q -e 'renv::restore()'

COPY pyproject.toml uv.lock ./
RUN uv sync --locked --no-install-project

# Then copy the project and install it into the Python environment.
COPY . .
RUN uv sync --locked

ENV PATH="/project/.venv/bin:$PATH"

CMD ["bash"]
