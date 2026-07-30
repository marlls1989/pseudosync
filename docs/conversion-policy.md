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

**Arc.** The template a timing arc names declares only one axis. Warn on standard error,
naming the cell, the arc, the library, the template and the missing axis, and skip that arc
— standard error only, no report entry. A one-dimensional template is legal Liberty and
native to this domain: the templates this tool generates are themselves one-dimensional,
because the phantom clock's own slew is ignorable. The warning must not call such an input
malformed.

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

## 5. An input driving both a converted and a skipped output

Such an input gets a constraint computed over the **converted outputs only**. An input
driving no converted output gets no constraint at all — the degenerate case of the same
rule, not a separate one.

In default mode this means a cell with one converted and one skipped output is emitted as
`ff` while the skipped output keeps its original combinational arcs, from a pin now marked
`nextstate_type : data`. That artefact is accepted: convertibility is decided per output, so
the converted output gets its flip-flop model and the skipped one is left exactly as the
input wrote it.

## 6. Two output modes, both for production

**Default — the flop model**, used during synthesis. A converted output's original non-reset
arcs are replaced by the pseudo-synchronous ones, and the `latch` group becomes `ff`.

**`--latch` — the latch model**, used to generate SDF for delay-annotated simulation, which
needs the real input-to-output delays rather than the phantom clock's. The `latch` group and
all original arcs are preserved, and the pseudo-synchronous arcs are added alongside them.

Skipped-ness itself is mode-independent — the same outputs are skipped under both modes.
What differs is what a skip *means*. Under `--latch` a skipped output simply gains no
pseudo-synchronous arcs, since the originals survive by construction. Under the default it
keeps its original arcs instead of having them replaced.

## 7. Exit status

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

## 8. Two output artefacts never share a destination

The converted library and the reconstruction report are resolved to distinct destinations
before anything is written, so neither can overwrite the other.

`-` names the standard stream belonging to the artefact it is given to: standard input for
the input file, standard output for `--output`, standard error for `--report`.

No write error is discarded. A report that was not fully stored is never reported as written.

Destination collision is decided by comparing resolved paths — canonicalised parent plus
file name. Paths that alias the same file by other means are not detected.
