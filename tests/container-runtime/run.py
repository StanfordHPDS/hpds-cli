#!/usr/bin/env python3
"""Build and run generated Dockerfiles against representative projects."""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tarfile
import tempfile


ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "tests" / "fixtures" / "container-runtime"


def run(command: list[str], *, cwd: Path | None = None, env: dict[str, str] | None = None) -> None:
    print("+", " ".join(command), flush=True)
    subprocess.run(command, cwd=cwd, env=env, check=True)


def poison(project: Path) -> None:
    for directory in [
        ".venv",
        "renv/library",
        "R-renv-library",
        "R-library",
        ".cache/uv",
        ".cache/renv",
    ]:
        target = project / directory
        target.mkdir(parents=True, exist_ok=True)
        (target / "HPDS_HOST_POISON").write_text("must not enter the image\n")
    fake_python = project / ".venv" / "bin" / "python"
    fake_python.parent.mkdir(parents=True, exist_ok=True)
    fake_python.write_text("#!/bin/sh\necho incompatible host Python >&2\nexit 97\n")
    fake_python.chmod(0o755)
    fake_r_package = project / "renv" / "library" / "host-only" / "DESCRIPTION"
    fake_r_package.parent.mkdir(parents=True, exist_ok=True)
    fake_r_package.write_text("Package: hostonly\nVersion: 999.0.0\n")


def materialize_cellar(project: Path) -> None:
    source = project / "vendor-src" / "vendored"
    if not source.is_dir():
        return
    cellar = project / "renv" / "cellar"
    cellar.mkdir(parents=True, exist_ok=True)
    with tarfile.open(cellar / "vendored_1.0.0.tar.gz", "w:gz") as archive:
        archive.add(source, arcname="vendored")


def check_command(
    name: str,
    expected_hpds_version: str,
    script_dir: str = ".",
    *,
    assert_project_status: bool = True,
) -> list[str]:
    hpds_check = f"test \"$(hpds --version)\" = {shlex.quote(expected_hpds_version)}"
    poison_check = (
        "! find /project -name HPDS_HOST_POISON -print -quit | grep -q ."
    )
    cellar_check = "test -f /project/renv/cellar/vendored_1.0.0.tar.gz"
    status_check = (
        "stopifnot(isTRUE(renv::status()$synchronized)); "
        if assert_project_status
        else ""
    )
    if name == "r":
        runtime = (
            f"{cellar_check} && R -s -e "
            "'stopifnot(getRversion() == \"4.4.3\"); "
            "stopifnot(as.character(packageVersion(\"digest\")) == \"0.6.37\"); "
            "stopifnot(nzchar(digest::digest(1:5))); "
            "stopifnot(vendored::fixture_value() == 42L); "
            f"{status_check}invisible(TRUE)'"
        )
    elif name == "python-version":
        runtime = f"python {script_dir}/check.py 3 12"
    elif name == "python-no-version":
        runtime = f"python {script_dir}/check.py 3 11 3 13"
    elif name == "both":
        runtime = (
            f"{cellar_check} && python {script_dir}/check.py 3 12 && "
            "R -s -e "
            "'stopifnot(getRversion() == \"4.4.3\"); "
            "stopifnot(as.character(packageVersion(\"digest\")) == \"0.6.37\"); "
            "stopifnot(nzchar(digest::digest(1:5))); "
            "stopifnot(vendored::fixture_value() == 42L); "
            f"{status_check}invisible(TRUE)'"
        )
    else:
        raise ValueError(f"unknown fixture {name}")
    return ["sh", "-ceu", f"{hpds_check}\n{poison_check}\n{runtime}"]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--hpds",
        type=Path,
        default=ROOT / "target" / "debug" / "hpds",
        help="hpds binary used to generate each Dockerfile",
    )
    parser.add_argument(
        "--engine",
        choices=["docker", "apptainer", "all"],
        default="docker",
        help="container engine to validate (default: docker)",
    )
    parser.add_argument(
        "--apptainer-build-as-root",
        action="store_true",
        help="run only apptainer build through sudo; runtime checks remain unprivileged",
    )
    args = parser.parse_args()

    docker = shutil.which("docker") if args.engine in {"docker", "all"} else None
    apptainer = shutil.which("apptainer") if args.engine in {"apptainer", "all"} else None
    if args.engine in {"docker", "all"} and docker is None:
        parser.error("docker is required for the selected engine; install it and start the daemon")
    if args.engine in {"apptainer", "all"} and apptainer is None:
        parser.error("apptainer is required for the selected engine; install it and retry")
    if not args.hpds.is_file():
        parser.error(f"hpds binary not found at {args.hpds}; build it first")
    expected_hpds_version = subprocess.check_output(
        [str(args.hpds.resolve()), "--version"], text=True
    ).strip()

    if docker is not None:
        run([docker, "info"])
    fixture_specs = [
        ("r", "r"),
        ("python-version", "python"),
        ("python-no-version", "python"),
        ("both", "both"),
    ]
    tags: list[str] = []
    try:
        with tempfile.TemporaryDirectory(prefix="hpds-container-runtime-") as temporary:
            temporary_root = Path(temporary)
            run_id = f"{os.getpid()}-{temporary_root.name.rsplit('-', 1)[-1].lower()}"
            for name, language in fixture_specs:
                project = temporary_root / name
                shutil.copytree(FIXTURES / name, project)
                materialize_cellar(project)
                poison(project)
                generation_env = os.environ.copy()
                generation_env["HPDS_R_VERSION"] = "4.6.1"
                run(
                    [
                        str(args.hpds.resolve()),
                        "use",
                        "container",
                        "--kind",
                        "both" if args.engine == "all" else args.engine,
                        "--language",
                        language,
                    ],
                    cwd=project,
                    env=generation_env,
                )
                if docker is not None:
                    tag = f"hpds-container-runtime-{run_id}-{name}"
                    tags.append(tag)
                    build = [docker, "buildx", "build", "--load", "--tag", tag]
                    build.append(".")
                    run(build, cwd=project)
                    run(
                        [
                            docker,
                            "run",
                            "--rm",
                            tag,
                            *check_command(name, expected_hpds_version),
                        ]
                    )
                if apptainer is not None:
                    image = temporary_root / f"{name}.sif"
                    runtime_home = temporary_root / f"{name}-home"
                    runtime_home.mkdir()
                    build = [apptainer, "build", "--force", str(image), "container.def"]
                    if args.apptainer_build_as_root:
                        build.insert(0, "sudo")
                    run(build, cwd=project)
                    run(
                        [
                            apptainer,
                            "exec",
                            "--home",
                            f"{runtime_home}:/home/validation",
                            "--bind",
                            f"{project}:/workspace",
                            "--pwd",
                            "/project",
                            str(image),
                            *check_command(
                                name,
                                expected_hpds_version,
                                "/workspace",
                                assert_project_status=False,
                            ),
                        ]
                    )
    finally:
        for tag in tags:
            assert docker is not None
            subprocess.run(
                [docker, "image", "rm", "--force", tag],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )

    print("all container runtime fixtures passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
