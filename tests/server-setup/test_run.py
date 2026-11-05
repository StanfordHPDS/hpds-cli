from __future__ import annotations

import contextlib
import importlib.util
import io
from pathlib import Path
import sys
from typing import ClassVar
import unittest


MODULE_PATH = Path(__file__).with_name("run.py")
SPEC = importlib.util.spec_from_file_location("server_setup_run", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
RUN = importlib.util.module_from_spec(SPEC)
# dataclasses look their module up in sys.modules while the module loads.
sys.modules[SPEC.name] = RUN
SPEC.loader.exec_module(RUN)

REPO = Path(__file__).resolve().parents[2]
SNAPSHOT = (
    REPO
    / "tests"
    / "snapshots"
    / "setup__plan_server_prints_the_numbered_plan_on_every_os.snap"
)
SETUP_SNAPSHOTS = REPO / "src" / "setup" / "snapshots"


def snapshot_body(path: Path = SNAPSHOT) -> str:
    """The text recorded by a Rust snapshot test, without its header."""
    text = path.read_text()
    return text.split("---\n", 2)[2]


def setup_snapshot(name: str) -> str:
    """A compact setup run recorded by the Rust tests in `src/setup`."""
    return snapshot_body(SETUP_SNAPSHOTS / f"hpds__setup__tests__{name}.snap")


# The steps of the recorded compact server runs, in plan order.
RECORDED_PLAN = ["foreground first", "background waits", "foreground release"]


class PlanParsingTests(unittest.TestCase):
    def test_parses_the_recorded_server_plan(self) -> None:
        titles = RUN.parse_plan(snapshot_body())
        self.assertEqual(titles[0], "system updates (apt upgrade)")
        self.assertEqual(titles[-1], "git defaults")
        self.assertIn("R + CRAN/PPM repositories", titles)
        self.assertIn("code-server + extensions", titles)

    def test_title_count_matches_the_header(self) -> None:
        body = snapshot_body()
        announced = int(body.split("runs these ", 1)[1].split(" ", 1)[0])
        self.assertEqual(len(RUN.parse_plan(body)), announced)

    def test_command_lines_are_not_titles(self) -> None:
        titles = RUN.parse_plan(snapshot_body())
        self.assertFalse(any(title.startswith("sudo ") for title in titles))
        self.assertFalse(any(title.startswith("hpds ") for title in titles))

    def test_rejects_a_missing_header(self) -> None:
        with self.assertRaises(ValueError):
            RUN.parse_plan("  1. one\n")

    def test_rejects_a_count_that_disagrees_with_the_header(self) -> None:
        with self.assertRaises(ValueError):
            RUN.parse_plan(
                "hpds setup --profile server runs these 3 steps:\n  1. a\n  2. b\n"
            )

    def test_rejects_out_of_order_numbering(self) -> None:
        with self.assertRaises(ValueError):
            RUN.parse_plan(
                "hpds setup --profile server runs these 2 steps:\n  1. a\n  3. b\n"
            )


class SummaryTests(unittest.TestCase):
    PLAN: ClassVar[list[str]] = [
        "system libraries (apt)",
        "Docker Engine",
        "git defaults",
    ]

    def test_parses_passing_and_failing_lines(self) -> None:
        entries = RUN.parse_summary(
            "setup summary:\n  ✓ Docker Engine\n  ✗ git defaults: boom: detail\n"
        )
        self.assertEqual(
            entries,
            [
                RUN.SummaryEntry(True, "Docker Engine"),
                RUN.SummaryEntry(False, "git defaults: boom: detail"),
            ],
        )

    def test_ignores_check_marks_outside_the_summary(self) -> None:
        text = (
            "[1/3] system libraries (apt)\n"
            "  ✓ not a summary line\n"
            "setup summary:\n"
            "  ✓ system libraries (apt)\n"
            "All 1 step succeeded. Details: /home/nero/hpds-setup.log\n"
            "  ✓ trailing\n"
        )
        self.assertEqual(
            RUN.parse_summary(text), [RUN.SummaryEntry(True, "system libraries (apt)")]
        )

    def test_a_multi_line_error_does_not_end_the_summary(self) -> None:
        text = (
            "setup summary:\n"
            "  ✓ system libraries (apt)\n"
            "  ✗ Docker Engine: apt-get failed\n"
            "E: Unable to locate package docker-ce\n"
            "\n"
            "  ✓ git defaults\n"
        )
        entries = RUN.parse_summary(text)
        self.assertEqual(len(entries), 3)
        self.assertIn("Unable to locate package", entries[1].text)
        self.assertEqual(
            RUN.summary_problems(self.PLAN, entries),
            [
                (
                    "failed: Docker Engine: apt-get failed\n"
                    "E: Unable to locate package docker-ce"
                )
            ],
        )

    def test_output_after_a_failed_last_step_is_not_part_of_it(self) -> None:
        text = (
            "setup summary:\n"
            "  ✗ git defaults: boom\n"
            "0 of 1 step succeeded. Details: /home/nero/hpds-setup.log\n"
            "error: 1 of 1 setup steps failed\n"
        )
        self.assertEqual(
            RUN.parse_summary(text), [RUN.SummaryEntry(False, "git defaults: boom")]
        )

    def test_the_tally_ends_a_multi_line_error(self) -> None:
        text = (
            "setup summary:\n"
            "  ✓ system libraries (apt)\n"
            "  ✗ Docker Engine: `apt-get` failed: E: Unable to locate package\n"
            "full output: /home/nero/hpds-setup.log\n"
            "1 of 2 steps succeeded. Details: /home/nero/hpds-setup.log\n"
        )
        self.assertEqual(
            RUN.parse_summary(text),
            [
                RUN.SummaryEntry(True, "system libraries (apt)"),
                RUN.SummaryEntry(
                    False,
                    "Docker Engine: `apt-get` failed: E: Unable to locate package\n"
                    "full output: /home/nero/hpds-setup.log",
                ),
            ],
        )

    def test_parses_the_summary_of_a_recorded_log(self) -> None:
        entries = RUN.parse_summary(setup_snapshot("compact_background_log"))
        self.assertEqual(
            entries, [RUN.SummaryEntry(True, title) for title in RECORDED_PLAN]
        )
        self.assertEqual(RUN.summary_problems(RECORDED_PLAN, entries), [])

    def test_all_passing_has_no_problems(self) -> None:
        entries = [RUN.SummaryEntry(True, title) for title in self.PLAN]
        self.assertEqual(RUN.summary_problems(self.PLAN, entries), [])

    def test_reports_a_failed_step_with_its_error(self) -> None:
        entries = [
            RUN.SummaryEntry(True, "system libraries (apt)"),
            RUN.SummaryEntry(False, "Docker Engine: USER is not set"),
            RUN.SummaryEntry(True, "git defaults"),
        ]
        self.assertEqual(
            RUN.summary_problems(self.PLAN, entries),
            ["failed: Docker Engine: USER is not set"],
        )

    def test_reports_a_planned_step_missing_from_the_summary(self) -> None:
        entries = [RUN.SummaryEntry(True, title) for title in self.PLAN[:2]]
        problems = RUN.summary_problems(self.PLAN, entries)
        self.assertIn("missing from the summary: git defaults", problems)
        self.assertTrue(any("lists 2 steps" in problem for problem in problems))

    def test_reports_a_step_the_plan_does_not_list(self) -> None:
        entries = [RUN.SummaryEntry(True, title) for title in [*self.PLAN, "extra"]]
        self.assertIn(
            "not in the plan: ✓ extra", RUN.summary_problems(self.PLAN, entries)
        )

    def test_an_empty_summary_is_a_problem(self) -> None:
        self.assertEqual(
            RUN.summary_problems(self.PLAN, []), ["the setup summary lists no steps"]
        )

    def test_title_prefix_alone_does_not_match(self) -> None:
        entries = [RUN.SummaryEntry(True, "Docker Engine extra")]
        self.assertIn(
            "missing from the summary: Docker Engine",
            RUN.summary_problems(["Docker Engine"], entries),
        )


class SummaryOrderTests(unittest.TestCase):
    PLAN: ClassVar[list[str]] = SummaryTests.PLAN

    def test_plan_order_has_no_problems(self) -> None:
        entries = [
            RUN.SummaryEntry(True, "system libraries (apt)"),
            RUN.SummaryEntry(False, "Docker Engine: boom"),
            RUN.SummaryEntry(True, "git defaults"),
        ]
        self.assertEqual(RUN.summary_order_problems(self.PLAN, entries), [])

    def test_reports_a_step_listed_before_an_earlier_one(self) -> None:
        entries = [
            RUN.SummaryEntry(True, "system libraries (apt)"),
            RUN.SummaryEntry(True, "git defaults"),
            RUN.SummaryEntry(True, "Docker Engine"),
        ]
        self.assertEqual(
            RUN.summary_order_problems(self.PLAN, entries),
            [
                (
                    "the summary lists git defaults before Docker Engine, "
                    "against the plan order"
                )
            ],
        )

    def test_unplanned_and_missing_entries_do_not_count_as_disorder(self) -> None:
        entries = [
            RUN.SummaryEntry(True, "system libraries (apt)"),
            RUN.SummaryEntry(True, "extra"),
            RUN.SummaryEntry(True, "git defaults"),
        ]
        self.assertEqual(RUN.summary_order_problems(self.PLAN, entries), [])

    def test_parses_a_log_section_after_step_one_warnings(self) -> None:
        text = (
            "=== hpds setup --profile server ===\n"
            "hpds version: 0.4.0\n"
            "[1/3] system libraries (apt)\n"
            "refreshing apt package lists (needs sudo)\n"
            "  will run: sudo env DEBIAN_FRONTEND=noninteractive apt-get update\n"
            "$ sudo env DEBIAN_FRONTEND=noninteractive apt-get update\n"
            "status: success\n"
            "warning: could not register the Docker apt repository (curl: (6) "
            "Could not resolve host)\n"
            "  setup summary: is not a header when indented\n"
            "waiting for the RStudio Server and code-server downloads to finish\n"
            "[1/3] system libraries (apt) ✓\n"
            "[2/3] Docker Engine\n"
            "[2/3] Docker Engine ✗\n"
            "[3/3] git defaults (ran in the background)\n"
            "[3/3] git defaults ✓\n"
            "setup summary:\n"
            "  ✓ system libraries (apt)\n"
            "  ✗ Docker Engine: apt-get failed\n"
            "E: Unable to locate package docker-ce\n"
            "E: Package 'docker-ce-cli' has no installation candidate\n"
            "  ✓ git defaults\n"
            "2 of 3 steps succeeded. Details: /home/nero/hpds-setup.log\n"
        )
        entries = RUN.parse_summary(text)
        self.assertEqual(
            entries,
            [
                RUN.SummaryEntry(True, "system libraries (apt)"),
                RUN.SummaryEntry(
                    False,
                    "Docker Engine: apt-get failed\n"
                    "E: Unable to locate package docker-ce\n"
                    "E: Package 'docker-ce-cli' has no installation candidate",
                ),
                RUN.SummaryEntry(True, "git defaults"),
            ],
        )
        self.assertEqual(RUN.header_problems(text, self.PLAN), [])
        self.assertEqual(
            RUN.step_line_problems(text, self.PLAN),
            ["step line marks a failure: [2/3] Docker Engine ✗"],
        )
        self.assertEqual(RUN.count_lines(text, RUN.REFRESH_ANNOUNCEMENT), 1)


class HeaderTests(unittest.TestCase):
    PLAN: ClassVar[list[str]] = SummaryTests.PLAN

    def test_one_header_per_step_passes_with_background_suffixes(self) -> None:
        text = (
            "[1/3] system libraries (apt)\n"
            "some output\n"
            "[3/3] git defaults (ran in the background)\n"
            "[2/3] Docker Engine\r\n"
        )
        self.assertEqual(RUN.header_problems(text, self.PLAN), [])

    def test_reports_a_missing_header(self) -> None:
        text = "[1/3] system libraries (apt)\n[3/3] git defaults\n"
        self.assertEqual(
            RUN.header_problems(text, self.PLAN),
            ["step 2 has 0 headers, expected 1"],
        )

    def test_reports_a_repeated_header(self) -> None:
        text = (
            "[1/3] system libraries (apt)\n"
            "[2/3] Docker Engine\n"
            "[2/3] Docker Engine (ran in the background)\n"
            "[3/3] git defaults\n"
        )
        self.assertEqual(
            RUN.header_problems(text, self.PLAN),
            ["step 2 has 2 headers, expected 1"],
        )

    def test_reports_the_wrong_step_count(self) -> None:
        text = "[1/3] system libraries (apt)\n[2/4] Docker Engine\n[3/3] git defaults\n"
        self.assertEqual(
            RUN.header_problems(text, self.PLAN),
            ["header counts 4 steps, the plan 3: [2/4] Docker Engine"],
        )

    def test_reports_a_wrong_title_and_a_number_outside_the_plan(self) -> None:
        text = (
            "[1/3] system libraries (apt)\n"
            "[2/3] Docker\n"
            "[3/3] git defaults\n"
            "[4/3] extra\n"
        )
        self.assertEqual(
            RUN.header_problems(text, self.PLAN),
            [
                "header 2 names 'Docker', the plan 'Docker Engine'",
                "header numbers a step outside the plan: [4/3] extra",
            ],
        )

    def test_indented_lines_are_not_headers(self) -> None:
        text = "  [1/3] system libraries (apt)\n"
        self.assertIn(
            "step 1 has 0 headers, expected 1", RUN.header_problems(text, self.PLAN)
        )

    def test_step_lines_are_not_headers(self) -> None:
        text = (
            "[1/3] system libraries (apt)\n"
            "[1/3] system libraries (apt) ✓\n"
            "[2/3] Docker Engine\n"
            "[2/3] Docker Engine ✗\n"
            "[3/3] git defaults\n"
            "[3/3] git defaults ✓\n"
        )
        self.assertEqual(RUN.header_problems(text, self.PLAN), [])

    def test_the_recorded_log_introduces_each_step_once(self) -> None:
        log = setup_snapshot("compact_background_log")
        self.assertEqual(RUN.header_problems(log, RECORDED_PLAN), [])
        self.assertIn("[2/3] background waits (ran in the background)", log)


class StepLineTests(unittest.TestCase):
    PLAN: ClassVar[list[str]] = SummaryTests.PLAN

    def test_the_recorded_log_numbers_step_lines_by_the_plan(self) -> None:
        log = setup_snapshot("compact_background_log")
        self.assertIn("[2/3] background waits ✓", log)

    def test_the_recorded_log_has_one_passing_line_per_step(self) -> None:
        log = setup_snapshot("compact_background_log")
        self.assertEqual(RUN.step_line_problems(log, RECORDED_PLAN), [])

    def test_reports_failures_in_a_recorded_terminal(self) -> None:
        terminal = setup_snapshot("compact_failure_terminal")
        plan = ["first", "git defaults", "second"]
        self.assertEqual(
            RUN.step_line_problems(terminal, plan),
            [
                "step line marks a failure: [2/3] git defaults ✗",
                "step line marks a failure: [3/3] second ✗",
            ],
        )

    def test_reports_missing_repeated_and_misnumbered_step_lines(self) -> None:
        text = (
            "[1/3] system libraries (apt) ✓\n"
            "[1/3] system libraries (apt) ✓\n"
            "[2/4] Docker Engine ✓\n"
            "[4/3] extra ✓\n"
        )
        self.assertEqual(
            RUN.step_line_problems(text, self.PLAN),
            [
                "step line counts 4 steps, the plan 3: [2/4] Docker Engine ✓",
                "step line numbers a step outside the plan: [4/3] extra ✓",
                "step 1 has 2 step lines, expected 1",
                "step 3 has 0 step lines, expected 1",
            ],
        )

    def test_reports_a_step_line_with_the_wrong_title(self) -> None:
        text = "[1/3] system libraries (apt) ✓\n[2/3] Docker ✓\n[3/3] git defaults ✓\n"
        self.assertEqual(
            RUN.step_line_problems(text, self.PLAN),
            ["step line 2 names 'Docker', the plan 'Docker Engine'"],
        )

    def test_headers_without_a_mark_are_not_step_lines(self) -> None:
        text = "[1/3] system libraries (apt)\n"
        self.assertIn(
            "step 1 has 0 step lines, expected 1",
            RUN.step_line_problems(text, self.PLAN),
        )


class TerminalStepLineTests(unittest.TestCase):
    PLAN: ClassVar[list[str]] = SummaryTests.PLAN

    def test_the_recorded_terminal_has_one_passing_line_per_step(self) -> None:
        terminal = setup_snapshot("compact_background_terminal")
        self.assertEqual(RUN.terminal_step_line_problems(terminal, RECORDED_PLAN), [])
        self.assertEqual(RUN.terminal_noise(terminal), [])

    def test_lines_in_completion_order_pass_when_counts_ascend(self) -> None:
        text = (
            "[1/3] git defaults ✓\n"
            "[2/3] system libraries (apt) ✓\n"
            "[3/3] Docker Engine ✓\n"
        )
        self.assertEqual(RUN.terminal_step_line_problems(text, self.PLAN), [])

    def test_reports_counts_that_do_not_rise_by_one(self) -> None:
        text = (
            "[1/3] system libraries (apt) ✓\n"
            "[3/3] Docker Engine ✓\n"
            "[2/3] git defaults ✓\n"
        )
        self.assertEqual(
            RUN.terminal_step_line_problems(text, self.PLAN),
            [
                "step line 2 is numbered 3: [3/3] Docker Engine ✓",
                "step line 3 is numbered 2: [2/3] git defaults ✓",
            ],
        )

    def test_reports_missing_repeated_unknown_and_miscounted_lines(self) -> None:
        text = (
            "[1/3] system libraries (apt) ✓\n"
            "[2/3] system libraries (apt) ✓\n"
            "[3/4] extra ✓\n"
        )
        self.assertEqual(
            RUN.terminal_step_line_problems(text, self.PLAN),
            [
                "step line counts 4 steps, the plan 3: [3/4] extra ✓",
                "step line names a step outside the plan: [3/4] extra ✓",
                "'system libraries (apt)' has 2 step lines, expected 1",
                "'Docker Engine' has 0 step lines, expected 1",
                "'git defaults' has 0 step lines, expected 1",
            ],
        )

    def test_reports_failures_in_a_recorded_terminal(self) -> None:
        terminal = setup_snapshot("compact_background_fails_first_terminal")
        plan = ["waits for background", "background fails", "last"]
        self.assertEqual(
            RUN.terminal_step_line_problems(terminal, plan),
            ["step line marks a failure: [1/3] background fails ✗"],
        )


class TerminalTests(unittest.TestCase):
    PLAN: ClassVar[list[str]] = SummaryTests.PLAN

    def test_flags_headers_and_the_background_marker(self) -> None:
        text = (
            "hpds setup --profile server: 3 steps. Full log: /home/nero/hpds-setup.log\n"
            "[1/3] system libraries (apt)\n"
            "[1/3] system libraries (apt) ✓\n"
            "[3/3] git defaults (ran in the background)\n"
            "[3/3] git defaults ✓\n"
        )
        self.assertEqual(
            RUN.terminal_noise(text),
            [
                "step header on the terminal: [1/3] system libraries (apt)",
                (
                    "background marker on the terminal: "
                    "[3/3] git defaults (ran in the background)"
                ),
            ],
        )

    def test_the_recorded_log_is_not_a_compact_terminal(self) -> None:
        noise = RUN.terminal_noise(setup_snapshot("compact_background_log"))
        self.assertIn("step header on the terminal: [1/3] foreground first", noise)
        self.assertIn(
            "background marker on the terminal: "
            "[2/3] background waits (ran in the background)",
            noise,
        )

    def test_reads_the_recorded_next_steps(self) -> None:
        self.assertEqual(
            RUN.next_steps(setup_snapshot("compact_dev_terminal")),
            [
                (
                    "user.name is not set; run `git config --global user.name <VALUE>` "
                    "or `hpds git setup`"
                ),
                "gh is installed but not logged in to GitHub; run `gh auth login`",
            ],
        )

    def test_next_steps_end_at_the_first_other_line(self) -> None:
        text = (
            "Next steps:\n"
            "  - one\n"
            "error: 1 of 3 setup steps failed\n"
            "  - not a next step\n"
        )
        self.assertEqual(RUN.next_steps(text), ["one"])
        self.assertEqual(RUN.next_steps("  - stray\n"), [])

    def test_the_budget_counts_steps_and_the_ending(self) -> None:
        self.assertEqual(
            RUN.terminal_line_budget(16),
            16 + 3 + len(RUN.IDENTITY_WARNINGS) + RUN.NEXT_STEPS_ALLOWANCE,
        )

    def test_the_line_count_skips_blank_lines_and_failure_details(self) -> None:
        terminal = setup_snapshot("compact_failure_terminal")
        # The opening line, three step lines, and the tally.
        self.assertEqual(RUN.terminal_line_count(terminal), 5)
        self.assertEqual(RUN.terminal_line_count("a\n\n  b\n"), 2)


IDENTITY_TERMINAL = (
    "All 16 steps succeeded. Details: /home/nero/hpds-setup.log\n"
    "Next steps:\n"
    "  - a reboot is pending to finish the system updates; run `sudo reboot`\n"
    "  - user.name is not set; run `git config --global user.name <VALUE>` "
    "or `hpds git setup`\n"
    "  - user.email is not set; run `git config --global user.email <VALUE>` "
    "or `hpds git setup`\n"
)
IDENTITY_LOG = (
    "warning: user.name is not set; run `git config --global user.name <VALUE>` "
    "or `hpds git setup`\n"
    "warning: user.email is not set; run `git config --global user.email <VALUE>` "
    "or `hpds git setup`\n"
)


class IdentityWarningTests(unittest.TestCase):
    def test_warnings_under_next_steps_and_in_the_log_pass(self) -> None:
        self.assertEqual(
            RUN.identity_warning_problems(IDENTITY_TERMINAL, IDENTITY_LOG), []
        )

    def test_a_warning_outside_next_steps_does_not_count(self) -> None:
        terminal = "warning: user.name is not set\nwarning: user.email is not set\n"
        self.assertEqual(
            RUN.identity_warning_problems(terminal, IDENTITY_LOG),
            [
                "the terminal's next steps lack 'user.name is not set'",
                "the terminal's next steps lack 'user.email is not set'",
            ],
        )

    def test_the_log_must_hold_each_warning(self) -> None:
        self.assertEqual(
            RUN.identity_warning_problems(IDENTITY_TERMINAL, "user.name is not set\n"),
            [
                "the log has no warning line with 'user.name is not set'",
                "the log has no warning line with 'user.email is not set'",
            ],
        )


def two_run_log() -> str:
    """A log holding two server runs, the second idempotent."""
    first = setup_snapshot("compact_background_log")
    second = first.replace("one\n", "already done\n")
    return "stray line before any run\n" + first + second


class OutputScanTests(unittest.TestCase):
    def test_counts_only_the_exact_refresh_announcement(self) -> None:
        text = (
            "refreshing apt package lists (needs sudo)\n"
            "refreshing apt package lists again (needs sudo)\n"
            "  will run: refreshing apt package lists (needs sudo)\n"
            "refreshing apt package lists (needs sudo)  \n"
        )
        self.assertEqual(RUN.count_lines(text, RUN.REFRESH_ANNOUNCEMENT), 2)

    def test_finds_mentions_of_the_install_script_host(self) -> None:
        text = (
            "installing code-server (needs sudo)\n"
            "+ curl -fsSL https://Code-Server.dev/install.sh\n"
        )
        self.assertEqual(
            RUN.lines_mentioning(text, RUN.CODE_SERVER_SCRIPT_HOST),
            ["+ curl -fsSL https://Code-Server.dev/install.sh"],
        )
        self.assertEqual(
            RUN.lines_mentioning("code-server 4.141.0\n", RUN.CODE_SERVER_SCRIPT_HOST),
            [],
        )

    def test_formats_seconds_with_one_decimal(self) -> None:
        self.assertEqual(RUN.format_seconds(143.26), "143.3 s")


class LogSectionTests(unittest.TestCase):
    def test_splits_a_log_on_its_run_headers(self) -> None:
        sections = RUN.log_sections(two_run_log())
        self.assertEqual(len(sections), 2)
        for section in sections:
            self.assertTrue(section.startswith(f"{RUN.LOG_HEADER}\n"))
        self.assertNotIn("stray line", "".join(sections))
        self.assertIn("one\n", sections[0])
        self.assertIn("already done\n", sections[1])

    def test_the_last_section_is_the_latest_run(self) -> None:
        last = RUN.last_log_section(two_run_log())
        self.assertIn("already done\n", last)
        self.assertEqual(last.count(RUN.LOG_HEADER), 1)
        self.assertTrue(last.rstrip().endswith("All 3 steps succeeded. Details: <LOG>"))

    def test_a_dev_run_is_not_a_server_section(self) -> None:
        with self.assertRaises(ValueError):
            RUN.last_log_section(setup_snapshot("compact_dev_log"))

    def test_rejects_a_log_without_a_run(self) -> None:
        with self.assertRaises(ValueError):
            RUN.last_log_section("")

    def test_reads_the_log_or_says_why_not(self) -> None:
        section, sections, problem = RUN.read_setup_log(two_run_log(), 0)
        self.assertEqual((sections, problem), (2, ""))
        self.assertIn("already done", section)
        self.assertEqual(
            RUN.read_setup_log("cat: /home/nero/hpds-setup.log: No such file\n", 1),
            (
                "",
                0,
                (
                    "/home/nero/hpds-setup.log is missing: "
                    "cat: /home/nero/hpds-setup.log: No such file"
                ),
            ),
        )
        _, _, problem = RUN.read_setup_log("no header\n", 0)
        self.assertTrue(problem.startswith("/home/nero/hpds-setup.log: "), problem)


REFRESH = "refreshing apt package lists (needs sudo)\n"


def log_section(refreshes: int) -> str:
    """A passing server run's log section with `refreshes` apt refreshes."""
    return (
        f"{RUN.LOG_HEADER}\n"
        "hpds version: 0.4.0\n"
        "[1/3] system libraries (apt)\n"
        + REFRESH
        * refreshes
        + "[1/3] system libraries (apt) ✓\n"
        "[2/3] Docker Engine\n"
        "[2/3] Docker Engine ✓\n"
        "[3/3] git defaults (ran in the background)\n"
        + IDENTITY_LOG
        + "[3/3] git defaults ✓\n"
        "setup summary:\n"
        "  ✓ system libraries (apt)\n"
        "  ✓ Docker Engine\n"
        "  ✓ git defaults\n"
        "All 3 steps succeeded. Details: /home/nero/hpds-setup.log\n"
    )


TERMINAL = (
    "hpds setup --profile server: 3 steps. Full log: /home/nero/hpds-setup.log\n"
    "[1/3] system libraries (apt) ✓\n"
    "[2/3] Docker Engine ✓\n"
    "[3/3] git defaults ✓\n" + IDENTITY_TERMINAL
)


class SetupOutputCheckTests(unittest.TestCase):
    PLAN: ClassVar[list[str]] = SummaryTests.PLAN

    def results(
        self,
        seconds: float,
        *,
        first: bool,
        terminal: str = TERMINAL,
        log: str | None = None,
        sections: int | None = None,
        log_problem: str = "",
    ) -> dict[str, tuple[str, str]]:
        if log is None:
            log = log_section(2 if first else 1)
        if sections is None:
            sections = 1 if first else 2
        checks = RUN.Checks(container=None)
        run = RUN.SetupRun(RUN.Output(0, terminal), seconds, log, sections, log_problem)
        with contextlib.redirect_stdout(io.StringIO()):
            checks.setup_output("setup", self.PLAN, run, first=first)
        return {result.name: (result.status, result.note) for result in checks.results}

    def test_a_fast_first_run_passes_every_check(self) -> None:
        budget = RUN.terminal_line_budget(3)
        self.assertEqual(
            self.results(90.0, first=True),
            {
                "setup: one step line per step": ("PASS", ""),
                "setup: terminal line budget": ("PASS", f"9/{budget} lines"),
                "setup: log records every step": ("PASS", ""),
                "setup: apt refresh count": ("PASS", ""),
                "setup: no code-server install script": ("PASS", ""),
                "setup: run time": ("PASS", "90.0 s"),
            },
        )

    def test_a_first_run_refreshes_twice(self) -> None:
        self.assertEqual(
            self.results(90.0, first=True, log=log_section(1))[
                "setup: apt refresh count"
            ][0],
            "FAIL",
        )

    def test_a_rerun_refreshes_once_and_appends_a_second_section(self) -> None:
        results = self.results(400.0, first=False)
        self.assertEqual(results["setup: apt refresh count"][0], "PASS")
        self.assertEqual(results["setup: log records every step"][0], "PASS")
        self.assertEqual(results["setup: run time"], ("PASS", "400.0 s"))
        self.assertEqual(
            self.results(400.0, first=False, log=log_section(2))[
                "setup: apt refresh count"
            ][0],
            "FAIL",
        )
        self.assertEqual(
            self.results(400.0, first=False, sections=1)[
                "setup: log records every step"
            ][0],
            "FAIL",
        )

    def test_a_slow_first_run_warns_without_failing(self) -> None:
        self.assertEqual(
            self.results(151.0, first=True)["setup: run time"], ("WARN", "151.0 s")
        )

    def test_headers_on_the_terminal_fail(self) -> None:
        terminal = TERMINAL.replace(
            "[2/3] Docker Engine ✓\n", "[2/3] Docker Engine\n[2/3] Docker Engine ✓\n"
        )
        self.assertEqual(
            self.results(90.0, first=True, terminal=terminal)[
                "setup: one step line per step"
            ][0],
            "FAIL",
        )

    def test_a_terminal_over_its_budget_fails(self) -> None:
        budget = RUN.terminal_line_budget(3)
        terminal = TERMINAL + "chatter\n" * (budget - 9 + 1)
        self.assertEqual(
            self.results(90.0, first=True, terminal=terminal)[
                "setup: terminal line budget"
            ],
            ("FAIL", f"{budget + 1}/{budget} lines"),
        )

    def test_the_install_script_host_fails_in_the_terminal_or_the_log(self) -> None:
        name = "setup: no code-server install script"
        mention = "+ curl -fsSL https://code-server.dev/install.sh\n"
        command = "$ curl -fsSL https://code-server.dev/install.sh\n"
        self.assertEqual(
            self.results(90.0, first=True, terminal=TERMINAL + mention)[name][0],
            "FAIL",
        )
        self.assertEqual(
            self.results(90.0, first=True, log=log_section(2) + command)[name][0],
            "FAIL",
        )

    def test_command_output_in_the_log_may_mention_the_host(self) -> None:
        name = "setup: no code-server install script"
        output = "stdout:\n  see https://code-server.dev/docs for help\n"
        self.assertEqual(
            self.results(90.0, first=True, log=log_section(2) + output)[name][0],
            "PASS",
        )

    def test_an_unreadable_log_fails_the_log_checks(self) -> None:
        results = self.results(
            90.0, first=True, log="", sections=0, log_problem="missing"
        )
        self.assertEqual(results["setup: log records every step"][0], "FAIL")
        self.assertEqual(results["setup: apt refresh count"][0], "FAIL")
        self.assertEqual(results["setup: one step line per step"][0], "PASS")


class SetupSummaryCheckTests(unittest.TestCase):
    PLAN: ClassVar[list[str]] = SummaryTests.PLAN

    def status(self, run: object) -> str:
        checks = RUN.Checks(container=None)
        with contextlib.redirect_stdout(io.StringIO()):
            checks.setup_summary("setup: every step succeeds", self.PLAN, run)
        return checks.results[0].status

    def test_reads_the_summary_from_the_log_section(self) -> None:
        run = RUN.SetupRun(RUN.Output(0, TERMINAL), 1.0, log_section(2), 1)
        self.assertEqual(self.status(run), "PASS")

    def test_fails_on_a_failed_exit_or_a_missing_log(self) -> None:
        failed = RUN.SetupRun(RUN.Output(1, TERMINAL), 1.0, log_section(2), 1)
        self.assertEqual(self.status(failed), "FAIL")
        missing = RUN.SetupRun(RUN.Output(0, TERMINAL), 1.0, "", 0, "missing")
        self.assertEqual(self.status(missing), "FAIL")


class CodeServerConfigTests(unittest.TestCase):
    def test_reads_the_default_bind_addr(self) -> None:
        config = (
            "bind-addr: 127.0.0.1:8080\nauth: password\npassword: secret\ncert: false\n"
        )
        self.assertEqual(RUN.parse_bind_addr(config), ("127.0.0.1", 8080))

    def test_reads_a_quoted_wildcard_bind_addr(self) -> None:
        host, port = RUN.parse_bind_addr("bind-addr: '0.0.0.0:9000'\n")
        self.assertEqual((RUN.probe_host(host), port), ("127.0.0.1", 9000))

    def test_rejects_a_config_without_bind_addr(self) -> None:
        with self.assertRaises(ValueError):
            RUN.parse_bind_addr("auth: none\n")

    def test_rejects_a_bind_addr_without_a_port(self) -> None:
        with self.assertRaises(ValueError):
            RUN.parse_bind_addr("bind-addr: localhost\n")

    def test_finds_missing_extensions_case_insensitively(self) -> None:
        listing = "MS-Python.python\nquarto.quarto@1.2.3\n"
        self.assertEqual(
            RUN.missing_extensions(listing, RUN.CODE_SERVER_EXTENSIONS),
            ["ms-toolsai.jupyter", "charliermarsh.ruff", "tomoki1207.pdf"],
        )


class SmallPredicateTests(unittest.TestCase):
    def test_http_ok_accepts_success_and_redirects(self) -> None:
        self.assertTrue(RUN.http_ok("200"))
        self.assertTrue(RUN.http_ok("302\n"))
        self.assertFalse(RUN.http_ok("000"))
        self.assertFalse(RUN.http_ok("502"))
        self.assertFalse(RUN.http_ok("curl: (7) Failed to connect"))

    def test_boot_finished_accepts_running_and_degraded(self) -> None:
        self.assertTrue(RUN.boot_finished("running\n"))
        self.assertTrue(RUN.boot_finished("degraded"))
        self.assertFalse(RUN.boot_finished("starting"))
        self.assertFalse(RUN.boot_finished(""))

    def test_json_mismatches_names_each_wrong_key(self) -> None:
        text = '{"save_workspace": "never", "load_workspace": "always"}'
        self.assertEqual(
            RUN.json_mismatches(
                text, {"save_workspace": "never", "load_workspace": "never", "x": True}
            ),
            [
                "load_workspace is 'always', expected 'never'",
                "x is None, expected True",
            ],
        )
        self.assertEqual(RUN.json_mismatches("[]", {"a": 1}), ["not a JSON object"])
        self.assertTrue(RUN.json_mismatches("{", {})[0].startswith("not valid JSON"))

    def test_table_counts_failures(self) -> None:
        table = RUN.render_table(
            [
                RUN.CheckResult("quarto", "PASS"),
                RUN.CheckResult("docker: service active", "WARN"),
                RUN.CheckResult("gh", "FAIL"),
            ]
        )
        self.assertIn("quarto                  PASS", table)
        self.assertTrue(table.endswith("3 checks, 1 failed"))

    def test_table_shows_a_check_note(self) -> None:
        table = RUN.render_table(
            [
                RUN.CheckResult("setup: run time", "WARN", "slow", "151.0 s"),
                RUN.CheckResult("quarto", "PASS"),
            ]
        )
        self.assertIn("setup: run time  WARN  151.0 s", table)
        self.assertIn("quarto           PASS\n", table)


if __name__ == "__main__":
    unittest.main()
