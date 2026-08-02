# Emitting the flop and latch models

What each of the two output models writes, what it preserves, and how a condition is regenerated on
the way out. `the-pseudo-synchronous-split.md` states where the emitted numbers come from and
`post-settled-states.md` what they are keyed by; this document covers the groups they are written as.

Construction of the emitted groups lives in `src/emit.rs`, the reset restatement in `src/reset.rs`,
and the walk that applies them in `src/engine.rs`.

## 1. Two models, both for production

The same conversion is written out in two shapes, and both are used in a real flow:

**The flop model** (the default) is what synthesis reads. A converted output's original non-reset
arcs are replaced by the pseudo-synchronous ones, and the cell's `latch` group becomes `ff`, with
`enable` becoming `clocked_on` and `data_in` becoming `next_state`. The cell now looks to the tool
like a flip-flop, which is the whole object of the exercise.

**The latch model** (`--latch`) is what delay annotation reads. The `latch` group and every original
arc are preserved, and the pseudo-synchronous arcs are added **alongside** them rather than replacing
them. SDF extraction needs the real input-to-output delays rather than the phantom clock's, and this
is the library that still carries them.

The two are interchangeable at the file level: a design synthesised against the flop library has its
SDF extracted by swapping this library in for that one, with nothing else about the design changing.
The latch's sequential arcs keep the timing loop broken, while the original combinational arcs supply
the delays to annotate.

Keeping both in one library is what the latch model is for. The original flow also swapped libraries
at sign-off, from the pseudo-synchronous models to the plain asynchronous ones (Thonnart et al.
§IV.B, Figure 5) — but those plain models carry only the combinational arcs, so the handshake cycles
they close are combinational loops again, and the reason the pseudo-synchronous model existed
(`the-pseudo-synchronous-split.md` §1) is given up exactly when the delays are to be annotated. The
latch model keeps the sequential arcs alongside the real ones, so the loop stays broken through
annotation and the swap costs nothing.

Which outputs are skipped is the same under both models — that decision is about what the arcs can
express, not about which model is being written (`limits-of-the-model.md`). What differs is what a
skip *means*. Under `--latch` a skipped output gains no pseudo-synchronous arcs, the originals
surviving by construction; under the default it keeps its original arcs instead of having them
replaced.

## 2. What the flop model does with a converted output's reset arcs

An asynchronous reset arc is characterised like any other path through the cell: under a
combinational `timing_type`, because that is what the measurement is. In the flop model that becomes
a problem the latch model does not have.

A converted output carries a `rising_edge` arc against the phantom clock. Beside it sit the reset
arcs, still tagged combinational. An output stating a sequential arc beside combinational ones is not
a model the synthesis tool will hold — it declines to model the cell at all, which costs the caller
the very thing the conversion was for.

Each retained reset arc is therefore restated under the asynchronous type naming **the output
arrival direction its own tables state**:

| The arc's tables | Restated as |
|---|---|
| `cell_fall`, `fall_transition` | `clear` |
| `cell_rise`, `rise_transition` | `preset` |

Liberty's two asynchronous output types are named for what each does to the output
(`the-liberty-timing-model.md` §5), so what the tables measured is what names the type.

### Why the tables decide and the sequential group does not

The obvious alternative — read the cell's `latch`/`ff` group, which declares `clear` and `preset`
functions — does not work, and the reason is structural rather than incidental.

That group names **one** asynchronous function for the whole cell. A dual-rail cell whose reset
clears one rail and presets the other cannot be described by it, and such cells are exactly what this
tool exists for.

The libraries themselves settle it. In the FREEPDK45 example library shipped in `examples/`, one
output bundle carries two arcs from the same `related_pin`, under the same `timing_sense`, the same
`when` and the same `sdf_cond` — one stating `timing_type : clear` over a `cell_fall`, the other
`timing_type : preset` over a `cell_rise`. The table family is the only thing that differs between
them, so nothing else can be what tells them apart. That cell's `latch_bank` meanwhile declares a
`clear` function and no `preset` at all: an active-low reset, whose deactivating arrival the library
itself states as a `preset`, and which the sequential group therefore does not describe.

### An arc measuring both arrivals becomes two

A `timing` group states exactly one `timing_type`, so an arc whose tables measure both a rise and a
fall cannot be restated as one group. It becomes two.

Each half is **built from the two table families of the arrival it names**, and every other subgroup
the arc carried is discarded. Taking what the half needs, rather than taking the whole arc and
removing the opposite direction's tables, is what makes a half structurally unable to carry data
describing the direction it does not name: a list of what to shed can only name the group types
whoever wrote it had met, so an unfamiliar one would ride onto both halves unexamined, while a list
of what to keep has no such gap.

The attributes are untouched — `related_pin`, `timing_sense`, `when` and `sdf_cond` are unchanged on
both halves, as are the values of the surviving tables. No order between the two is specified: which
is emitted first is not a property of the emitted library.

An arc whose tables state one arrival is not split. It is retagged and keeps every subgroup it had.

### The cases that pass through untouched

An arc already stating `clear` or `preset` is emitted verbatim. The library, or an earlier run over
the same file, has already said which way the output arrives, so restating is idempotent.

`timing_sense` is neither read nor rewritten anywhere in the restatement. This is why no concept of
reset polarity exists anywhere in the tool: what the tables measured is asked and answered on the
output's own side, and the polarity of the pin that drove it never enters the question.

None of this happens under `--latch`, where every arc keeps the tag the library wrote it under.

An arc the rule cannot state is emitted **exactly as the library wrote it**, with a warning naming
the related pin, the output pin, the cell and the library. Four shapes reach that: an arc carrying no
delay and no transition table either way; one whose `combinational_rise`/`combinational_fall` suffix
is contradicted by its own tables; one stating no `timing_type`; and one stating a type that is
neither combinational nor asynchronous. This is **not a refusal** — nothing is refused at any scope
over it and the conversion goes ahead around the arc. An arc this tool cannot read is a fact about
the input cell, and dropping it, or rejecting a library that was accepted before, would cost the
caller timing the library does carry.

## 3. Conditions on the way out

Every emitted `when` carries an `sdf_cond` beside it, and the two are never written apart: a `when`
with no `sdf_cond` would leave the SDF side of the same check unconditioned.

The `sdf_cond` is **regenerated** from the same Boolean expression the `when` was parsed from,
through the same fold that renders the `when` — never copied from whatever the source library wrote
in an `sdf_cond` of its own. The pair then states one condition by construction, rather than by two
translations that could disagree.

It is rendered in SDF 2.1 comparison form: a pin written as a comparison against a one-bit literal
(`P == 1'B1`, `P == 1'B0`) rather than as a bare name or its complement, which is the form a Verilog
timing-check condition takes.

Every condition is normalised to disjunctive normal form before it is rendered. A source `when`
combining an XOR — or any disjunction — is therefore regenerated as a sum of AND/NOT product terms
rather than as Verilog's own `^`. The surface form changes and the meaning does not; the tool neither
warns about this nor guards against it.

### Preserved arcs keep their own spellings

Wherever an original arc survives — every arc under `--latch`, and in the flop model a converted
output's reset arcs together with every arc of a cell or output the conversion left alone — the
`when`, the `sdf_cond`, the `timing_sense` and every table value the library wrote survive unchanged.
The one exception is the flop model's reset restatement, and it is confined to the `timing_type` tag:
a split arc's two halves each carry the one `when`, the one `sdf_cond` and the one `timing_sense`
their source had.

The groups the conversion generates meanwhile carry the least restrictive condition of their state's
collision class (`post-settled-states.md` §4), which for a class that overlaps without containment is
a minimised union no library wrote.

Several spellings of one condition therefore coexist in an emitted file, in either model, and that is
expected rather than a defect. Preserved arcs keep the source library's spelling; generated groups
carry the regenerated comparison form. All are correct, and nothing here promises byte-identical
spellings between a preserved arc and the pseudo group whose state its condition collided into.
