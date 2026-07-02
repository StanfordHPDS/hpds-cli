# Docker image for {{project}} (R).
#
# Build:  docker build -t {{project}} .
# Run:    docker run --rm -it {{project}}

# The lab image is a rolling tag rebuilt on the lab's schedule; pinning a
# release here would fork projects off the shared image.
# hadolint ignore=DL3007
FROM stanfordhpds/r-renv:latest

WORKDIR /project

# Install R packages as prebuilt Linux binaries from Posit Package Manager.
ENV RENV_CONFIG_REPOS_OVERRIDE=https://packagemanager.posit.co/cran/__linux__/noble/latest

# Restore the R library from the lockfile before copying the rest of the
# project, so this layer stays cached until renv.lock changes.
COPY renv.lock renv.lock
RUN R -q -e 'renv::restore()'

COPY . .

CMD ["R"]
