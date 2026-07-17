# {{project}}

## Description

<!-- A brief description of the project. -->

## File structure

<!-- Example only: replace or remove this table to match the project. -->

  | Path       | Contents                                                             |
  | ---------- | -------------------------------------------------------------------- |
  | `scripts/` | Analysis code                                                        |
  | `data/`    | Input data (document provenance; commit only if small and shareable) |
  | `docs/`    | Project documentation                                                |
  | `results/` | Generated outputs                                                    |

## How to run

<!-- The exact commands that reproduce the results from a fresh clone. -->

```bash
uv sync
make
```

## Dependencies

<!-- Languages, package managers, lockfiles, and external tools. -->

- Python (dependencies locked in `uv.lock`; restore with `uv sync`)
