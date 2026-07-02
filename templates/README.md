# templates/

Each directory here is a template component, embedded into the `hpds` binary
at compile time (spec §6) and applied by `hpds use <component>` (M3.2+).

Engine-test fixtures live in `tests/fixtures/templates/`, not here: everything
in this directory ships inside release binaries.
