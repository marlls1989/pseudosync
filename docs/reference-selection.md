# Reference selection: how widely one reference is drawn

Which clock-to-output propagation an input's setup constraint is taken against, on a cell with more
than one output or more than one characterised condition. `the-pseudo-synchronous-split.md` states
the identity this serves; `post-settled-states.md` covers the finest of the keys described here.

## 1. The same reference must stand on both sides

The split is an identity over three quantities:

```
delay(A→Z) = propagation(G→Z) + setup(A→G)
```

`setup(A→G)` is computed as `delay(A→Z) − propagation(G→Z)`, so the propagation term subtracted from
the measured delay and the propagation term emitted on the output must be **the same number**. If the
constraint is derived against one value and the output states another, the two halves no longer
reconstruct the arc they came from, and the residual stops measuring separability and starts
measuring the mismatch instead.

On a cell with one output characterised under no condition there is one candidate and no choice to
make. The question arises only when a cell has several outputs, several conditions, or both: there is
then more than one measured propagation, and something must decide how widely any one of them is
allowed to stand.

That decision is what a **reference** is — the propagation chosen to represent some set of measured
arcs. `--reference-mode` chooses how finely those sets are keyed.

## 2. The ladder

The three modes are one ladder. They differ only in the key, and each rung divides what the rung
above it pooled:

| Mode | Keyed on | One reference stands for |
|---|---|---|
| `pooled` | the cell | every arc of every output |
| `per-output` | the output pin | every arc of that output |
| `per-state` | a post-settled state of an output | the arcs of that output holding in that state |

The finer the key, the fewer measurements one reference has to represent, and the smaller the
distortion from representing them.

**`pooled`** takes the mean across every output and charges it everywhere. On a cell whose outputs
are independent rails — a dual-rail bundle, where the two outputs are different circuit elements
rather than two views of one — this averages measurements that describe different things. It is
retained as a deliberately kept regression: it is the tool's original behaviour, kept so its cost
against the finer modes can be measured on a real library before any decision to remove it, not
because it is a designed alternative to them.

**`per-output`** gives each output its own reference. An input is then constrained against the
outputs it actually drives, rather than against a cell-wide figure including outputs it has no path
to.

**`per-state`** (the default) divides further, by the state the output settles into. An output
characterised under several conditions has a measured propagation for each, and pooling them would
represent several distinct behaviours with one number. `post-settled-states.md` describes what a
state is and how arcs are grouped into them.

All three coincide on a single-output cell characterised under no condition: the cell-wide mean of
one output is that output's own reference, and its one state is the whole output. They can differ the
moment either condition fails.

## 3. What an input driving several outputs is charged

An input pin is one pin, and its setup constraint is one number per direction — it cannot carry a
different constraint per output, because the check is a property of the input-to-clock pin pair.

Where a pin drives several outputs, its constraint is the mean of its arcs over those outputs, each
arc charged against the reference of **its own** output and state. The averaging is over the arcs;
the reference each arc is taken against is not averaged with it. Under `per-output` this is the mean
over every output the pin drives; under `per-state` it is that same averaging restricted to the
outputs the pin drives in the state being computed, which makes `per-output` the degenerate case of
one state per output rather than a separate rule.

## 4. An input driving both a converted and a skipped output

A pin may drive one output the conversion could express and another it could not
(`limits-of-the-model.md` covers why an output is skipped). Its constraint is computed over the
**converted outputs only**.

The skipped output contributes no propagation term, so there is nothing for a remainder to be taken
against on that path; including it would require inventing one. A pin driving no converted output at
all receives no constraint — the degenerate case of the same rule rather than a separate one.

This produces a cell that is a flip-flop in part: in the default mode the cell's sequential group
becomes `ff` and the converted output carries its pseudo-synchronous arcs, while the skipped output
keeps the original combinational arcs it arrived with, driven from a pin now marked
`nextstate_type : data`. That mixture is accepted rather than avoided. Convertibility is decided per
output, so the output that can be expressed gets the model and the one that cannot is left as the
library wrote it.

## 5. The derived templates do not vary with the mode

The conversion generates a `<name>_pseudo_constraint` and a `<name>_pseudo_delay` lookup template for
each template the arcs it converted were characterised on, and prepends them to the library
(`src/templates.rs`, `src/engine.rs`).

These describe axes and breakpoints alone — slew for the constraint, load for the delay. How widely a
reference was drawn changes which values are emitted, never what they are indexed by, so one pair per
source template is generated whichever mode was asked for.
