# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`pseudosync` is a Rust CLI that rewrites EDA **Liberty** (`.lib`) standard-cell timing
files: it takes asynchronous **latch**-based cells and emits a **pseudo-synchronous** model
of them — synthesising setup/hold constraints on the data inputs and a clock→output timing
arc — so downstream synthesis/timing tools can treat the cell as if it were clocked. The
domain is digital ASIC characterisation. The timing numbers are derived from the cell's
original combinational timing tables, not re-characterised.

## Read these before working here

- **`GUIDELINES.md`** — how work is done in this repository. **Owner-only: never edit it.**
  A rule that seems wrong or missing is raised, never committed.
- **`docs/conversion-policy.md`** — the behaviour contract: what the tool emits and what it
  refuses. Sections are numbered and cited as §N.
- **`KNOWN-ISSUES.md`** — defects found and deliberately deferred, with what someone using
  the tool actually sees.
- **`README.md`** — the user-facing description.
- **`CHANGELOG.md`** — Keep a Changelog; new work is recorded under `[Unreleased]`.

## Commands

```bash
cargo build                  # debug build
cargo build --release        # release build

# Run. An input of "-" reads stdin; without --output, the result goes to stdout.
# Progress and warnings are printed to stderr.
cargo run -- [OPTIONS] IN.lib
cargo run -- examples/ASCEND_FREEPDK45_ALHO_nom_1.10V_25C.lib -o out.lib

cargo test                   # every test; they live in-crate, there is no tests/ directory
cargo test cell_qualifies    # a single test by name
```

`espresso-logic` builds a small C component, so clang-devel must be installed.

### The green bar (`GUIDELINES.md`)

```
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

`#[allow]` is a genuine last resort: where one is used, it says in place what the lint
wanted, why the proper form can't be had, and what the attribute buys. The tree currently
has none.

### CLI options

| Option | Default | Drives |
|---|---|---|
| `-c`, `--clock-pin` | `G` | The pin treated as the clock. A cell qualifies only if it has a `latch*` group **and** a pin of this name. It is expected to exist in the `.lib` and nowhere else |
| `-r`, `--reset-pin` | `(R\|S)N?` | Regex identifying asynchronous set/reset pins. Their arcs are excluded from constraint synthesis and retained on outputs; reset inputs get no setup/hold |
| `-l`, `--latch` | off | Emit the latch model rather than the flop model |
| `-m`, `--reference-mode` | `per-state` | How widely one clock-to-output reference is drawn: `pooled` (cell-wide), `per-output`, `per-state` |
| `-w`, `--when-merge` | `mean` | How arcs sharing one key are merged: `mean`, `max`, `min`, elementwise per slew/load point |
| `--anchor` | `middle` | Where the collapsed axis's value is read: `middle` (a measured value) or `average` |
| `-R`, `--report` | none | Write the reconstruction report here; `-` writes it to stderr |
| `--report-summary-only` | off | Limit the report's tables, never its refusals |
| `-o`, `--output` | stdout | Where the converted library is written |

## Architecture

Two crates: this one (`src/`) plus the external **`liberty-parser`** crate (crates.io
`0.3`, a maintained fork of `liberty-parse`), which owns the Liberty AST (`Group`,
`Attribute`, `Value`) and all parse/serialise. Everything here operates on
`liberty_parser::liberty::Group` trees — the import path is `liberty_parser`.

**This crate is a binary. There is no `lib.rs` and no public API surface**; every module
sits behind a private `mod` in `src/main.rs`, and tests reach private items from a
`#[cfg(test)]` module inside the module under test.

| Module | Owns |
|---|---|
| `main.rs` | the `structopt` CLI, destination resolution, and the run |
| `liberty_io.rs` | reading a Liberty file in and writing one back out |
| `pins.rs` | pin direction predicates, cell qualification, which pin groups own or receive timing |
| `arcs.rs` | timing arcs: averaging, merging across `when`, reduction to a reference, restoration from the split |
| `conditions.rs` | the `when` expressions a library states, and the state an arc leaves the cell in once its input has settled |
| `templates.rs` | what the library declares about its lookup templates |
| `emit.rs` | construction of the Liberty groups the pseudo-flop model is written as |
| `reset.rs` | how the flop model states a retained asynchronous reset arc |
| `engine.rs` | the conversion itself, and the walk over a library |
| `report.rs` | what the conversion cost: the arcs as characterised, the arcs rebuilt from the model, the residual |
| `render.rs` | rendering of that report |

`process_library(lib: &mut Group, opts: &CellOptions) -> LibraryReport` walks a library,
runs `process_cell` on each qualifying cell, and synthesises the `*_pseudo_constraint`
(indexed by slew) and `*_pseudo_delay` (indexed by load) `lu_table_template`s the new arcs
reference. `CellOptions` carries `clock_name`, `reset_name`, `latch`, `mode`, `when_merge`
and `anchor`.

Timing tables are `ndarray` `Array2<f64>` LUTs over input slew × output load; a `RefArc`
holds the one-dimensional slices used as the constraint reference.

**`docs/conversion-policy.md` is the contract for what the conversion does** — the
refusal scopes, the reference-mode ladder, the post-settled state model, the `sdf_cond`
regeneration and the two output modes. Read it there rather than inferring the rules from
the engine.

The two modes correspond to the committed example outputs: `examples/*_pseudolatch.lib`
(`--latch`) and `examples/*_pseudoflop.lib` (default); `examples/*.lib` with no suffix are
the originals.

## Gotchas

- **`GUIDELINES.md` is not editable by a worker.** Raise a rule problem; never commit one.
- **Nothing enters `docs/` without evidence** — a Liberty manual page, a section of the
  Pulsar paper, a named test, or a measurement actually run. The documentation is derived
  work and holds no authority.
- **An expected value is never obtained by running the code.** Derive it from what the
  domain requires and state the derivation.
- **No golden-file comparison tests.** Comparing against known-good output is legitimate as
  a throwaway instrument during behaviour-preserving work, never as a committed test.
- **Lints point at missing design** — a `too_many_arguments` or `type_complexity` warning
  names a type that is missing.
- `liberty-parser` is an external crates.io dependency with its own repo and test suite. To
  change the parser, publish a new version and bump the constraint here; it is not a
  vendored submodule to edit in place.
