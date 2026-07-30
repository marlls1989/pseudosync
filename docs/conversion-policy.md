# Conversion policy

What pseudosync guarantees about the library it emits, what it refuses to do, how it
reports that, and what the exit status means.

This exists because the tool used to answer that question four different ways depending
on which code path noticed the problem first: it half-converted a cell, or omitted one
with nothing but a line on standard error, or aborted the whole run on an internal
assertion, or carried on using a number measured somewhere else. Those were not four
decisions; they were the absence of one.

---

## 1. Convertibility is decided per output, not per cell

An output pin is **subject to pseudosync** when at least one of its non-reset source
pins supplies a complete reference: `cell_rise`, `cell_fall`, `rise_transition` and
`fall_transition`, on a lookup template with both a slew axis and a load axis.

All four families are needed because the conversion produces two things from them — the
delays become the clock-to-output arc, the transitions become that arc's output slews —
and both axes are needed because the constraint is indexed by slew and the delay by
load. A template with no load axis describes something this conversion has no way to
re-express.

The four families need not come from a single `timing` group. Arcs are merged across
`when` conditions first, and each family is counted separately, so a pin characterised
`combinational_rise` under one condition and `combinational_fall` under another supplies
a complete reference between them.

## 2. An output not subject to pseudosync is left exactly as the input wrote it

This is **not an error**. A latch cell may carry an output the state element does not
drive, and that output's combinational arcs are an accurate description of the silicon
which must survive the conversion intact.

Such an output is skipped: its timing groups are not removed, no clock-to-output arc is
added, and a warning naming the cell and the output is written to standard error and
recorded in the reconstruction report.

## 3. A cell is left alone only when no output at all is convertible

If no output of a qualifying cell is subject to pseudosync, there is nothing to build a
flip-flop model from, and the cell is emitted byte-for-byte as the input wrote it —
latch group included. It is named on standard error and in the report.

One such cell never stops the run. A Liberty file holds many cells and many libraries,
and one cell the conversion cannot describe is not a reason to discard the rest.

## 4. An input feeding no converted output carries no constraint

A setup constraint is an input arc minus the clock-to-output delay charged for that
path. Where an input drives only outputs that were skipped, there is no such delay, so
no constraint is emitted for it. The tool does not substitute a delay measured on a
different output in order to produce a number.

## 5. Partial conversion is normal operation and does not change the exit status

**Exit 0** — the run completed. Some cells or outputs may have been skipped; they are
named on standard error and in the report.

**Exit 1** — the run could not produce its product at all: the input could not be read
or parsed, an output could not be written, or the command line was invalid.

Skipping is not a fault the caller committed. There is no way to tell pseudosync
*convert only these cells* or *ignore those*, so converting what is convertible is the
contract rather than a degraded mode of it, and a non-zero status would assert an error
the caller did not make. If a cell selection facility is ever added, this is the rule to
revisit.

Because the exit status stays 0, the report and the standard-error warnings are the only
signal — which is why every skip and every left-alone cell must reach both.

## 6. Malformed input produces an error, not a crash

Any condition that can be decided by reading the input file is reported against the cell
or output it concerns. The tool does not assert, unwrap or index its way into a panic on
the strength of what a library happens to contain.

An invariant that an earlier step has already established is a different matter: where
code relies on something a previous decision guarantees, it says so and fails loudly if
that guarantee is ever relaxed. Such a failure is a defect in pseudosync, never a
complaint about the input.

## 7. Two output artefacts never share a destination

The converted library and the reconstruction report are resolved to distinct
destinations before anything is written, so neither can overwrite the other.

`-` names the standard stream belonging to the artefact it is given to: standard input
for the input file, standard output for `--output`, standard error for `--report`.

No write error is discarded. A report that was not fully stored is never reported as
written.

---

## Deviations at the time of writing

This policy is the decision; the code does not yet meet all of it. Each item below is a
known gap, not a hidden one.

- **Skipping is not yet implemented.** An output with no usable reference currently
  causes the whole cell to be left alone (rule 2 not met), and in one case escapes that
  and produces a cell that is part latch and part flip-flop.
- **Some malformed input still panics** rather than being reported: a characterisation
  table whose values are missing, non-numeric, or of unequal row lengths, and outputs
  whose references disagree on template or table shape (rule 6 not met).
- **The two artefacts can still collide.** Passing the same path to `--output` and
  `--report` writes the report over the library. `-` given to `--output` creates a file
  named `-` rather than writing to standard output (rule 7 not met).
- **Some report write errors are discarded**, so report content can be lost silently
  (rule 7 not met).
- **A left-alone cell reaches standard error but not the report** (rules 3 and 5 not
  met).

This section shrinks as the gaps close, and is empty when the code matches the policy.
