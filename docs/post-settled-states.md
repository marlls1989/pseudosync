# Post-settled states and condition collision

How arcs are grouped when a cell was characterised under `when` conditions, and why grouping by
Boolean equality is not enough. This is the finest rung of `reference-selection.md`'s ladder, and it
applies under `--reference-mode per-state` alone. `the-liberty-timing-model.md` introduces
state-dependent arcs and the mutual-exclusivity requirement the grouping serves.

The classification runs in `src/conditions.rs`, which decides collision and builds the classes, and
`src/engine.rs`, which draws them at the right scope and computes against them.

## 1. What a state is

A cell characterised under conditions does not have one behaviour, it has one per condition. Under
`per-state`, each gets its own reference rather than being pooled into an average of behaviours that
never occur together.

For each delay family an arc carries, the **post-settled state** is:

```
the arc's own `when`   ∧   a literal naming which way the constrained pin settled
```

The literal is `P` if the pin settled high, `!P` if it settled low. A whenless arc's state is the
literal alone, and it is filed under a catch-all for its own output and edge.

The literal is what separates a pin's two directions. One condition characterised in both directions
describes two states, because the cell is in a different configuration after the pin has risen than
after it has fallen, and the propagation measured in each is a different measurement.

## 2. Which way the input was moving

The literal follows the **input's** transition, not the output's.

Table families are named for the output (`the-liberty-timing-model.md` §2), so the family alone does
not say which way the pin being constrained was moving. `timing_sense` is what recovers it: it states
the output's direction relative to the input's, so inverting it gives the input's direction from the
family name. Under `positive_unate` they agree; under `negative_unate` they are opposite, and a
`negative_unate` arc's `cell_rise` family is entered by its input **falling**. That arc is filed
under `!P`, even though the table it came from is named for a rise.

The same reading decides which of `rise_constraint`/`fall_constraint` a derived setup value is
written to (RM p.336). Both questions — the state a delay is filed under, and the family a constraint
is written to — are answered from the table family and the `timing_sense`, and never from the
`timing_type` suffix, which names the clock's edge rather than the constrained pin's
(`the-liberty-timing-model.md` §5).

An arc whose `timing_sense` is absent, unrecognised or `non_unate` determines no input direction at
all, and `limits-of-the-model.md` covers what becomes of it.

## 3. Collision: conditions that can hold at once

Two conditions **collide** when their conjunction is satisfiable — when some assignment of the pins
meets both. This is decided on a BDD, over the functions the expressions denote, rather than on their
text.

Overlap rather than equality is the right question because of what UG pp. 7-49–50 requires: "You must
define mutually exclusive conditions for state-dependent timing arcs", no more than one of which may
be met at any time. Two conditions that can both hold are therefore one state and not two, whatever
they were spelled as. A `when` need not name every pin of the cell, so two conditions over different
pin subsets can share satisfying assignments without denoting the same function — and a classifier
interning on Boolean equality would emit both as separate states, producing exactly the library the
requirement forbids.

Three cases show the range:

- `A * B` and `B * A` are one state, being one function. Equality is the easy case, and it is the
  only one a text or function comparison catches.
- `A * B` and `A * B * C` are one state, because the first covers the second. Every assignment
  meeting the second meets the first.
- `A * B` and `B * C` are one state, because both hold with all three pins high — although neither
  covers the other and they denote different functions.

## 4. A state is the transitive closure of collision

Collision is not transitive. `A` and `B * C` may be disjoint while a third condition overlaps both,
and mutual exclusivity is violated by each overlapping pair regardless of the pairs that do not
overlap. A state is therefore the **transitive closure** of pairwise collision: a bridging condition
closes two otherwise disjoint conditions into one class.

Each class is stated under the **least restrictive** condition that covers it — the union of its
members — short-circuited to a member wherever the union equals one of them. A class of one, a class
of equal spellings, and a class one of whose members covers the rest are therefore stated in the
library's own spelling, and only a class that overlaps without containment carries a minimised union
no library wrote.

The classification is consumed where it is made rather than relabelled afterwards, so the blended
reference and the constraint arithmetic are both computed against the merged state.

## 5. Collision is sought within one output, never across two

The requirement is about the state-dependent timing arcs of **one pin pair**, and that is the scope
the classifier draws at.

After the split, every propagation arc of an output departs from the phantom clock: they are all
`G → Q`. Two arcs of one output are then told apart by their `when` alone — including two arcs that
came from different source pins, whose `related_pin` no longer distinguishes them once converted.
Those genuinely are one pin pair's state-dependent arcs, and their conditions must exclude one
another.

`G → Q1` and `G → Q2` are two different pin pairs. Liberty never required their conditions to exclude
one another, so two outputs whose conditions overlap are not one state, and merging them would be
wrong rather than merely unnecessary: stating one output's arc under a condition drawn from another
output's characterisation claims that output's numbers over a state it holds no table for.

## 6. An input's checks are classified separately

Under `per-state`, an input pin's setup and hold groups are classified by a **second, independent**
grouping: the source `when`s that pin's arcs were characterised under, drawn within one input pin as
the first is drawn within one output pin.

The two are independent in both directions, and either can split what the other merged:

- One source `when` stays **one** check group even where the post-settled classification splits its
  two input directions into two states — because a check group's identity is the condition, and the
  direction is carried by which constraint table holds the value.
- Two of a pin's `when`s that cannot hold at once stay **two** check groups even where some third
  pin's condition bridges them into a single delay state. Each then carries its own condition's arc,
  charged against that one bridged state's reference.

## 7. What a check group is stated under, and how many there are

Each group's `when` and `sdf_cond` state its class's condition **with nothing conjoined**. What the
check constrains is said by `timing_type` — `setup_rising`, `hold_rising` — never by the condition it
holds under. Those suffixes name the phantom clock's edge (RM p.332), and since that clock only ever
rises, the suffix is fixed however many conditions the checks multiply into.

A check group is a state-dependent timing arc as much as a delay group is, so UG pp. 7-49–50 binds it
too: a class that overlaps without containment can be stated under no member of it, and carries the
union.

A condition characterised in **both** input directions yields one setup group and one hold group, not
two. UG p. 7-56 asks a constraint group for at least one lookup table rather than for a particular
one, and the direction is carried structurally by which of `rise_constraint`/`fall_constraint` holds
the values. Both directions are looked up at the **group's own** scope — the class its source `when`s
fell into — and at no post-settled state, because the group is the unit: one check on one
input-to-clock pin pair, whose conditions must exclude one another, so one condition yields one value
per direction however many outputs the pin drives under it.

A condition whose every arc lost its reference — because the state it names carries none — is emitted
as neither group, rather than as an empty group with no lookup table to carry.

The catch-all's `default_timing` marking is written on both kinds of group it applies to: an output's
whenless clock-to-output arc, and an input's whenless setup/hold pair. Both mean whatever the
conditioned groups do not cover, and both are read after them for that reason.

## 8. Merging within a state

`--when-merge` decides how the several arcs collected for one key become the single table emitted for
it: `mean` (representative), `max` (the pessimistic envelope) or `min` (the optimistic one),
elementwise per slew/load point.

What it is doing differs by mode. Under `pooled` and `per-output` the key is the whole output, so it
merges every `when`-conditioned arc of a pin pair into the one arc those modes emit. Under
`per-state` the key is a single state, so it instead resolves a **collision** — several conditions
that can hold at once and are therefore one state — within that state alone. Arcs whose conditions
cannot hold at once are filed apart and never merged into each other.
