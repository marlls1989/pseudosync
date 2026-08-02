# The pseudo-synchronous split

How one characterised combinational arc becomes two arcs referred to a clock that does not exist, and
why a QDI circuit needs that done to it at all. This is the method the tool implements, and the
document the others refer back to: `reference-selection.md` covers which reference an arc is charged
against, `post-settled-states.md` how conditioned arcs are keyed, `emitting-the-two-models.md` what
is written out, and `limits-of-the-model.md` the cases the split cannot express.
`the-liberty-timing-model.md` introduces the format vocabulary used throughout.

The method is Section III and Figure 7 of the Pulsar paper (Sartori, Wuerdig, Moreira, Calazans,
*IEEE ASYNC 2019*), which extends the pseudo-synchronous technique of Thonnart, Beigné and Vivet
(*A Pseudo-Synchronous Implementation Flow for WCHB QDI Asynchronous Circuits*, *IEEE ASYNC 2012*,
pp. 73–80).

## 1. Why a clock at all: the combinational loops

A QDI pipeline is built from handshake loops. Each stage's completion detection feeds an
acknowledgement back to the stage before it, so forward logic and backward logic close a cycle
through the C-elements — and that cycle is, to a timing tool reading the netlist, a **combinational
loop**.

Conventional EDA tools cannot analyse a design containing one. They require the loop to be broken:
either at arcs the designer names, or — failing that — at points the tool picks itself. Breaking a
loop by disabling timing arcs through the C-elements does work, and it costs exactly what it
disables: all control over the internal timing arcs of the cells the loop was broken at (Thonnart
et al. §II.B). Those are the arcs a performance-oriented flow most wants to constrain.

Modelling the C-element as a **pseudo-flip-flop** removes the loop instead of cutting it. A
sequential element is a start point and an end point for timing analysis, not a path through, so a
cycle passing through one is no longer a cycle. Every combinational loop in the pipeline is broken
without disabling a single timing arc, and the tool can then treat the circuit as fully synchronous —
placement, sizing, in-place optimisation and all (Thonnart et al. §II.C).

That is what the split is *for*. Everything below is the arithmetic of making a C-element's
characterisation look like a flip-flop's.

## 2. The identity

A C-element's characterised path from an input to an output is a delay over two variables — the
input's slew and the output's load. A flip-flop states the same journey as two quantities against a
clock: a setup constraint on the data pin, and a clock-to-output propagation delay. The method
asserts that the first is the sum of the other two:

```
delay(A→Z) = propagation(G→Z) + setup(A→G)
```

`G` is the clock the flop model is organised around. Reading the identity right to left is the
conversion: given the measured `delay(A→Z)`, choose a `propagation(G→Z)` and let the setup constraint
be whatever remains.

The two halves are indexed differently, and that is the point rather than an accident:

| Half | Indexed by | Because |
|---|---|---|
| `propagation(G→Z)` | output load | a delay is a function of what the output drives |
| `setup(A→G)` | input slew | a constraint is a requirement on the arriving edge |

Each half is therefore one-dimensional, and between them they name both axes of the table they came
from.

### The setup constraint is an accounting device, not a requirement

A synchronous setup constraint states a real requirement: violate it and the flop captures the wrong
value. Nothing of the kind is true here. There is no such thing as a setup violation for a
C-element — it waits for its inputs by construction, which is what makes the circuit
delay-insensitive in the first place.

The constraint exists to carry the part of the cell delay attributable to the input signals, so that
a tool reasoning about paths has somewhere to put it (Thonnart et al. §III.D). It is a bookkeeping
split of a real delay across two arcs the tool understands, and it should not be read as a claim
about what the silicon requires.

## 3. The clock exists only in the file

`G` is a **phantom** pin. It is written into the emitted `.lib` and exists nowhere else — not in the
cell's layout, not in its abstract view, and as no net in the silicon. Nothing is connected to it and
no clock tree is ever built for it.

This is where pseudosync departs from the technique it implements. Thonnart et al. refer the model to
the cell's **real `Reset` pin**: every C-element needing a reset to initialise its state, and the
reset being distributed from a single root, the global reset can be declared a clock with ideal
propagation (§II.C). The original `Reset→Z` arc is then discarded, the reset network having become a
clock network.

Introducing a pin that exists nowhere else leaves the cell's real pins to their own descriptions
instead. The reset arcs a library characterised survive the conversion
(`emitting-the-two-models.md` §2), and the reset network remains a network the timing tool can
constrain on its own terms rather than one it is simultaneously trying to synthesise a clock tree
for.

Because the phantom pin exists only in this file, its own slew is not a physical quantity. There is
no edge arriving at `G` whose steepness could be measured, so the axis a real clock would contribute
is not merely unknown but meaningless — which is why the propagation half is free to be indexed by
load alone, and the constraint half by the data pin's slew alone.

## 4. Collapsing a two-dimensional table onto one axis

The measured arc is a grid: one value for each (slew, load) pair the library characterised. Each half
of the split is a single row of numbers. Something has to stand for the axis being dropped.

In the original method this is a reference row `I` and a reference column `J`, "arbitrarily picked or
selected with knowledge of the most common uses in a given technology" — an average transition time,
an average FO4 output capacitance (Thonnart et al. §III.B).

`--anchor` makes that choice explicit:

- **`middle`** (the default) reads the middle row, the middle column, and the middle element. Every
  number the tool emits is then a value the library actually measured, at a real characterised point.
- **`average`** takes the mean along the axis being collapsed. It uses every measurement rather than
  one, at the cost that the emitted number stands for no single characterised point.

Neither is more correct in general. `middle` keeps the output traceable to the input's own
measurements; `average` spreads the whole axis into the result.

This choice is orthogonal to `reference-selection.md`'s: that one decides *which* output's reference
an input is charged against, this one decides *how* a reference is read off whichever table was
chosen. Both are needed and neither substitutes for the other.

The arithmetic lives in `src/arcs.rs`, which reduces a characterised arc to the one-dimensional
slices the model is written from.

## 5. The residual

The identity does not hold exactly, and neither this tool nor the method it implements pretends
otherwise.

`delay(A→Z)` is a function of slew **and** load together. `propagation(G→Z)` and `setup(A→G)` are two
independently chosen numbers, one depending on load and one on slew. Adding them produces a surface
that is separable by construction, and the measured delay is not: a real delay's dependence on slew
can itself vary with load, which no sum of two one-variable functions can express.

The difference between the measured table and the table rebuilt by adding the two halves is the
**residual**. Two properties of the split follow directly from the arithmetic and are worth knowing:
the reference arc has a setup time of zero at the reference row, and at the reference column the two
halves of the reference arc sum to exactly the original delay. The error is zero where the reference
was read, and grows with distance from it.

The original method measured this too, and judged it acceptable for the purpose: the models are
meant for the optimisation phases of an implementation flow rather than for precise timing analysis,
where a rough approximation is enough to let the tools size and place instances sensibly (Thonnart
et al. §III.C). The residual is therefore the declared cost of the method, not a defect in this
implementation of it.

What this tool adds is measuring it on the library in front of you. The reconstruction report
(`src/report.rs`, rendered by `src/render.rs`) rebuilds each arc from its own two halves and states
the per-point relative error alongside per-arc and per-cell statistics, so the cost of a given
`--anchor`, `--when-merge` and reference mode is read rather than assumed.

## 6. Nothing is borrowed to fill a gap

The identity binds three quantities of one path. Substituting a term measured elsewhere would produce
a number describing no path in the cell.

Two consequences follow, and both are visible in the emitted library:

- An output the conversion did not convert has no propagation half. Nothing driving that output
  receives a setup constraint from that path, because there is no `propagation(G→Z)` for the
  remainder to be taken against.
- Another output's propagation is never substituted for a missing one. Beyond describing no real
  path, it would corrupt the one identity the reconstruction report exists to check: the rebuilt arc
  would differ from the measured one for a reason that has nothing to do with separability, and the
  residual would stop meaning what it means everywhere else.

An input driving several outputs is a real case rather than a gap, and `reference-selection.md`
covers what it is charged against.
