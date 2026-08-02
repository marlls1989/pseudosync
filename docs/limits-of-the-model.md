# Limits of the model: what the split cannot express

The split described in `the-pseudo-synchronous-split.md` needs particular things from a
characterisation, and not every legal Liberty arc supplies them. This document covers what those
requirements are, why each is a requirement rather than a preference, and what becomes of an input
that does not meet one.

The theme throughout: an input the model cannot express is a fact about that cell, not an error the
caller made. What the tool does about it is leave that part alone and say so, at the smallest scope
that will do.

## 1. What the tool is being asked to convert

A cell is a **candidate** when it declares both a `latch` group and a pin named by `--clock-pin`
(default `G`). That pairing is a declaration of intent: the cell is a latch, and the pin it is
already enabled by is the one the model should be organised around.

A cell failing that test is an ordinary cell, and is emitted verbatim and silently — no warning, and
nothing in the reconstruction report. Nothing was asked of the tool there, so the tool has nothing to
say about it. Only a candidate the conversion cannot honour is worth a reader's attention.

## 2. Convertibility belongs to an output, not to a cell

An output is convertible when at least one of its non-reset source pins supplies a **complete
reference**: `cell_rise`, `cell_fall`, `rise_transition` and `fall_transition`, on a template
carrying both a slew axis and a load axis.

All four families are needed because the split produces two things from them. The delays become the
clock-to-output arc; the transitions become that arc's output slews. A clock-to-output arc stating a
delay and no slew describes an edge arriving at no particular speed, which the next stage's own
characterisation is indexed by.

Both axes are needed because the two halves are indexed by different ones — the constraint by slew,
the delay by load (`the-pseudo-synchronous-split.md` §2). A template with no load axis carries no
information the propagation half could be built from.

The four families need not come from a single `timing` group. Arcs are merged across `when`
conditions first and each family is counted separately, so a pin characterised `combinational_rise`
under one condition and `combinational_fall` under another supplies a complete reference between
them.

Deciding this per output rather than per cell is what lets a latch cell carrying an output the state
element does not drive convert the rest of itself. That output's combinational arcs are an accurate
description of the silicon, and they survive intact.

## 3. An arc that determines nothing

Three kinds of arc cannot take part in the split, and each is skipped with a warning on standard
error naming the cell, the arc and the library.

**A one-axis template.** The split collapses a two-dimensional table one axis at a time, so an arc
whose template declares one axis has nothing to collapse. This is not a complaint about the input: a
one-dimensional template is ordinary Liberty and native to this domain — the templates this tool
generates are themselves one-dimensional, because the phantom clock's slew is not a physical quantity
(`the-pseudo-synchronous-split.md` §3). The warning names the missing axis rather than calling the
library malformed.

**No determinable input direction.** A constraint is keyed on the direction the *input* pin was
moving, and `timing_sense` is the only attribute that recovers it (RM p. 328). Under `non_unate` —
or with no `timing_sense` stated, for which Liberty gives no default — nothing determines it. This
holds in every reference mode alike: none of the three has a fallback direction a constraint could be
charged to.

**An unreadable `when`, under `per-state` only.** That mode files every arc under the post-settled
state its `when` describes, and a condition the tool cannot parse names no state. Under the other two
modes the arc still converts, neither drawing its reference from any condition, so an unreadable
`when` bears on nothing there. This is the one limit that varies with the mode.

## 4. An output with no complete reference

An output none of whose non-reset sources supplies a complete reference is skipped. Its timing groups
are left exactly as the input wrote them, it gains no clock-to-output arc, and the rest of the cell
still converts.

There is nothing to state. A clock-to-output delay is one half of an arc being split, so an output
with no arc being split has no such delay — and the alternative, borrowing another output's, would
describe a path that does not exist (`the-pseudo-synchronous-split.md` §6).

The output is named on standard error and recorded in the reconstruction report.

## 5. A cell whose arcs are not comparable: the domain

A cell is emitted verbatim when no output of it is convertible, or when the arcs the conversion would
transform are not all on one **domain**.

A domain is a lookup template *together with the dimensions the table carries* — the number of rows
and columns, which is the length of that template's two index lists. Differing dimensions are a
differing template whatever the name says, because two tables whose values stand at different
breakpoints cannot be transformed together. So this is one rule rather than two.

It holds at both levels at which arcs can disagree:

- **Between the outputs of a cell**, where a reference drawn from one would be read at breakpoints
  the other was never measured at.
- **Between the four families of a single `timing` group**, which matters as much. Reading the axes
  of one family and slicing all four at a row taken from another would publish one family's table
  under a template derived from a different one. That is wrong numbers rather than a refusal —
  emitted without complaint, and wrong.

The check is made before anything is mutated, so such a cell is emitted exactly as it arrived, latch
group included, and named on standard error and in the report. One such cell never stops the run: a
Liberty library holds many cells, and one the conversion cannot describe is no reason to discard the
rest.

## 6. A library that does not declare what it references

One case is not a skip but a refusal of the whole run: a candidate cell's characterisation table
names an `lu_table_template` the library does not declare.

Such a library is broken. The conversion would emit a cell referencing derived templates it has
nothing to build from, and the file reads and parses without complaint, so nothing else would catch
it. The run is refused before any conversion is attempted and no artefact is written, so a broken
library never yields a partial product.

## 7. These cascade

The scopes are not independent. Skipping an arc can leave an output short of a complete reference,
and skipping every output leaves the cell unconvertible. A single one-axis template can therefore
account for a cell emitted verbatim, and the warnings are what connect the two for a reader.

## 8. Why none of this is an error

Every case above except the undeclared template leaves the run's exit status at zero, and
`running-the-tool.md` covers why: there is no way to tell pseudosync which cells to convert, so
converting what is convertible is what the tool does rather than a degraded mode of it.

That is also why the warnings and the reconstruction report matter. They are the only signal that
anything was left alone, and every skipped output and every left-alone candidate cell reaches both.
