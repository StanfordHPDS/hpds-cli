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

SNAPSHOT = (
    Path(__file__).resolve().parents[1]
    / "snapshots"
    / "setup__plan_server_prints_the_numbered_plan_on_every_os.snap"
)


def snapshot_body() -> str:
    """The plan text recorded by the Rust snapshot test, without its header."""
    text = SNAPSHOT.read_text()
    return text.split("---\n", 2)[2]


class PlanParsingTests(unittest.TestCase):
    def test_parses_the_recorded_server_plan(self) -> None:
        titles = RUN.parse_plan(snapshot_body())
        self.assertEqual(titles[0], "system libraries (apt)")
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
            "summary written to /tmp/hpds-setup.log\n"
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
            "summary written to /tmp/hpds-setup.log\n"
            "error: 1 of 1 setup steps failed\n"
        )
        self.assertEqual(
            RUN.parse_summary(text), [RUN.SummaryEntry(False, "git defaults: boom")]
        )

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

    def test_parses_a_summary_after_step_one_warnings(self) -> None:
        text = (
            "+ docker exec --user nero hpds-server-setup-1 hpds setup --profile server --yes\n"
            "[1/3] system libraries (apt)\n"
            "refreshing apt package lists (needs sudo)\n"
            "  will run: sudo env DEBIAN_FRONTEND=noninteractive apt-get update\n"
            "warning: could not register the Docker apt repository (curl: (6) "
            "Could not resolve host)\n"
            "  setup summary: is not a header when indented\n"
            "waiting for the RStudio Server and code-server downloads to finish\n"
            "warning: could not prepare the code-server package (download "
            "interrupted); code-server + extensions will install it itself\n"
            "[2/3] Docker Engine\n"
            "[3/3] git defaults (ran in the background)\n"
            "setup summary:\n"
            "  ✓ system libraries (apt)\n"
            "  ✗ Docker Engine: apt-get failed\n"
            "E: Unable to locate package docker-ce\n"
            "E: Package 'docker-ce-cli' has no installation candidate\n"
            "  ✓ git defaults\n"
            "summary written to /tmp/hpds-setup.log\n"
            "error: 1 of 3 setup steps failed\n"
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


class SetupOutputCheckTests(unittest.TestCase):
    PLAN: ClassVar[list[str]] = SummaryTests.PLAN
    TEXT = (
        "[1/3] system libraries (apt)\n"
        "refreshing apt package lists (needs sudo)\n"
        "[2/3] Docker Engine\n"
        "[3/3] git defaults (ran in the background)\n"
    )

    def results(self, seconds: float, *, first: bool) -> dict[str, tuple[str, str]]:
        checks = RUN.Checks(container=None)
        run = RUN.SetupRun(RUN.Output(0, self.TEXT), seconds)
        with contextlib.redirect_stdout(io.StringIO()):
            checks.setup_output("setup", self.PLAN, run, first=first)
        return {result.name: (result.status, result.note) for result in checks.results}

    def test_a_fast_first_run_passes_every_check(self) -> None:
        self.assertEqual(
            self.results(90.0, first=True),
            {
                "setup: one header per step": ("PASS", ""),
                "setup: one apt refresh": ("PASS", ""),
                "setup: no code-server install script": ("PASS", ""),
                "setup: run time": ("PASS", "90.0 s"),
            },
        )

    def test_a_slow_first_run_warns_without_failing(self) -> None:
        self.assertEqual(
            self.results(151.0, first=True)["setup: run time"], ("WARN", "151.0 s")
        )

    def test_a_rerun_skips_the_refresh_count_and_the_budget(self) -> None:
        results = self.results(400.0, first=False)
        self.assertNotIn("setup: one apt refresh", results)
        self.assertEqual(results["setup: run time"], ("PASS", "400.0 s"))


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
            ["ms-toolsai.jupyter", "charliermarsh.ruff"],
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
