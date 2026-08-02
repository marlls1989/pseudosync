# Changelog

All notable changes to pseudosync are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0/).

## [Unreleased]

### Added

- **`--reference-mode` (`-m`), defaulting to `per-state`.** Three rungs of one ladder: `pooled` draws
  one clock-to-output reference for the whole cell, `per-output` one per output, `per-state` one per
  post-settled state of each output, conditioning the emitted delays and checks on it. The finer the
  key, the fewer measurements one reference has to stand for. `pooled` is retained as a deliberate
  regression, so that its cost can be measured rather than assumed.
- **The reconstruction report: `--report` (`-R`) and `--report-summary-only`.** Per cell and per arc,
  the original table, the constraint and reference arcs it was split into, the table rebuilt from that
  split, and the residual between them, with per-point relative error and per-arc and per-cell error
  statistics. It is how the cost of a given `--reference-mode` and `--when-merge` is read rather than
  assumed. `--report-summary-only` limits the tables and never the refusals.
- **`--when-merge` (`-w`) selects how arcs sharing one key are merged**: `mean` is representative,
  `max` the pessimistic envelope, `min` the optimistic one, elementwise per slew/load point. Residuals
  are measured against the raw `when` conditions rather than against the merged arc alone, so the cost
  of merging appears as its own term instead of being averaged out of the error it contributes to.
- **`--anchor` selects where the collapsed axis's value is read**: `middle` takes the middle row,
  column and element, so every number emitted is one the library measured; `average` takes the mean
  over that axis instead.
- **Refusal at the narrowest scope that will do, recorded in both artefacts.** An arc whose lookup
  template declares only one axis is skipped; an output no non-reset source of which supplies a
  complete reference is skipped and keeps its timing groups exactly as the input wrote them; a cell is
  emitted verbatim when no output of it converts, or when the arcs it would transform are not all on
  one domain — the same lookup template carrying the same table dimensions; and the run is refused
  outright, before any conversion, when a candidate cell names a template the library does not
  declare. A run with skips exits 0, so standard error and the report are the only signals there are,
  and every skipped output and refused cell reaches both.
- **Retained reset arcs are restated asynchronously in the flop model.** A converted output carried the
  synthesised `rising_edge` arc beside reset arcs still tagged with the combinational family, which
  Genus cannot hold on one output and so blackboxed the cell. Each retained reset arc is now restated
  under the asynchronous type its own tables name — a fall family becomes `clear`, a rise family
  `preset`, and an arc measuring both becomes two. `related_pin`, `timing_sense`, `when`, `sdf_cond`
  and every table value pass through unchanged, and the sequential group is never consulted. An arc
  the rule cannot state is emitted unchanged with a warning, never refused; `--latch` restates nothing.
- **`README.md`, `GUIDELINES.md`, `KNOWN-ISSUES.md`, and `docs/`.** `GUIDELINES.md` carries the
  contributor rules, under which nothing enters the documentation without what backs it — a Liberty
  manual page, a section of the Pulsar paper, a named test, or a measurement actually run. `docs/`
  describes the algorithms in seven documents: the Liberty timing model, the pseudo-synchronous
  split, reference selection, post-settled states, the two emitted models, the limits of the model,
  and running the tool. Implementation detail — line numbers and the tests pinning a behaviour —
  lives in the source doc comments rather than in `docs/`, so that the prose describes the code
  without standing over it.

### Changed

- **The vendored `liberty-parse` submodule is replaced by the published `liberty-parser` crate**, at
  0.2 and then 0.3 for multi-line, round-trip-stable table serialisation. The submodule and its
  `.gitmodules` wiring are gone and the parser is an ordinary dependency.
- **`lib.rs` is gone and the crate is a binary** (breaking). One 2000-line `lib.rs` became modules
  along the lines the code already divided along — Liberty reading and writing, arcs, pins,
  conditions, emission, templates, reset, the engine and the report — each behind a private `mod`.
  This crate has no public API surface to offer, so nothing is exported from a library nobody links.
- **Overlapping `when` conditions collide, not only equal ones.** Liberty UG pp. 7-49–50 requires that
  no two state-dependent timing arcs on one pin pair be satisfiable at once, and the emitted libraries
  violated it: a source `when` need not name every pin, so two conditions over different pin subsets
  can share satisfying assignments without being equal, and interning states on Boolean equality
  emitted both as separate states. Two conditions now collide when their conjunction is not a
  contradiction, decided on a BDD; a class is the transitive closure of that relation, stated under the
  least restrictive condition covering it.
- **Post-settled states are drawn within one output, and the constraint is keyed by the input pin's own
  check condition.** States classified across the whole cell could hold members from two outputs and so
  state one output's arc under a condition drawn from the other's characterisation. `G → Q1` and
  `G → Q2` are different pin pairs, and Liberty's mutual-exclusivity requirement never applied between
  them.
- **Container-style bundles are handled and arc reduction reworked**, so a cell whose outputs are
  separate rails is no longer treated as a cell with one output seen several ways.
- **Every warning on standard error takes one loud form and names the library it happened in**, with
  the cell or arc alongside whatever else identifies it. Each message keeps its own reason text;
  nothing changes about when one is emitted.
- **A split reset arc's halves are each built from the two table families the arrival it names needs**,
  every other subgroup being discarded, rather than taking the whole arc and shedding the opposite
  direction's tables. A list of what to shed can only name the group types whoever wrote it had met, so
  an unfamiliar one would ride onto both halves unexamined; a list of what to keep has no such gap. An
  arc whose tables state one arrival is not split and keeps every subgroup it had.
- **An arc's input direction is derived per arc from `timing_sense`** rather than assumed: under
  `positive_unate` the input moves with the output, under `negative_unate` against it, and under
  `non_unate` or with no sense stated nothing determines it, so the arc is skipped.
- **The library is written before the report.** The report was written first, so a report path that
  could not be opened discarded the whole run after every expensive step had already succeeded. The
  library is the product; a report that cannot be written is still an error, but no longer takes the
  library with it. Both destinations are resolved before anything is parsed and a collision between
  them is refused there, and `--output -` means standard output rather than a file called `-`.

### Fixed

- **The Liberty writer did not flush.** A library small enough to fit the buffer was announced as
  written when the disk had refused every byte — the flush a buffered writer performs when it drops
  throws its error away.
- **`write_summary` rolled a cell up twice whenever its arcs were not adjacent**, having dropped only
  adjacent repeats.
- **One of two constraint values was computed and never emitted.** A check group correctly merges one
  pin's overlapping conditions, but carried a single delay-side scope written once per member arc, so
  the last member overwrote the earlier ones.
- **No delay is invented for a path nothing characterised.** An input driving both a converted and a
  skipped output is constrained over the converted outputs only, one driving none gets no constraint,
  and `pooled` no longer hands the cell-wide mean to an output that has no reference of its own.
- **A cell one of whose characterised outputs yields no reference arc is skipped rather than half
  converted.**
- **The report renderers propagate their write errors** instead of discarding them at twelve sites.

### Removed

- **The `tests/` directory and the benchmark scaffolding.** Every test now sits in a `#[cfg(test)]`
  module inside the module it tests, reaching private items directly instead of forcing visibility
  open to suit a test. `TEST_COVERAGE.md` goes with them.
- **The byte comparisons against the committed example outputs.** A golden file either passes
  vacuously or fails on every intended change, and it makes the recorded output the definition of
  correct — so a defect frozen into one is then defended by the suite. What those comparisons were
  meant to cover is restated as assertions about what the emitted library must say.
- **`--offset-placement`**: the split the paper describes is the only one. The claim that the
  placement left the residual unchanged was false, and was corrected at all five sites carrying it
  before the option went.
- **The `pseudosync.txt` debug file.** Loading the engine no longer creates or appends a file in the
  process working directory.

## [0.1.0] - 2025-11-04

### Added

- **Latch-to-pseudo-flop conversion.** A cell declaring both a `latch` group and a pin named by
  `--clock-pin` has each input-to-output timing arc split into a clock-to-output delay on the output
  and a setup constraint on the input, referred to a fictitious clock pin, so that a synchronous
  synthesis or timing tool will use the cell and time it. Hold constraints are the negated setup,
  emitted for min-delay analysis. Inputs gain `nextstate_type : data`, and the `latch` group becomes
  `ff`, with `enable` becoming `clocked_on` and `data_in` becoming `next_state`.
- **`--latch`, the pseudo-latch model**: the `latch` group and every original arc are kept and the
  pseudo-synchronous timing is added alongside them, for delay annotation against the real
  input-to-output delays rather than the fictitious clock's.
- **`--clock-pin` (default `G`) and `--reset-pin` (default `(R|S)N?`)**, the latter identifying the
  asynchronous set/reset pins whose arcs are excluded from constraint synthesis and preserved on the
  outputs.
- **`--output`**, defaulting to standard output.
- **Derived lookup templates**, a `*_pseudo_constraint` indexed by slew and a `*_pseudo_delay` indexed
  by load, synthesised per template the conversion used and prepended to the library.
- **Bundle pin support**, and `voltage_map` preservation through the parser.

[Unreleased]: https://github.com/marlls1989/pseudosync/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/marlls1989/pseudosync/releases/tag/v0.1.0
