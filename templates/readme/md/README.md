# {{project}}

## Description

<!-- One or two paragraphs: the question this project answers, the data it
uses, and its current status. -->

## File structure

<!-- Keep this table current as the project grows. -->

| Path       | Contents                                              |
|------------|-------------------------------------------------------|
| `scripts/` | Analysis code                                         |
| `data/`    | Input data (document provenance; commit only if small and shareable) |
| `docs/`    | Project documentation                                 |
| `results/` | Generated tables and figures                          |

## How to run

<!-- The exact commands that reproduce the results from a fresh clone. -->

```bash
uv sync
uv run python scripts/analysis.py
```

## Dependencies

<!-- Languages, package managers, lockfiles, and external tools. -->

- Python (dependencies locked in `uv.lock`; restore with `uv sync`)
