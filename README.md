# pseudosync

Rewrites Liberty latch cells as pseudo-synchronous flip-flop models.

A latch is characterised as a transparent path: each input-to-output timing arc is a
two-dimensional table over *input slew* and *output load*. A flip-flop is characterised
differently — a setup and hold constraint on the data input, which depends on slew, and
a clock-to-output delay on the output, which depends on load. The two describe the same
silicon on different axes.

pseudosync performs that change of axes. It splits each original arc into

- a **slew-dependent constraint** placed on the input pin, and
- a **load-dependent clock-to-output delay** placed on the output pin,

so that reconstructing `setup[slew] + delay[load]` approximates the original table. The
approximation is exact only for arcs that are genuinely separable; the difference is the
*residual*, and the reconstruction report measures it.

The emitted library is intended for timing closure, so the tool's bias throughout is
that a library which is silently wrong is worse than one that plainly says what it could
not do.

## Usage

```
pseudosync [OPTIONS] <input>
```

`<input>` is a Liberty file, or `-` to read standard input. With no `--output` the
converted library is written to standard output.

| Option | Default | Meaning |
|---|---|---|
| `-o`, `--output <path>` | stdout | Where the converted library is written |
| `-l`, `--latch` | off | Keep the `latch` group and the original arcs, adding pseudo-synchronous timing alongside them, instead of producing a flip-flop |
| `-c`, `--clock-pin <name>` | `G` | The pin the conversion treats as the clock |
| `-r`, `--reset-pin <regex>` | `(R\|S)N?` | Arcs whose `related_pin` matches are treated as asynchronous set/reset: excluded from the conversion and retained unchanged |
| `-m`, `--reference-mode <mode>` | `per-output` | `per-output` gives each output its own clock-to-output reference and constrains each input against the outputs it actually drives; `pooled` gives every output the cell-wide mean |
| `-w`, `--when-merge <mode>` | `mean` | How several `when`-conditioned arcs of one pin pair are merged: `mean` is representative, `max` the pessimistic envelope, `min` the optimistic one. Merging is elementwise, per slew/load point |
| `-R`, `--report <path>` | none | Write the reconstruction report here; `-` writes it to standard error |
| `--report-summary-only` | off | Limit the report to per-arc and per-cell error statistics, omitting the full tables |

## What it changes in the library

For each cell carrying a latch group and a pin matching `--clock-pin`:

- every input the library characterised against a converted output gains `setup_rising`
  and `hold_rising` constraint groups against the clock, and is marked
  `nextstate_type : data`;
- every converted output gains a clock-to-output arc — `timing_type : rising_edge`,
  carrying `cell_rise`/`cell_fall` delays and `rise_transition`/`fall_transition` slews;
- without `--latch`, the original non-reset arcs of a converted output are removed and
  the `latch` group becomes `ff`, with `enable` becoming `clocked_on` and `data_in`
  becoming `next_state`;
- the library gains a derived `lu_table_template` pair per template the conversion used:
  `<name>_pseudo_constraint`, indexed by slew, and `<name>_pseudo_delay`, indexed by
  load.

Cells with no latch group, or with no pin matching `--clock-pin`, are left untouched.

## Partial conversion

Not every output of every cell can be re-described on the flip-flop axes. Where it
cannot be, pseudosync converts what it can, leaves the rest exactly as the input wrote
it, and says so — on standard error and in the reconstruction report. This is normal
operation, not failure, and it does not change the exit status.

`docs/conversion-policy.md` states the rules precisely: what makes an output
convertible, when a whole cell is left alone, and what the exit status means.

## The reconstruction report

`--report` dumps, per cell and per arc: the original table, the constraint and reference
arcs it was split into, the table rebuilt from that split, and the residual between
them, followed by per-arc and per-cell error statistics. It is how the cost of a given
`--reference-mode` and `--when-merge` is measured against the library it replaced.

## Examples

`examples/` holds three ASCEND FREEPDK45 corner libraries and, for each, the output of a
default run (`_pseudoflop`) and of a `--latch` run (`_pseudolatch`). They are
documentation of what the tool emits, regenerated whenever behaviour legitimately
changes.

## Building

```
cargo build --release
```

Contributor rules are in `GUIDELINES.md`.
