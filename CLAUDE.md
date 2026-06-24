# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`pseudosync` is a Rust CLI that rewrites EDA **Liberty** (`.lib`) standard-cell timing
files: it takes asynchronous **latch**-based cells and emits a **pseudo-synchronous** model
of them — synthesising setup/hold constraints on the data inputs and a clock→output timing
arc — so downstream synthesis/timing tools can treat the cell as if it were clocked. The
domain is digital ASIC characterisation. The timing numbers are derived (averaged) from the
cell's original combinational timing tables, not re-characterised.

## Commands

```bash
# liberty-parse is a git submodule — required before first build
git submodule update --init --recursive

cargo build                  # debug build
cargo build --release        # release build

# Run. INPUT of "-" reads stdin; without --output, result goes to stdout.
# Progress is printed to stderr (eprintln!).
cargo run -- [--latch] [--clock-pin G] [--reset-pin '(R|S)N?'] [--output OUT.lib] IN.lib
cargo run -- examples/ASCEND_FREEPDK45_ALHO_nom_1.10V_25C.lib -o out.lib

cargo test                              # all unit + integration tests
cargo test --test pseudosync_tests      # one integration suite
cargo test cell_qualifies               # a single test by name
cargo bench                             # criterion benchmarks (benches/pseudosync_bench.rs)
cd liberty-parse && cargo test          # the vendored parser's own suite
```

### CLI options that drive behaviour

- `--clock-pin` (default `G`): the **existing** pin that already acts as the latch
  enable/clock. A cell qualifies for transformation only if it has a `latch*` group **and** a
  pin with exactly this name. This pin becomes the synchronous clock in the output.
- `--reset-pin` (default regex `(R|S)N?`): identifies asynchronous set/reset pins. Their
  timing arcs are **excluded** from constraint synthesis and **preserved** on outputs (not
  replaced by the pseudo arc). Reset inputs get no setup/hold constraints.
- `--latch` (flag): selects the output mode (see below).

## Architecture

Two crates: this crate (`src/`) plus the vendored **`liberty-parse`** submodule, which owns
the Liberty AST (`Group`, `Attribute`, `Value`) and all parse/serialise. Everything here
operates on `liberty_parse::liberty::Group` trees. `src/` is `main.rs` (CLI) + `lib.rs` (the
whole engine); there are no other modules.

**`src/main.rs`** — `structopt` CLI: parse the file → `process_library(lib, clock_pin,
reset_pin, latch)` per library → write out.

**`src/lib.rs`** — the engine. `process_library` selects qualifying cells
(`cell_qualifies`: has a `latch*` subgroup **and** a pin named `clock_pin`) and runs
`process_cell`, structured as 6 explicit phases:
1. Walk output pins, extract timing tables (`extract_timing_tables_from_arc`), and pick a
   per-output reference arc (`select_reference_arc`, middle LUT row/col). Arcs whose
   `related_pin` matches the reset regex are skipped.
2. Average the per-output reference arcs into one `mean_reference_arc` for the cell.
3. `add_pseudo_timing_to_output_pin` for each output: append a pseudo clock→output arc
   (`rising_edge`, `non_unate`). **Mode-dependent** — in pseudoflop mode (`!latch`) the
   original `timing` arcs are first erased *except* reset-related ones; in pseudolatch mode
   the originals are kept and the pseudo arc is added alongside.
4. Derive setup constraints from input→output delay arcs relative to the reference arc;
   hold = negated setup (`calculate_setup_constraints` / `calculate_hold_constraints`).
5. Attach the setup/hold `timing` groups and `nextstate_type: data` to each non-reset input.
6. `convert_latch_to_flipflop` **only in pseudoflop mode**: rename `latch*`→`ff*`,
   `enable`→`clocked_on`, `data_in`→`next_state` (the enable expression is kept as-is).
Finally `process_library` synthesises the `*_pseudo_constraint` / `*_pseudo_delay`
`lu_table_template`s referenced by the new arcs and prepends them.

The two modes correspond to the example reference outputs:
`examples/*_pseudolatch.lib` (`--latch`, keep latch + original arcs) and
`examples/*_pseudoflop.lib` (default, convert to ff + replace non-reset output arcs);
`examples/*.lib` (no suffix) are the originals.

Timing tables are `ndarray` `Array2<f64>` LUTs (input slew × output load); a `RefArc` holds
the 1-D rise/fall transition + cell-delay slices used as the constraint reference. Averaging
across arcs is how multiple input/output paths collapse into one pseudo-synchronous
characterisation.

## Gotchas

- Loading `lib.rs` creates/appends a `pseudosync.txt` file in the CWD via a `lazy_static`
  debug writer — a side effect of using the library at all.
- `liberty-parse` is an external submodule (its own repo + test suite); changes there are a
  separate concern from this crate.
- `TEST_COVERAGE.md` documents the `tests/` + `benches/` suites.
