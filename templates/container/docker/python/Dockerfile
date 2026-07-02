# Docker image for {{project}} (Python).
#
# Build:  docker build -t {{project}} .
# Run:    docker run --rm -it {{project}}

# The lab image is a rolling tag rebuilt on the lab's schedule; pinning a
# release here would fork projects off the shared image.
# hadolint ignore=DL3007
FROM stanfordhpds/python-uv:latest

WORKDIR /project

# uv package-manager configuration: compile bytecode for faster startup and
# copy (rather than hardlink) packages across filesystem boundaries.
ENV UV_COMPILE_BYTECODE=1 \
    UV_LINK_MODE=copy

# Install dependencies from the lockfile before copying the rest of the
# project, so this layer stays cached until the lockfile changes.
COPY pyproject.toml uv.lock ./
RUN uv sync --locked --no-install-project

# Then copy the project and install it into the environment.
COPY . .
RUN uv sync --locked

ENV PATH="/project/.venv/bin:$PATH"

CMD ["python"]
