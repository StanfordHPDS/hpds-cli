#!/usr/bin/env python3
"""Run `hpds setup --profile server --yes` in a fresh systemd Ubuntu container.

The container stands in for a newly created Ubuntu 24.04 VM: systemd is PID 1
and a passwordless-sudo user, `nero`, runs setup with the environment an ssh
login gives it. After setup, every component the server profile installs is
smoke-tested as that user, and setup is run a second time to confirm that
completed steps are idempotent. Each run's terminal output is checked for one
line per step, and its section of `~/hpds-setup.log` for the full record.

The server profile is x86_64 only, and its systemd services do not start under
emulation, so the full run needs an x86_64 Linux host with Docker.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from itertools import pairwise
import json
from pathlib import Path
import re
import shlex
import signal
import subprocess
import sys
import threading
import time
from collections.abc import Callable


HERE = Path(__file__).resolve().parent

USER = "nero"
HOME = f"/home/{USER}"
# The PATH that /etc/environment gives a login on the stock Ubuntu image.
IMAGE_PATH = (
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:"
    "/usr/games:/usr/local/games:/snap/bin"
)
LOGIN_PATH = f"{HOME}/.local/bin:{IMAGE_PATH}"
# What an ssh login sets. `runuser -l` would drop /usr/sbin from PATH, and
# Docker's default exec environment has no USER, so neither stands in for a
# real login.
LOGIN_ENV = {
    "USER": USER,
    "LOGNAME": USER,
    "HOME": HOME,
    "PATH": LOGIN_PATH,
}

IMAGE_TAG = "hpds-server-setup:local"
# Setup appends each run to this log in the user's home directory, under a
# header line of its own.
SETUP_LOG = f"{HOME}/hpds-setup.log"
LOG_HEADER = "=== hpds setup --profile server ==="
SETUP_COMMAND = ["hpds", "setup", "--profile", "server", "--yes"]
PLAN_COMMAND = ["hpds", "setup", "--plan", "--profile", "server"]
# The line the log holds before each `apt-get update`; a retry after a
# failed refresh announces itself differently.
REFRESH_ANNOUNCEMENT = "refreshing apt package lists (needs sudo)"
# A fresh machine refreshes the package lists twice: once in the system
# updates step, and once more after the system libraries step adds the
# Docker and rig sources. A second run finds those sources in place and the lists
# fresh, so only the system updates refresh them.
FIRST_RUN_REFRESHES = 2
RERUN_REFRESHES = 1
# Next steps the terminal may list beyond the git identity warnings, such
# as gh's login, Docker group membership, and a pending reboot. A run that
# needs more is printing more than one line per step.
NEXT_STEPS_ALLOWANCE = 8
# A failed step's error lines are indented this far under its step line.
FAILURE_INDENT = " " * 8
# hpds downloads the code-server package itself; this host serves the
# upstream install script it no longer runs.
CODE_SERVER_SCRIPT_HOST = "code-server.dev"
# Seconds a fresh setup should take; a slower first run is reported, not failed.
FIRST_RUN_BUDGET = 150

IDENTITY_WARNINGS = ("user.name is not set", "user.email is not set")
VACCINATE_MARKER = "# >>> hpds vaccinate >>>"
R_PROFILE = "/etc/R/Rprofile.site"
R_REPOSITORY = "https://packagemanager.posit.co/cran/__linux__/noble/latest"
VENV_CHECK = "/tmp/hpds-venv-check"
PIP_INDEX = "packagemanager.posit.co/pypi/latest/simple"
RSTUDIO_PREFS = "/etc/rstudio/rstudio-prefs.json"
RSTUDIO_PREFERENCES = {
    "insert_native_pipe_operator": True,
    "save_workspace": "never",
    "load_workspace": "never",
    "rainbow_parentheses": True,
    "rainbow_fenced_divs": True,
}
RSTUDIO_PORT = 8787
CODE_SERVER_CONFIG = f"{HOME}/.config/code-server/config.yaml"
CODE_SERVER_SETTINGS = f"{HOME}/.local/share/code-server/User/settings.json"
CODE_SERVER_EXTENSIONS = (
    "ms-python.python",
    "ms-toolsai.jupyter",
    "quarto.quarto",
    "charliermarsh.ruff",
    "tomoki1207.pdf",
)
# Where `quarto install tinytex --update-path` may leave tlmgr for a user.
TLMGR_CANDIDATES = (
    "tlmgr",
    f"{HOME}/bin/tlmgr",
    f"{HOME}/.local/bin/tlmgr",
    f"{HOME}/.TinyTeX/bin/x86_64-linux/tlmgr",
)


# Pure parsing and evaluation, unit tested in test_run.py.


def parse_plan(text: str) -> list[str]:
    """The step titles of `hpds setup --plan`, in order.

    Raises ValueError when the header is missing or the numbered lines do
    not count up from 1 to the number the header announces.
    """
    header = re.search(r"runs these (\d+) steps:", text)
    if header is None:
        raise ValueError("plan output has no `runs these N steps:` header")
    expected = int(header.group(1))
    titles: list[str] = []
    for line in text[header.end() :].splitlines():
        match = re.fullmatch(r"\s*(\d+)\. (.+)", line)
        if match is None:
            continue
        number = int(match.group(1))
        if number != len(titles) + 1:
            raise ValueError(f"plan step {number} is out of order")
        titles.append(match.group(2).strip())
    if len(titles) != expected:
        raise ValueError(
            f"plan header announces {expected} steps but lists {len(titles)}"
        )
    return titles


@dataclass(frozen=True)
class SummaryEntry:
    ok: bool
    text: str


def log_sections(text: str) -> list[str]:
    """The runs a setup log holds, oldest first, each from its header line.

    Text before the first header belongs to no run and is dropped.
    """
    sections: list[list[str]] = []
    for line in text.splitlines():
        if line.rstrip() == LOG_HEADER:
            sections.append([])
        if sections:
            sections[-1].append(line)
    return ["\n".join(lines) + "\n" for lines in sections]


def last_log_section(text: str) -> str:
    """The most recent run in a setup log.

    Raises ValueError when the log holds no run header.
    """
    sections = log_sections(text)
    if not sections:
        raise ValueError(f"the setup log has no `{LOG_HEADER}` header")
    return sections[-1]


# Lines `hpds setup` writes straight after its summary, besides the tally:
# `setup complete` ends a verbose run.
SUMMARY_TERMINATORS = ("setup complete", "error:")
TALLY = re.compile(r"(All \d+|\d+ of \d+) steps? succeeded\..*")


def ends_summary(line: str) -> bool:
    """True for a line that follows the summary rather than continuing it."""
    text = line.strip()
    return text.startswith(SUMMARY_TERMINATORS) or TALLY.fullmatch(text) is not None


def parse_summary(text: str) -> list[SummaryEntry]:
    """The ✓/✗ entries that follow `setup summary:`.

    A failed step's error can span several lines; lines after a ✗ entry
    that carry no mark belong to that entry.
    """
    entries: list[SummaryEntry] = []
    in_summary = False
    for line in text.splitlines():
        if line.strip() == "setup summary:":
            in_summary = True
            entries = []
            continue
        if not in_summary or not line.strip():
            continue
        if line.startswith("  ✓ "):
            entries.append(SummaryEntry(True, line[4:].strip()))
        elif line.startswith("  ✗ "):
            entries.append(SummaryEntry(False, line[4:].strip()))
        elif entries and not entries[-1].ok and not ends_summary(line):
            last = entries[-1]
            entries[-1] = SummaryEntry(False, f"{last.text}\n{line.rstrip()}")
        else:
            in_summary = False
    return entries


def matches_title(entry: SummaryEntry, title: str) -> bool:
    return entry.text == title or entry.text.startswith(f"{title}: ")


def summary_problems(expected: list[str], entries: list[SummaryEntry]) -> list[str]:
    """Every way the summary falls short of each planned step passing."""
    problems: list[str] = []
    if not entries:
        return ["the setup summary lists no steps"]
    if len(entries) != len(expected):
        problems.append(
            f"the summary lists {len(entries)} steps but the plan lists {len(expected)}"
        )
    matched: set[int] = set()
    for title in expected:
        index = next(
            (
                i
                for i, entry in enumerate(entries)
                if i not in matched and matches_title(entry, title)
            ),
            None,
        )
        if index is None:
            problems.append(f"missing from the summary: {title}")
            continue
        matched.add(index)
        if not entries[index].ok:
            problems.append(f"failed: {entries[index].text}")
    for i, entry in enumerate(entries):
        if i not in matched:
            mark = "✓" if entry.ok else "✗"
            problems.append(f"not in the plan: {mark} {entry.text}")
    return problems


def summary_order_problems(
    expected: list[str], entries: list[SummaryEntry]
) -> list[str]:
    """Each place where the summary lists a planned step before an earlier one.

    Entries that match no planned title are left to `summary_problems`.
    """
    positions: list[tuple[int, str]] = []
    claimed: set[int] = set()
    for entry in entries:
        index = next(
            (
                i
                for i, title in enumerate(expected)
                if i not in claimed and matches_title(entry, title)
            ),
            None,
        )
        if index is not None:
            claimed.add(index)
            positions.append((index, expected[index]))
    return [
        f"the summary lists {later} before {earlier}, against the plan order"
        for (before, later), (after, earlier) in pairwise(positions)
        if after < before
    ]


BACKGROUND_SUFFIX = " (ran in the background)"
HEADER = re.compile(r"\[(\d+)/(\d+)\] (.+?)( \(ran in the background\))?")
STEP_LINE = re.compile(r"\[(\d+)/(\d+)\] (.+) ([✓✗])")


def numbered_problems(
    lines: list[tuple[int, int, str, str]], expected: list[str], what: str
) -> list[str]:
    """Every way `(number, count, title, line)` entries stray from the plan.

    Each planned step must appear exactly once, with its own title and the
    plan's step count.
    """
    total = len(expected)
    seen: dict[int, int] = {}
    problems: list[str] = []
    for number, count, title, line in lines:
        if count != total:
            problems.append(f"{what} counts {count} steps, the plan {total}: {line}")
        if not 1 <= number <= total:
            problems.append(f"{what} numbers a step outside the plan: {line}")
            continue
        seen[number] = seen.get(number, 0) + 1
        if title != expected[number - 1]:
            problems.append(
                f"{what} {number} names {title!r}, the plan {expected[number - 1]!r}"
            )
    for number in range(1, total + 1):
        times = seen.get(number, 0)
        if times != 1:
            problems.append(f"step {number} has {times} {what}s, expected 1")
    return problems


def header_problems(text: str, expected: list[str]) -> list[str]:
    """Every way the `[n/N] title` step headers in `text` stray from the plan.

    The log introduces each planned step exactly once, with its own title
    and the plan's step count. A step that ran on the background lane may
    carry the ` (ran in the background)` suffix. Step lines, which end in
    a ✓ or ✗ mark, are not headers.
    """
    headers: list[tuple[int, int, str, str]] = []
    for line in text.splitlines():
        stripped = line.rstrip()
        if STEP_LINE.fullmatch(stripped) is not None:
            continue
        match = HEADER.fullmatch(stripped)
        if match is not None:
            headers.append(
                (int(match.group(1)), int(match.group(2)), match.group(3), line)
            )
    return numbered_problems(headers, expected, "header")


def step_line_problems(text: str, expected: list[str]) -> list[str]:
    """Every way the `[n/N] title ✓` step lines in a log stray from the plan.

    The log numbers each step line by its place in the plan. Each planned
    step must have exactly one step line, with its own title and the plan's
    step count, and every step line must carry ✓.
    """
    steps: list[tuple[int, int, str, str]] = []
    failed: list[str] = []
    for line in text.splitlines():
        match = STEP_LINE.fullmatch(line.rstrip())
        if match is None:
            continue
        steps.append((int(match.group(1)), int(match.group(2)), match.group(3), line))
        if match.group(4) != "✓":
            failed.append(f"step line marks a failure: {line.rstrip()}")
    return numbered_problems(steps, expected, "step line") + failed


def terminal_step_line_problems(text: str, expected: list[str]) -> list[str]:
    """Every way the terminal's `[n/N] title ✓` step lines stray from the plan.

    The terminal prints each step's line as the step finishes, so lines come
    in completion order and `n` counts finished steps: the lines must be
    numbered 1 to N in order, with the plan's step count. Each planned step
    must have exactly one line, matched by title, and every line must carry ✓.
    """
    total = len(expected)
    problems: list[str] = []
    failed: list[str] = []
    seen: dict[str, int] = {}
    position = 0
    for line in text.splitlines():
        stripped = line.rstrip()
        match = STEP_LINE.fullmatch(stripped)
        if match is None:
            continue
        position += 1
        number, count, title = int(match.group(1)), int(match.group(2)), match.group(3)
        if number != position:
            problems.append(f"step line {position} is numbered {number}: {stripped}")
        if count != total:
            problems.append(
                f"step line counts {count} steps, the plan {total}: {stripped}"
            )
        if title in expected:
            seen[title] = seen.get(title, 0) + 1
        else:
            problems.append(f"step line names a step outside the plan: {stripped}")
        if match.group(4) != "✓":
            failed.append(f"step line marks a failure: {stripped}")
    for title in expected:
        times = seen.get(title, 0)
        if times != 1:
            problems.append(f"{title!r} has {times} step lines, expected 1")
    return problems + failed


def terminal_noise(text: str) -> list[str]:
    """The lines a compact terminal should not show: step headers without a
    result mark, and the background marker, both of which belong in the log.
    """
    noise: list[str] = []
    for line in text.splitlines():
        stripped = line.rstrip()
        if BACKGROUND_SUFFIX.strip() in stripped:
            noise.append(f"background marker on the terminal: {stripped}")
        elif (
            HEADER.fullmatch(stripped) is not None
            and STEP_LINE.fullmatch(stripped) is None
        ):
            noise.append(f"step header on the terminal: {stripped}")
    return noise


def next_steps(text: str) -> list[str]:
    """The `  - ` items listed under the last `Next steps:` line."""
    items: list[str] = []
    listing = False
    for line in text.splitlines():
        if line.rstrip() == "Next steps:":
            items, listing = [], True
        elif listing and line.startswith("  - "):
            items.append(line[4:].rstrip())
        else:
            listing = False
    return items


def identity_warning_problems(terminal: str, log: str) -> list[str]:
    """Each git identity warning missing from the terminal's next steps or
    from the log's `warning:` lines."""
    listed = next_steps(terminal)
    logged = [line for line in log.splitlines() if line.startswith("warning: ")]
    problems: list[str] = []
    for warning in IDENTITY_WARNINGS:
        if not any(warning in item for item in listed):
            problems.append(f"the terminal's next steps lack {warning!r}")
        if not any(warning in line for line in logged):
            problems.append(f"the log has no warning line with {warning!r}")
    return problems


def terminal_line_budget(steps: int) -> int:
    """The most lines a compact run may print for `steps` steps: one per
    step, the opening line, the tally, `Next steps:` and its allowance.
    """
    return steps + 3 + len(IDENTITY_WARNINGS) + NEXT_STEPS_ALLOWANCE


def terminal_line_count(text: str) -> int:
    """The non-blank terminal lines that count against the budget. A failed
    step's indented error lines are left to the summary check."""
    return sum(
        1
        for line in text.splitlines()
        if line.strip() and not line.startswith(FAILURE_INDENT)
    )


def count_lines(text: str, wanted: str) -> int:
    """How many lines of `text` are exactly `wanted`, ignoring trailing space."""
    return sum(line.rstrip() == wanted for line in text.splitlines())


def lines_mentioning(text: str, needle: str) -> list[str]:
    """The lines of `text` that mention `needle`, ignoring case."""
    return [line for line in text.splitlines() if needle.lower() in line.lower()]


def format_seconds(seconds: float) -> str:
    return f"{seconds:.1f} s"


def parse_bind_addr(config: str) -> tuple[str, int]:
    """The host and port in a code-server `config.yaml`."""
    for line in config.splitlines():
        match = re.fullmatch(r"\s*bind-addr:\s*(\S+)\s*", line)
        if match is None:
            continue
        value = match.group(1).strip("'\"")
        host, separator, port = value.rpartition(":")
        if not separator or not port.isdigit():
            raise ValueError(f"bind-addr is not host:port: {value}")
        return host, int(port)
    raise ValueError("config.yaml has no bind-addr")


def probe_host(host: str) -> str:
    """The address to probe for a server bound to `host`."""
    if host in ("", "0.0.0.0", "[::]", "::", "*"):
        return "127.0.0.1"
    return host


def http_ok(status: str) -> bool:
    """True for a 2xx or 3xx status (a login redirect means the server is up)."""
    status = status.strip()
    return status.isdigit() and 200 <= int(status) < 400


def json_mismatches(text: str, expected: dict[str, object]) -> list[str]:
    """Each expected key that the JSON object in `text` lacks or disagrees on."""
    try:
        value = json.loads(text)
    except json.JSONDecodeError as error:
        return [f"not valid JSON: {error}"]
    if not isinstance(value, dict):
        return ["not a JSON object"]
    return [
        f"{key} is {value.get(key)!r}, expected {want!r}"
        for key, want in expected.items()
        if value.get(key) != want
    ]


def missing_extensions(listing: str, wanted: tuple[str, ...]) -> list[str]:
    """The wanted extension ids absent from `code-server --list-extensions`."""
    installed = {line.split("@")[0].strip().lower() for line in listing.splitlines()}
    return [extension for extension in wanted if extension.lower() not in installed]


def boot_finished(state: str) -> bool:
    """True once `systemctl is-system-running` reports a settled boot."""
    return state.strip() in ("running", "degraded")


@dataclass
class CheckResult:
    name: str
    status: str
    detail: str = ""
    # A short note shown in the table, such as a run's duration.
    note: str = ""


def render_table(results: list[CheckResult]) -> str:
    """The per-check pass/fail table printed at the end of a run."""
    if not results:
        return "no checks ran"
    width = max(len(result.name) for result in results)
    lines = [f"{'check'.ljust(width)}  result", f"{'-' * width}  ------"]
    lines += [
        f"{result.name.ljust(width)}  {result.status}"
        + (f"  {result.note}" if result.note else "")
        for result in results
    ]
    failed = sum(result.status == "FAIL" for result in results)
    lines.append(f"{len(results)} checks, {failed} failed")
    return "\n".join(lines)


# Docker plumbing.


@dataclass(frozen=True)
class Output:
    code: int
    text: str


class Container:
    def __init__(self, docker: str, name: str) -> None:
        self.docker = docker
        self.name = name

    def exec(
        self,
        command: list[str],
        *,
        root: bool = False,
        timeout: float = 300,
        stream: bool = False,
    ) -> Output:
        argv = [self.docker, "exec"]
        if not root:
            argv += ["--user", USER, "--workdir", HOME]
            for key, value in LOGIN_ENV.items():
                argv += ["--env", f"{key}={value}"]
        argv += [self.name, *command]
        if stream:
            return run_streaming(argv, timeout)
        try:
            completed = subprocess.run(
                argv,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                timeout=timeout,
                check=False,
            )
        except subprocess.TimeoutExpired as error:
            text = decode(error.output or b"")
            return Output(124, f"{text}\ntimed out after {timeout:.0f}s")
        return Output(completed.returncode, decode(completed.stdout))

    def shell(self, script: str) -> Output:
        return self.exec(["sh", "-c", script])


def decode(data: bytes) -> str:
    return data.decode("utf-8", errors="replace")


def run_streaming(argv: list[str], timeout: float) -> Output:
    """Run `argv`, echoing its output live, and return the captured text."""
    print("+", shlex.join(argv), flush=True)
    process = subprocess.Popen(argv, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    timed_out = threading.Event()

    def kill() -> None:
        timed_out.set()
        process.kill()

    timer = threading.Timer(timeout, kill)
    timer.start()
    lines: list[str] = []
    try:
        assert process.stdout is not None
        for raw in process.stdout:
            line = decode(raw)
            lines.append(line)
            sys.stdout.write(line)
            sys.stdout.flush()
        code = process.wait()
    finally:
        timer.cancel()
    text = "".join(lines)
    if timed_out.is_set():
        return Output(124, f"{text}\ntimed out after {timeout:.0f}s")
    return Output(code, text)


def run(command: list[str]) -> None:
    print("+", shlex.join(command), flush=True)
    subprocess.run(command, check=True)


def retry(
    attempt: Callable[[], tuple[bool, str]], tries: int, delay: float
) -> tuple[bool, str]:
    ok, detail = False, ""
    for index in range(tries):
        ok, detail = attempt()
        if ok:
            break
        if index + 1 < tries:
            time.sleep(delay)
    return ok, detail


# Checks.


class Checks:
    def __init__(self, container: Container) -> None:
        self.container = container
        self.results: list[CheckResult] = []

    def record(
        self,
        name: str,
        ok: bool,
        detail: str,
        *,
        required: bool = True,
        note: str = "",
    ) -> bool:
        status = "PASS" if ok else ("FAIL" if required else "WARN")
        self.results.append(CheckResult(name, status, detail, note))
        if not ok:
            print(f"--- {status}: {name}", flush=True)
            print(detail.rstrip() or "(no output)", flush=True)
            print("---", flush=True)
        return ok

    def command(
        self,
        name: str,
        script: str,
        *,
        expect: str | None = None,
        required: bool = True,
    ) -> Output:
        """Run `script` as the login user; it passes on exit 0 (and `expect` in its output)."""
        output = self.container.shell(script)
        ok = output.code == 0 and (expect is None or expect in output.text)
        detail = f"$ {script}\nexit {output.code}\n{output.text}"
        if expect is not None and expect not in output.text:
            detail += f"\nexpected output to contain {expect!r}"
        self.record(name, ok, detail, required=required)
        return output

    def service(self, name: str, unit: str, *, required: bool = True) -> None:
        """Pass when systemd reports `unit` active; show its status and log otherwise.

        Runs as root, which talks to systemd directly rather than over D-Bus.
        """
        active = self.container.exec(["systemctl", "is-active", unit], root=True)
        detail = f"$ systemctl is-active {unit}\nexit {active.code}\n{active.text}"
        if active.code != 0:
            status = self.container.exec(
                ["systemctl", "status", unit, "--no-pager"], root=True
            )
            journal = self.container.exec(
                ["journalctl", "--unit", unit, "--lines", "50", "--no-pager"],
                root=True,
            )
            detail += f"\n{status.text}\n{journal.text}"
        self.record(name, active.code == 0, detail, required=required)

    def setup_summary(self, name: str, expected: list[str], run: SetupRun) -> None:
        """Check the summary in the run's log section: every planned step ✓."""
        problems: list[str] = []
        if run.output.code != 0:
            problems.append(f"setup exited {run.output.code}")
        if run.log_problem:
            problems.append(run.log_problem)
        else:
            entries = parse_summary(run.log)
            problems += summary_problems(expected, entries)
            problems += summary_order_problems(expected, entries)
        detail = "\n".join(problems)
        if problems:
            detail += f"\n\nsetup output:\n{run.output.text}"
        self.record(name, not problems, detail)

    def setup_output(
        self, prefix: str, expected: list[str], run: SetupRun, *, first: bool
    ) -> None:
        """Check what a setup run printed and logged, and report how long it took."""
        terminal = run.output.text
        problems = terminal_step_line_problems(terminal, expected) + terminal_noise(
            terminal
        )
        self.record(
            f"{prefix}: one step line per step",
            not problems,
            "\n".join(problems) + f"\n\nsetup output:\n{terminal}",
        )

        budget = terminal_line_budget(len(expected))
        used = terminal_line_count(terminal)
        self.record(
            f"{prefix}: terminal line budget",
            used <= budget,
            f"the terminal shows {used} lines, over the budget of {budget}"
            f"\n\nsetup output:\n{terminal}",
            note=f"{used}/{budget} lines",
        )

        sections = 1 if first else 2
        if run.log_problem:
            log_problems = [run.log_problem]
        else:
            log_problems = header_problems(run.log, expected) + step_line_problems(
                run.log, expected
            )
            if run.sections != sections:
                log_problems.append(
                    f"{SETUP_LOG} holds {run.sections} run section(s), expected {sections}"
                )
        self.record(
            f"{prefix}: log records every step",
            not log_problems,
            "\n".join(log_problems) + f"\n\n{SETUP_LOG}, last run:\n{run.log}",
        )

        refreshes = FIRST_RUN_REFRESHES if first else RERUN_REFRESHES
        count = count_lines(run.log, REFRESH_ANNOUNCEMENT)
        self.record(
            f"{prefix}: apt refresh count",
            not run.log_problem and count == refreshes,
            run.log_problem
            or f"{REFRESH_ANNOUNCEMENT!r} appears {count} time(s) in the log, "
            f"expected {refreshes}\n\n{SETUP_LOG}, last run:\n{run.log}",
        )

        # The log holds every command's output, which may mention the host
        # for unrelated reasons, so only the logged command lines count.
        logged_commands = "\n".join(
            line for line in run.log.splitlines() if line.startswith("$ ")
        )
        mentions = lines_mentioning(terminal, CODE_SERVER_SCRIPT_HOST) + (
            lines_mentioning(logged_commands, CODE_SERVER_SCRIPT_HOST)
        )
        self.record(
            f"{prefix}: no code-server install script",
            not mentions,
            f"the output or log mentions {CODE_SERVER_SCRIPT_HOST}:\n"
            + "\n".join(mentions),
        )

        took = format_seconds(run.seconds)
        within = not first or run.seconds <= FIRST_RUN_BUDGET
        self.record(
            f"{prefix}: run time",
            within,
            f"took {took}, over the {FIRST_RUN_BUDGET} s budget for a fresh setup",
            required=False,
            note=took,
        )

    def http(self, name: str, host: str, port: int) -> None:
        url = f"http://{probe_host(host)}:{port}/"
        script = (
            f"curl -sS -o /dev/null -w '%{{http_code}}' --max-time 5 {shlex.quote(url)}"
        )

        def attempt() -> tuple[bool, str]:
            output = self.container.shell(script)
            detail = f"$ {script}\nexit {output.code}\n{output.text}"
            return http_ok(output.text), detail

        ok, detail = retry(attempt, tries=30, delay=2)
        self.record(name, ok, detail)

    def json_file(self, name: str, path: str, expected: dict[str, object]) -> None:
        output = self.container.exec(["cat", path])
        if output.code != 0:
            self.record(name, False, f"could not read {path}:\n{output.text}")
            return
        problems = json_mismatches(output.text, expected)
        self.record(
            name, not problems, "\n".join(problems) + f"\n\n{path}:\n{output.text}"
        )


def smoke_checks(checks: Checks, first_run: SetupRun) -> None:
    c = checks.container

    # Docker Engine. The packages' postinst starts dockerd, which depends on
    # the host kernel and storage driver, so its state is reported but not
    # required.
    checks.command("docker: CLI", "docker --version")
    groups = c.exec(["id", "-nG", USER])
    checks.record(
        "docker: user in docker group",
        groups.code == 0 and "docker" in groups.text.split(),
        f"$ id -nG {USER}\nexit {groups.code}\n{groups.text}",
    )
    checks.service("docker: service active", "docker", required=False)

    # Rust via rustup. The login PATH does not include ~/.cargo/bin until the
    # next login sources ~/.profile, so the binaries are called directly.
    checks.command("rust: rustc", f"{HOME}/.cargo/bin/rustc --version")
    checks.command("rust: cargo", f"{HOME}/.cargo/bin/cargo --version")
    checks.command(
        "rust: stable is the default",
        f"{HOME}/.cargo/bin/rustup default",
        expect="stable",
    )

    # R and the Posit Package Manager repository.
    checks.command(
        "r: Rscript", "Rscript -e 'cat(R.version.string)'", expect="R version"
    )
    profile = c.exec(["cat", R_PROFILE])
    count = profile.text.count(R_REPOSITORY) if profile.code == 0 else 0
    checks.record(
        "r: Rprofile.site has the repository once",
        count == 1,
        f"{R_PROFILE} mentions {R_REPOSITORY} {count} time(s):\n{profile.text}",
    )
    checks.command(
        "r: repos option uses Posit Package Manager",
        'Rscript -e \'cat(getOption("repos"), sep = "\\n")\'',
        expect=R_REPOSITORY,
    )

    # Python and pip.
    checks.command("python: python3", "python3 --version")
    checks.command(
        "python: venv",
        f"python3 -m venv {VENV_CHECK} && {VENV_CHECK}/bin/python -m pip --version; "
        f"status=$?; rm -rf {VENV_CHECK}; exit $status",
        expect="pip ",
    )
    checks.command("python: pip index", "python3 -m pip config list", expect=PIP_INDEX)

    checks.command("quarto", "quarto --version")

    tlmgr = " ".join(shlex.quote(candidate) for candidate in TLMGR_CANDIDATES)
    checks.command(
        "tinytex: tlmgr",
        f'for t in {tlmgr}; do if command -v "$t" >/dev/null; then exec "$t" --version; fi; done; '
        "echo 'tlmgr not found'; exit 1",
        expect="TeX Live",
    )

    # RStudio Server.
    checks.command("rstudio-server: version", "rstudio-server version")
    checks.service("rstudio-server: service active", "rstudio-server")
    checks.http("rstudio-server: HTTP", "127.0.0.1", RSTUDIO_PORT)
    checks.json_file("rstudio-server: preferences", RSTUDIO_PREFS, RSTUDIO_PREFERENCES)

    # code-server.
    checks.command("code-server: version", "code-server --version")
    checks.service("code-server: service active", f"code-server@{USER}")

    def read_config() -> tuple[bool, str]:
        output = c.exec(["cat", CODE_SERVER_CONFIG])
        return output.code == 0, output.text

    found, config = retry(read_config, tries=15, delay=2)
    try:
        if not found:
            raise ValueError(f"{CODE_SERVER_CONFIG} was not written:\n{config}")
        host, port = parse_bind_addr(config)
    except ValueError as error:
        checks.record("code-server: HTTP", False, str(error))
    else:
        checks.http("code-server: HTTP", host, port)
    checks.json_file(
        "code-server: managed settings",
        CODE_SERVER_SETTINGS,
        {
            "quarto.path": f"{HOME}/.local/bin/quarto",
            "chat.disableAIFeatures": True,
            "telemetry.telemetryLevel": "off",
        },
    )
    listing = c.exec(["code-server", "--list-extensions"])
    absent = (
        missing_extensions(listing.text, CODE_SERVER_EXTENSIONS)
        if listing.code == 0
        else list(CODE_SERVER_EXTENSIONS)
    )
    checks.record(
        "code-server: extensions",
        not absent,
        f"missing {absent}\n$ code-server --list-extensions\nexit {listing.code}\n{listing.text}",
    )

    checks.command("gh", "gh --version")
    checks.command("duckdb", "duckdb --version")
    checks.command("uv", "uv --version")
    checks.command("uv: ruff", "ruff --version")
    checks.command("uv: sqlfluff", "sqlfluff --version")
    checks.command("rig", "rig --version")

    # git defaults, with no identity configured.
    checks.command(
        "git: init.defaultBranch is main",
        'b="$(git config --global init.defaultBranch)"; echo "$b"; test "$b" = main',
    )
    checks.command(
        "git: excludes file vaccinated",
        'f="$(git config --global core.excludesFile)" && test -n "$f" && cat "$f"',
        expect=VACCINATE_MARKER,
    )
    # A pending reboot or other next steps may be listed too; only the
    # identity warnings are required.
    identity = (
        [first_run.log_problem]
        if first_run.log_problem
        else identity_warning_problems(first_run.output.text, first_run.log)
    )
    checks.record(
        "git: missing identity warnings",
        not identity,
        "\n".join(identity) + f"\n\nsetup output:\n{first_run.output.text}",
    )


# Container lifecycle.


def wait_for_boot(container: Container, timeout: float) -> tuple[bool, str]:
    """Wait for systemd to settle; on timeout, describe the failed units."""
    deadline = time.monotonic() + timeout
    state = ""
    while time.monotonic() < deadline:
        output = container.exec(
            ["systemctl", "is-system-running"], root=True, timeout=30
        )
        state = output.text.strip()
        if boot_finished(state):
            return True, f"systemd is {state}"
        time.sleep(2)
    failed = container.exec(["systemctl", "--failed", "--no-pager"], root=True)
    return False, (
        f"systemd did not finish booting within {timeout:.0f}s "
        f"(last state: {state!r})\n{failed.text}"
    )


def install_hpds(docker: str, container: Container, binary: Path) -> None:
    target = f"{HOME}/.local/bin"
    run([docker, "exec", container.name, "mkdir", "-p", target])
    run([docker, "cp", str(binary), f"{container.name}:{target}/hpds"])
    run([docker, "exec", container.name, "chmod", "0755", f"{target}/hpds"])
    run(
        [
            docker,
            "exec",
            container.name,
            "chown",
            "-R",
            f"{USER}:{USER}",
            f"{HOME}/.local",
        ]
    )


@dataclass(frozen=True)
class SetupRun:
    # What the run printed on the terminal.
    output: Output
    seconds: float
    # The run's own section of the setup log, from its header line.
    log: str = ""
    # How many run sections the whole log holds.
    sections: int = 0
    # Why the log could not be read or split, if it could not.
    log_problem: str = ""


def read_setup_log(text: str, code: int) -> tuple[str, int, str]:
    """The last run section of the log, the number of sections, and a
    problem when `cat` failed (`code`) or the log holds no run."""
    if code != 0:
        return "", 0, f"{SETUP_LOG} is missing: {text.strip()}"
    try:
        return last_log_section(text), len(log_sections(text)), ""
    except ValueError as error:
        return "", 0, f"{SETUP_LOG}: {error}"


def run_setup(container: Container, timeout: float, label: str) -> SetupRun:
    """Run setup once, timing it and printing the time on a line of its own,
    then read its section of the log, which each run appends to."""
    start = time.monotonic()
    output = container.exec(SETUP_COMMAND, timeout=timeout, stream=True)
    seconds = time.monotonic() - start
    print(f"{label} took {format_seconds(seconds)}", flush=True)
    log = container.exec(["cat", SETUP_LOG])
    section, sections, problem = read_setup_log(log.text, log.code)
    return SetupRun(output, seconds, section, sections, problem)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument(
        "--hpds", type=Path, required=True, help="the Linux x86_64 hpds binary to test"
    )
    parser.add_argument(
        "--platform",
        help="docker --platform for the image and container, such as linux/amd64",
    )
    parser.add_argument("--docker", default="docker", help="the docker executable")
    parser.add_argument(
        "--keep-image", action="store_true", help="keep the test image after the run"
    )
    parser.add_argument(
        "--boot-timeout",
        type=float,
        default=180,
        help="seconds to wait for systemd to boot",
    )
    parser.add_argument(
        "--setup-timeout",
        type=float,
        default=50 * 60,
        help="seconds allowed for the first setup run",
    )
    parser.add_argument(
        "--rerun-timeout",
        type=float,
        default=20 * 60,
        help="seconds allowed for the second, idempotent setup run",
    )
    parser.add_argument(
        "--stop-after-plan",
        action="store_true",
        help="stop after the plan check; for hosts that cannot run the full setup",
    )
    return parser.parse_args(argv)


def exit_on_signal(signum: int, _frame: object) -> None:
    """Turn a termination signal into SystemExit so cleanup still runs."""
    raise SystemExit(128 + signum)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    signal.signal(signal.SIGTERM, exit_on_signal)
    binary = args.hpds.resolve()
    if not binary.is_file():
        print(f"hpds binary not found: {binary}", file=sys.stderr)
        return 2
    platform = ["--platform", args.platform] if args.platform else []
    name = f"hpds-server-setup-{int(time.time())}"
    container = Container(args.docker, name)
    checks = Checks(container)
    try:
        run([args.docker, "build", *platform, "--tag", IMAGE_TAG, str(HERE)])
        run(
            [
                args.docker,
                "run",
                "--detach",
                "--name",
                name,
                *platform,
                "--privileged",
                "--cgroupns=host",
                "--volume",
                "/sys/fs/cgroup:/sys/fs/cgroup:rw",
                # Give the Docker Engine under test storage that is not
                # itself overlayfs.
                "--volume",
                "/var/lib/docker",
                "--volume",
                "/var/lib/containerd",
                IMAGE_TAG,
            ]
        )
        booted, boot_detail = wait_for_boot(container, args.boot_timeout)
        if not checks.record("systemd boots", booted, boot_detail):
            return finish(checks)
        install_hpds(args.docker, container, binary)

        plan = container.exec(PLAN_COMMAND)
        try:
            titles = parse_plan(plan.text) if plan.code == 0 else []
            plan_problem = "" if plan.code == 0 else f"exit {plan.code}"
        except ValueError as error:
            titles, plan_problem = [], str(error)
        checks.record(
            "setup plan lists the steps",
            bool(titles) and not plan_problem,
            f"{plan_problem}\n{plan.text}",
        )
        if args.stop_after_plan or not titles:
            return finish(checks)

        first = run_setup(container, args.setup_timeout, "setup")
        checks.setup_summary("setup: every step succeeds", titles, first)
        checks.setup_output("setup", titles, first, first=True)
        smoke_checks(checks, first)
        second = run_setup(container, args.rerun_timeout, "setup again")
        checks.setup_summary("setup again: every step succeeds", titles, second)
        checks.setup_output("setup again", titles, second, first=False)
        return finish(checks)
    finally:
        subprocess.run(
            [args.docker, "rm", "--force", "--volumes", name],
            check=False,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        if not args.keep_image:
            subprocess.run(
                [args.docker, "image", "rm", "--force", IMAGE_TAG],
                check=False,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )


def finish(checks: Checks) -> int:
    print(render_table(checks.results), flush=True)
    return 1 if any(result.status == "FAIL" for result in checks.results) else 0


if __name__ == "__main__":
    sys.exit(main())
