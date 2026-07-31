# pseudosync

Makes resettable C-elements, and C-element-like cells, appear as flip-flops — so that
Genus will use them and synchronous static timing analysis can close timing on QDI
logic.

QDI logic is built from C-elements, which the synchronous flow has no model for. The
method this tool implements re-describes such a cell on the axes a flip-flop is
characterised on, referred to a clock that does not exist in the silicon, so that the
standard tools will use it and time it. It is described in

> M. L. L. Sartori, R. N. Wuerdig, M. T. Moreira, N. L. V. Calazans, "Pulsar:
> Constraining QDI Circuits Cycle Time Using Traditional EDA Tools", *IEEE ASYNC 2019*,
> pp. 114–123. DOI 10.1109/ASYNC.2019.00023.

Section III and Figure 7 of that paper define the models this tool emits.

## What the conversion does

A C-element is characterised in Liberty as a transparent path: each input-to-output
timing arc is a two-dimensional table over *input slew* and *output load*. A flip-flop is
characterised differently — a setup constraint on the data input, which depends on slew,
and a clock-to-output delay on the output, which depends on load.

pseudosync splits each original arc into those two halves, referred to a **fictitious
clock pin**:

```
delay(A→Z) = propagation(G→Z) + setup(A→G)
```

The clock pin exists only in the emitted `.lib` file. It is absent from the cell's layout
and from its abstract views, and there is no such net in the silicon. That is what makes
the real reset and set pins' own arcs survivable: the method's predecessor repurposed the
real `Reset` pin as the pseudo-clock, which tied the reset network to a clock tree the
tool was simultaneously trying to synthesise, and forced loop breakers before delays
could be annotated. Introducing a pin that exists nowhere else avoids both.

The two halves must sum back to the arc they came from. They do not do so exactly: two
independent values are summed, and together they do not describe the delay as a function
of both slew and load at once. The difference is the **residual**, and the reconstruction
report measures and reports it. That is the method's declared cost, not a defect — and it
is why the report exists.

The generated tables are one-dimensional. The fictitious clock's own slew is not a real
quantity, so it is ignorable, leaving the constraint indexed by slew alone and the delay
by load alone.

**Multi-output cells are new work.** The paper covers a two-input, single-output
resettable C-element. Everything this tool does with a cell driving several outputs — a
dual-rail bundle in particular — is beyond it, and no prior art settles it.

## Usage

```
pseudosync [OPTIONS] <input>
```

`<input>` is a Liberty file, or `-` to read standard input. A Liberty file holds one
library block.

| Option | Default | Meaning |
|---|---|---|
| `-o`, `--output <path>` | stdout | Where the converted library is written. `-` also means standard output |
| `-l`, `--latch` | off | Emit the latch model rather than the flop model: keep the `latch` group and every original arc, and add the pseudo-synchronous timing alongside them. This is the library to use when generating SDF for delay-annotated simulation, which needs the real input-to-output delays rather than the fictitious clock's |
| `-c`, `--clock-pin <name>` | `G` | The pin the conversion treats as the clock. It is expected to exist in the `.lib` and nowhere else |
| `-r`, `--reset-pin <regex>` | `(R\|S)N?` | Arcs whose `related_pin` matches are treated as asynchronous set/reset: excluded from the conversion and retained unchanged |
| `-m`, `--reference-mode <mode>` | `per-state` | `per-state` gives each post-settled state of each output its own clock-to-output reference and conditions the emitted delays and checks on it; `per-output` coarsens that to one reference per output, constraining each input against only the outputs it actually drives; `pooled` coarsens further still, to the cell-wide mean — a deliberately kept regression, not a designed alternative, retained only so its cost can be measured |
| `-w`, `--when-merge <mode>` | `mean` | How several arcs sharing one key are merged into the one table emitted for it: `mean` is representative, `max` the pessimistic envelope, `min` the optimistic one. Merging is elementwise, per slew/load point. Under `pooled`/`per-output` the key is the whole output, so this merges every `when`-conditioned arc of a pin pair; under `per-state` the key is one post-settled state, so this instead resolves a collision between conditions denoting the same state |
| `--anchor <mode>` | `middle` | Where in each characterised table the value standing for the collapsed axis is read: `middle` takes the middle row, column and element, so every number emitted is one the library measured; `average` takes the mean over that axis instead |
| `--offset-placement <mode>` | `setup` | Which half of the split carries the constant the two are separated around: `setup` leaves it in the setup constraint, `prop` folds it into the clock-to-output delay. The two halves sum to the same arc either way, but not the residual: `setup` is exact at the anchor point, `prop` adds a constant bias on any pin driving two or more outputs |
| `-R`, `--report <path>` | none | Write the reconstruction report here. `-` writes it to standard error |
| `--report-summary-only` | off | Limit the report's tables to the per-arc and per-cell error statistics. Refusals are still listed |

`-` names the standard stream belonging to the artefact it is given to: standard input
for `<input>`, standard output for `--output`, standard error for `--report`. The two
output artefacts may not share a destination, and a run that would have them do so is
refused before either is written.

`per-state` is the default and the behaviour to use. `per-output` and `pooled` are coarser
rungs of the same ladder — one reference per output, then one for the whole cell — kept
because a mode this new needs a fallback, not because either is a designed alternative to
it. `pooled` in particular is a deliberately kept regression: on a cell whose outputs are
independent rails it averages measurements that describe different elements, and it is
retained only so that cost can be measured before any decision to remove it. All three
coincide on a single-output cell characterised under no `when` condition at all — pooled's
cell-wide mean of one output is that output's own reference, and per-state's one state is
the whole output — and can differ the moment either does not hold.

## What it changes in the library

A cell is a candidate for conversion only if it declares **both** a `latch` group and a
pin named by `--clock-pin`. For such a cell:

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

A cell declaring neither, or only one, is an ordinary cell. It is emitted verbatim and
**silently** — nothing was asked of the tool there, so it says nothing about it.

## What it refuses, and at which scope

Not everything can be re-described on the flip-flop axes, and what cannot be is refused
at the narrowest scope that will do.

- **An arc** whose lookup template declares only one axis is skipped, with a warning
  naming which axis is missing. A one-dimensional template is ordinary Liberty — it is
  the shape this tool's own derived templates take — so this is not a complaint about the
  input.
- **An output** no non-reset source of which supplies a complete reference is skipped:
  its timing groups are left exactly as the input wrote them and it gains no clock-to-
  output arc, because an output with no arc being split has no clock-to-output delay to
  state. The rest of the cell still converts.
- **A cell** is emitted verbatim when no output of it is convertible, or when the arcs it
  would transform are not all on one **domain**: the same lookup template, carrying the
  same table dimensions. Differing dimensions are a differing template whatever the name
  says, and the rule applies within a single timing group as well as between outputs — the
  four families of one arc must agree too. Values indexed differently cannot be transformed
  together, so such a cell is one the conversion cannot describe. One such cell never stops
  the run.
- **The run** is refused outright, before any conversion, when a candidate cell names a
  lookup template the library does not declare. That library is broken: the conversion
  would emit a cell referencing derived templates it has nothing to build. Such a file
  reads and parses without complaint, so nothing else would catch it.

Every skipped output and every refused cell is named on standard error *and* recorded in
the reconstruction report. A run with skips exits 0 — skipping is not a fault the caller
committed — so those two artefacts are the only signal there is.

Exit 1 means no product could be produced: the input could not be read or parsed, an
artefact could not be written, the command line was invalid, or the library was broken.

`docs/conversion-policy.md` states these rules precisely, and is the contract.

## The reconstruction report

`--report` dumps, per cell and per arc: the original table, the constraint and reference
arcs it was split into, the table rebuilt from that split, and the residual between them,
followed by per-arc and per-cell error statistics. It is how the cost of a given
`--reference-mode` and `--when-merge` is measured against the library it replaced, and how
the residual is read rather than assumed.

## Examples

`examples/` holds three ASCEND FREEPDK45 corner libraries and, for each, the output of a
default run (`_pseudoflop`) and of a `--latch` run (`_pseudolatch`), produced by:

```
pseudosync examples/<corner>.lib         -o examples/<corner>_pseudoflop.lib
pseudosync examples/<corner>.lib --latch -o examples/<corner>_pseudolatch.lib
```

Every qualifying cell in these three libraries converts, so a run over them reports no
skips.

## Building

```
cargo build --release
```

`espresso-logic` builds a small C component as part of this, so clang-devel must be
installed.

Contributor rules are in `GUIDELINES.md`. What the tool does, and what it refuses to do,
is in `docs/conversion-policy.md`.
