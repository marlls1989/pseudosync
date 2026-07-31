# Conversion policy

What pseudosync does with a Liberty library and what it refuses to do.

These are decisions, and the code meets them. A gap between the two is a defect in the
code, to be fixed — not a state to be recorded and lived with.

---

## 1. Candidacy is a declared intent

A cell is a candidate for conversion only if it declares **both**:

- a `latch` group, and
- a pin named by `--clock-pin` (default `G`).

This is exactly what `cell_qualifies` tests (`src/pins.rs:13-18`).

A cell failing that test is an ordinary cell. It is emitted **verbatim and silently** — no
warning, and nothing in the reconstruction report. Nothing was asked of the tool, so the
tool has nothing to say about it. Only a *candidate* the conversion cannot honour is worth
reporting.

## 2. Convertibility is decided per output, not per cell

An output pin is **subject to pseudosync** when at least one of its non-reset source pins
supplies a complete reference: `cell_rise`, `cell_fall`, `rise_transition` and
`fall_transition`, on a lookup template with both a slew axis and a load axis.

All four families are needed because the conversion produces two things from them — the
delays become the clock-to-output arc, the transitions become that arc's output slews — and
both axes are needed because the constraint is indexed by slew and the delay by load. A
template with no load axis describes something this conversion has no way to re-express.

The four families need not come from a single `timing` group. Arcs are merged across `when`
conditions first, and each family is counted separately, so a pin characterised
`combinational_rise` under one condition and `combinational_fall` under another supplies a
complete reference between them.

## 3. Four scopes of refusal

**Run.** A candidate cell's characterisation table references an `lu_table_template` the
library does not declare. The library is broken: refuse the file and exit 1. No conversion
is attempted and no artefact is written, so a broken library never yields a partial product.

**Arc.** Three reasons, all arc-scope and standard-error-only — no report entry, because the
report is built from the arcs that survived phase 1, and these never reach it:

- The template a timing arc names declares only one axis. Warn on standard error, naming
  the cell, the arc, the library, the template and the missing axis, and skip that arc. A
  one-dimensional template is legal Liberty and native to this domain: the templates this
  tool generates are themselves one-dimensional, because the phantom clock's own slew is
  ignorable. The warning must not call such an input malformed.
- The arc's `timing_sense` is absent, unrecognised, or `non_unate`. A constraint is keyed on
  the direction the INPUT pin was moving in, and `timing_sense` is the only attribute that
  determines it (RM p.328): under `positive_unate` the input's direction matches the
  output's, under `negative_unate` it is the opposite, and under `non_unate` — or with no
  `timing_sense` stated, for which Liberty gives no default — nothing determines it. Warn on
  standard error, naming the cell, the arc and the reason, and skip that arc. This holds in
  every reference mode alike: none of the three has a fallback direction to charge a
  constraint to.
- Under `--reference-mode per-state` only: the arc's `when` does not parse as a Liberty
  Boolean expression. Per-state files every arc under the post-settled state its `when`
  describes, and a `when` this tool cannot read names no state, so under this mode alone the
  arc is skipped — warned about on standard error, naming the cell, the arc, the library and
  the parse failure — while the rest of the output still converts. Under the other two modes
  the arc still converts: neither draws its reference from any condition, so an unreadable
  `when` bears on nothing there.

**Output.** No non-reset source of the output supplies a complete reference. This is **not
an error**. A latch cell may carry an output the state element does not drive, and that
output's combinational arcs are an accurate description of the silicon which must survive
the conversion intact. Such an output is skipped: its timing groups are not removed, no
clock-to-output arc is added, and it is named on standard error and recorded in the
reconstruction report.

**Cell.** Either no output of the cell is convertible, or the arcs the conversion would
transform are not all on one **domain**. The cell is emitted byte-for-byte as the input
wrote it — latch group included — and named on standard error and in the report.

A domain is a lookup template *together with the dimensions the table carries* — the number
of rows and columns, which is the length of that template's two index lists. Differing
dimensions are a differing template whatever the name says, because two tables cannot be
transformed together unless they are indexed the same way. So this is one rule and not two,
and it holds at both levels at which arcs can disagree: between the outputs of a cell, and
between the four families of a single timing group. The second matters as much as the first
— reading the axes of one family and slicing all four at a row taken from another would
publish one family's table under a template derived from a different one, which is wrong
numbers rather than a refusal.

The check is made before anything is mutated, so such a cell is emitted exactly as it
arrived. One such cell never stops the run: a Liberty file holds many cells in its single
library block, and one the conversion cannot describe is not a reason to discard the rest.

These cascade. Skipping an arc can leave an output short of a complete reference, and
skipping every output leaves the cell unconvertible.

## 4. The two halves sum back to the arc they came from

The conversion splits a characterised input-to-output arc into two halves, referred to a
phantom clock pin that exists only in the emitted `.lib` — absent from the cell's layout and
abstract views:

```
delay(A→Z) = propagation(G→Z) + setup(A→G)
```

The setup half is indexed by input slew, the propagation half by output load.

Two consequences follow:

- A skipped output has no propagation, so nothing driving it gets a setup constraint from
  that path.
- Another output's propagation is never borrowed to fill the gap. The number would
  correspond to no real path, and it would destroy the very identity the reconstruction
  report exists to measure.

The split is an approximation rather than a change of coordinates: two independent values
are summed, and together they do not describe the delay as an explicit function of both slew
and load. The difference is the **residual**, which the reconstruction report measures. That
is the method's declared cost, reported rather than hidden.

### Where the split is anchored, and which half keeps the constant

Two knobs decide the arithmetic of the split above, and both are orthogonal to
`--reference-mode`: that setting chooses *which output's* reference an input is charged
against, these two choose *how* a reference is read off a table and *where* the constant
between the two halves is written. Both default to the behaviour the tool has always had,
so a run that names neither is unchanged.

**`--anchor`** — the 2-D arc is slew x load and the split collapses one axis at a time, so
something has to stand for the axis being collapsed. `middle` (the default) takes the
middle row, the middle column and the middle element, so every number emitted is one the
library actually measured. `average` takes the mean over that axis instead: it uses every
measurement, at the cost of standing for no single characterised point.

**`--offset-placement`** — `delay(A→Z) = propagation(G→Z) + setup(A→G)` fixes the *sum* of
the two halves, not how the constant at the anchor point is divided between them. `setup`
(the default) leaves it in the setup constraint; `prop` folds it into the clock-to-output
delay and leaves the constraint the arc's own slew profile. Nothing else moves: the
reconstruction and its residual are identical either way, because the constant is
subtracted from one half and added to the other. The choice is about which of the two
artefacts reads as the larger number.

## 5. Reference mode: the ladder, and how widely one reference is drawn

`--reference-mode` chooses how widely the clock-to-output reference is drawn on a
multi-output cell — the same reference that must be used on both sides of
`setup(A→G) + propagation(G→Z)` for the two halves to reconstruct `delay(A→Z)`. The three
modes are one ladder, differing only in how finely the reference is keyed:

- **`pooled`** keys it on the cell: one reference, the mean across every output, charged to
  every input and emitted on every output alike. This was the tool's original behaviour, and
  it is now a **deliberately kept regression** — introduced without being a designed
  alternative, retained only so its cost against the finer modes can be measured before any
  decision to remove it. On a cell whose outputs are independent rails it averages
  measurements that describe different elements.
- **`per-output`** keys it on the output: each output keeps its own reference for its
  emitted delay, and each input is constrained against the mean reference of only the
  outputs it actually drives.
- **`per-state`** (the default) keys it on the post-settled state: each state an output was
  characterised in — the `when` it was conditioned under, if any, and the whenless catch-all
  otherwise — keeps its own reference, and the emitted delays and checks are conditioned on
  it.

Arcs that collide on one key share a reference. `Scope::Whole` is the key `pooled` and
`per-output` draw at — every arc of an output shares it, which is what keeps those two modes
exactly what they always were. `Scope::State` and `Scope::CatchAll` are the two keys
`per-state` draws at instead: a conditioned arc under the post-settled state its condition
names, a whenless arc under the catch-all for its own output and edge. Two arcs describing
the same state share one reference however many `when`s the library spelled it under — which
is why the finest key is per-*state* and not per-arc.

The derived `<name>_pseudo_constraint`/`<name>_pseudo_delay` lookup-template pair is
generated once per lookup template the conversion used, whichever mode is asked for: the
templates describe the slew and load axes alone, which reference-mode granularity has no
bearing on.

## 6. The post-settled state a per-state arc is filed under

Only `per-state` classifies. For every delay family (`cell_rise`, `cell_fall`) a
characterised arc carries, the post-settled state is that arc's own `when` — or nothing, for
a whenless arc — conjoined with a literal naming which way the constrained pin settled: `P`
if it settled high, `!P` if it settled low. The literal follows the **input's** transition,
not the output's: a `negative_unate` arc's `cell_rise` family is entered by the input
*falling*, so it is filed under `!P` even though the table itself is named for the output's
own rise.

This is the same input-direction rule the arc-scope skip above depends on, applied to what
the arc is filed under rather than to whether it converts: `timing_sense` names the output's
own direction relative to the input's, so inverting it gives the input's — the same under
`positive_unate`, opposite under `negative_unate`. The rule bears equally on which
`rise_constraint`/`fall_constraint` table a derived setup value lands in (RM p.336): both the
state a delay is filed under and the family a constraint is written to are read off the same
table family and the same `timing_sense`, never off `timing_type`'s clock-edge suffix, which
names the clock and not the constrained pin.

Two arcs whose conditions denote the same Boolean function collide into one state however
differently they were spelled — `A * B` and `B * A` name one class, decided on a BDD rather
than on text, so two spellings of one function are never emitted as two overlapping states.
`--when-merge` decides how the several arcs an accumulator collects for one key are combined
into the single table the model emits for it: `mean` (representative), `max` (the
pessimistic envelope) or `min` (the optimistic one), elementwise per slew/load point. Under
`pooled`/`per-output` the key is the whole output, so this merges every `when`-conditioned
arc of a pin pair into the cell's one arc. Under `per-state` the key is one post-settled
state, so it instead resolves a **collision** — several conditions the library spelled
differently but which denote the same state — within that one state; arcs describing
different states are filed apart and never merged into each other.

## 7. Checks and clock arcs conditioned on a post-settled state

Under `per-state`, an input pin's checks are grouped by a second classification: the source
`when`s the library characterised that pin's arcs under, independent of the post-settled
classification above and numbered per pin rather than per cell. The conditioned groups come
first, the catch-all last — the order Liberty reads a `default_timing` group in.

Each group's `when` and `sdf_cond` state the source condition **verbatim, with nothing
conjoined**: what the check constrains is said by `timing_type` (`setup_rising`/
`hold_rising`), never by the condition it holds under, and the condition itself is one the
library actually wrote, not a synthetic one this tool invented. `setup_rising`/`hold_rising`
name the **clock's** edge (RM p.332), which is why that suffix stays fixed however many
conditions the checks multiply into — the fictitious clock this model refers everything to
only ever rises.

A condition characterised in both input directions still yields **one** setup group and one
hold group, not two: UG p.7-56 asks a constraint group for at least one lookup table, not for
a particular one, and the values for each direction are looked up at the post-settled state
that direction actually settled into — which is how the input's direction is carried,
structurally, by which of `rise_constraint`/`fall_constraint` holds the values, rather than by
the condition or the `timing_type`. One setup group and one hold group is emitted per
condition the pin was characterised under; a condition whose every arc lost its reference —
because the state it names carries none — is emitted as neither, rather than as an empty
group with no lookup table to carry.

The catch-all's `default_timing` marking is written uniformly on both kinds of group it
applies to: the clock-to-output arc for an output's whenless state, and the setup/hold pair
for an input's whenless arcs. Both mean the same thing — whatever the conditioned groups do
not cover — and both are read after the conditioned groups for the same reason.

## 8. The regenerated `sdf_cond`

Every emitted `when` carries an `sdf_cond` beside it, and the two are never written apart — a
`when` with no `sdf_cond` would leave the SDF side of the same check unconditioned. The
`sdf_cond` is always **regenerated** from the very Boolean expression the `when` was parsed
from, through the same fold that renders the `when`, and never copied from anything the
source library wrote in an `sdf_cond` attribute of its own: the pair states one condition by
construction, rather than by two translations that could disagree.

It is rendered in **SDF 2.1 comparison form**: a pin is written as a comparison against a
one-bit literal (`P == 1'B1`, `P == 1'B0`) rather than as a bare name or its complement, which
is the form a Verilog timing-check condition takes.

Under `--latch`, the original arcs survive unchanged — including whatever `when` each was
characterised under, spelled exactly as the library wrote it — while the new pseudo-
synchronous groups carry the **representative** spelling of their state's collision class:
first appearance wins, so two arcs that collided into one class because they denoted the
same function keep their own distinct source spellings on the originals while the one pseudo
group speaks for the class in whichever spelling was seen first. Two spellings of one
function coexisting on the same cell this way is expected, not a defect: both are correct,
and nothing here promises byte-identical spellings between an original arc and the pseudo
group its state collided into.

Every condition `BoolExpr` builds is normalised to disjunctive normal form before it is ever
rendered, so a source `when` combining an XOR — or any disjunction — is regenerated in
`sdf_cond` as a sum of AND/NOT product terms rather than as Verilog's own `^`. That is the
plain, **accepted** consequence for a Verilog timing check reading the regenerated
condition: the surface form changes, the meaning does not, and pseudosync neither warns about
it nor guards against it.

## 9. An input driving both a converted and a skipped output

Such an input gets a constraint computed over the **converted outputs only**. An input
driving no converted output gets no constraint at all — the degenerate case of the same
rule, not a separate one.

In default mode this means a cell with one converted and one skipped output is emitted as
`ff` while the skipped output keeps its original combinational arcs, from a pin now marked
`nextstate_type : data`. That artefact is accepted: convertibility is decided per output, so
the converted output gets its flip-flop model and the skipped one is left exactly as the
input wrote it.

## 10. Two output modes, both for production

**Default — the flop model**, used during synthesis. A converted output's original non-reset
arcs are replaced by the pseudo-synchronous ones, and the `latch` group becomes `ff`.

**`--latch` — the latch model**, used to generate SDF for delay-annotated simulation, which
needs the real input-to-output delays rather than the phantom clock's. The `latch` group and
all original arcs are preserved, and the pseudo-synchronous arcs are added alongside them.

Skipped-ness itself is mode-independent — the same outputs are skipped under both modes.
What differs is what a skip *means*. Under `--latch` a skipped output simply gains no
pseudo-synchronous arcs, since the originals survive by construction. Under the default it
keeps its original arcs instead of having them replaced.

## 11. Exit status

**Exit 0** — the run completed. Some cells or outputs may have been skipped; they are named
on standard error and in the report. Skipping is not a fault the caller committed. There is
no way to tell pseudosync *convert only these cells* or *ignore those*, so converting what
is convertible is the contract rather than a degraded mode of it, and a non-zero status
would assert an error the caller did not make. If a cell selection facility is ever added,
this is the rule to revisit.

Because the exit status stays 0, the report and the standard-error warnings are the only
signal — which is why every skipped output and every left-alone candidate cell must reach
both.

**Exit 1** — the run could not produce its product at all. Four reasons:

- the input could not be read or parsed;
- an output could not be written;
- the command line was invalid;
- the library is broken — a candidate cell references an `lu_table_template` the library does
  not declare. Such a file reads and parses without complaint, so it fits none of the other
  three.

## 12. Two output artefacts never share a destination

The converted library and the reconstruction report are resolved to distinct destinations
before anything is written, so neither can overwrite the other.

`-` names the standard stream belonging to the artefact it is given to: standard input for
the input file, standard output for `--output`, standard error for `--report`.

No write error is discarded. A report that was not fully stored is never reported as written.

Destination collision is decided by comparing resolved paths — canonicalised parent plus
file name. Paths that alias the same file by other means are not detected.
