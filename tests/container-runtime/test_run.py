from __future__ import annotations

import argparse
import importlib.util
from pathlib import Path
import tempfile
import unittest


MODULE_PATH = Path(__file__).with_name("run.py")
SPEC = importlib.util.spec_from_file_location("container_runtime_run", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
RUN = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUN)


class PublishedHpdsSelectionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.project = Path(self.temporary.name)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write_generated(self, filename: str, *, references: int = 1) -> Path:
        path = self.project / filename
        release = "https://example.test/releases/download/v0.1.4/asset"
        text = "\n".join([release] * references) + "\n"
        if filename == "container.def":
            text += "    HPDS 0.1.4\n"
        path.write_text(text)
        return path

    def test_docker_only_rewrites_its_release(self) -> None:
        dockerfile = self.write_generated("Dockerfile")
        RUN.select_published_hpds(self.project, "0.1.4", "0.1.3", ["Dockerfile"])
        self.assertIn("/releases/download/v0.1.3/", dockerfile.read_text())
        self.assertFalse((self.project / "container.def").exists())

    def test_apptainer_only_rewrites_release_and_label(self) -> None:
        definition = self.write_generated("container.def")
        RUN.select_published_hpds(
            self.project, "0.1.4", "0.1.3", ["container.def"]
        )
        text = definition.read_text()
        self.assertIn("/releases/download/v0.1.3/", text)
        self.assertIn("    HPDS 0.1.3\n", text)
        self.assertFalse((self.project / "Dockerfile").exists())

    def test_both_generated_files_are_rewritten(self) -> None:
        files = [self.write_generated(name) for name in ["Dockerfile", "container.def"]]
        RUN.select_published_hpds(
            self.project,
            "0.1.4",
            "0.1.3",
            ["Dockerfile", "container.def"],
        )
        for path in files:
            self.assertNotIn("/releases/download/v0.1.4/", path.read_text())

    def test_missing_or_duplicate_release_reference_is_rejected(self) -> None:
        self.write_generated("Dockerfile", references=0)
        with self.assertRaisesRegex(RuntimeError, "exactly one"):
            RUN.select_published_hpds(
                self.project, "0.1.4", "0.1.3", ["Dockerfile"]
            )
        self.write_generated("Dockerfile", references=2)
        with self.assertRaisesRegex(RuntimeError, "exactly one"):
            RUN.select_published_hpds(
                self.project, "0.1.4", "0.1.3", ["Dockerfile"]
            )

    def test_only_strict_stable_versions_are_accepted(self) -> None:
        self.assertEqual(RUN.stable_version("12.3.45"), "12.3.45")
        for value in ["v1.2.3", "1.2", "1.2.3-rc.1", "1.2.3;echo bad", ""]:
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                RUN.stable_version(value)


if __name__ == "__main__":
    unittest.main()
