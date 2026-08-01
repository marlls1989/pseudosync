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
  standard error, naming the cell, the arc, the library and the reason
  (`src/engine.rs:944-947`), and skip that arc. This holds in every reference mode alike:
  none of the three has a fallback direction to charge a constraint to.
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

### Where the split is anchored

One knob decides the arithmetic of the split above, and it is orthogonal to
`--reference-mode`: that setting chooses *which output's* reference an input is charged
against, this one chooses *how* a reference is read off a table. It defaults to the
behaviour the tool has always had, so a run that does not name it is unchanged.

**`--anchor`** — the 2-D arc is slew x load and the split collapses one axis at a time, so
something has to stand for the axis being collapsed. `middle` (the default) takes the
middle row, the middle column and the middle element, so every number emitted is one the
library actually measured. `average` takes the mean over that axis instead: it uses every
measurement, at the cost of standing for no single characterised point.

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

Arcs that collide on one key share a reference. To **collide** is for two conditions to be
satisfiable together — for some assignment of the pins to meet both — and not merely for
them to denote one function; §6 states the rule and what it is drawn from. `Scope::Whole` is
the key `pooled` and `per-output` draw at — every arc of an output shares it, which is what
keeps those two modes exactly what they always were. `Scope::State` and `Scope::CatchAll` are
the two keys `per-state` draws at instead: a conditioned arc under the post-settled state its
condition names, a whenless arc under the catch-all for its own output and edge. Arcs whose
conditions can hold at once share one reference however many `when`s the library spelled them
under — which is why the finest key is per-*state* and not per-arc.

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

Two conditions **collide** when their conjunction is satisfiable — when some assignment of
the pins meets both — decided on a BDD rather than on text, and a state is the transitive
closure of pairwise collision, collision not being transitive on its own. So `A * B` and
`B * A` are one state, being one function; `A * B` and `A * B * C` are one state, because
`A * B` covers `A * B * C`; and `A * B` and `B * C` are one state, because both hold with all
three pins high.

UG p.7-49–50 is what this is drawn from: "You must define mutually exclusive conditions for
state-dependent timing arcs", mutually exclusive meaning that no more than one condition may
be met at any time. Two conditions that can both be met are therefore one state and not two,
whatever their spellings. A source `when` need not name every pin, which is what makes
overlap rather than equality the question: conditions are not all full assignments, and two
of them over different pin subsets can share satisfying assignments without denoting the same
function. The five cases are pinned in `src/conditions.rs` by
`collision_classes_intern_by_function_in_first_appearance_order`,
`a_condition_covering_another_is_one_class_with_it`,
`two_overlapping_conditions_are_one_class`, `disjoint_conditions_are_two_classes` and
`a_bridging_condition_closes_two_disjoint_ones_into_one_class`.

A collision is looked for **within one output pin** and never across two, and that same
requirement is why. It is about the state-dependent timing arcs of one pin pair: after the
split every propagation arc of an output is `G -> Q`, so two arcs of one output are told
apart by their `when` alone — including two arcs from different sources, whose `related_pin`
no longer separates them once converted. `G -> Q1` and `G -> Q2` are two different pin pairs,
so two outputs' conditions were never required to exclude one another and their overlapping
is not a state to resolve; emitting one output's arc under a condition drawn from another
output's characterisation would claim that output's numbers over a state it holds no table
for. `two_outputs_whose_conditions_overlap_are_not_one_state` in `src/engine.rs` pins that,
and `a_collision_is_sought_within_a_group_and_never_across_two` in `src/conditions.rs` the
classifier alone.

`--when-merge` decides how the several arcs an accumulator collects for one key are combined
into the single table the model emits for it: `mean` (representative), `max` (the
pessimistic envelope) or `min` (the optimistic one), elementwise per slew/load point. Under
`pooled`/`per-output` the key is the whole output, so this merges every `when`-conditioned
arc of a pin pair into the cell's one arc. Under `per-state` the key is one post-settled
state, so it instead resolves a **collision** — several conditions that can hold at once, and
are therefore one state — within that one state; arcs whose conditions cannot hold at once
are filed apart and never merged into each other.

## 7. Checks and clock arcs conditioned on a post-settled state

Under `per-state`, an input pin's checks are grouped by a second classification: the source
`when`s the library characterised that pin's arcs under, independent of the post-settled
classification above and drawn within one input pin, as that one is drawn within one output
pin. The conditioned groups come first, the catch-all last — the order Liberty reads a
`default_timing` group in.

Independent both ways: each classification can split what the other merged. One source
`when` stays one check group where the post-settled classification splits its two input
directions into two states, which
`per_state_checks_carry_their_own_states_arc_minus_its_own_crossing` in `src/engine.rs`
pins; and two of a pin's `when`s that cannot hold at once stay two check groups even where a
third pin's condition bridges them into a single delay state, each then carrying its own
condition's arc against that one state's reference, which
`a_bridged_delay_state_still_leaves_its_source_two_check_groups` pins.

Each group's `when` and `sdf_cond` state its class's condition **with nothing conjoined**:
what the check constrains is said by `timing_type` (`setup_rising`/`hold_rising`), never by
the condition it holds under. That condition is the library's own spelling wherever the
class's union equals one of its members — a class of one, a class of equal spellings, or a
class one of whose members covers the rest — and the class's minimised union otherwise, which
is a condition no library wrote. UG p.7-49–50's mutual-exclusivity requirement is about a
pin's state-dependent timing arcs, which a check group is as much as a delay group is, so a
class that overlaps without containment can be stated under no member of it.
`overlapping_check_conditions_on_one_pin_are_grouped_under_their_union` in `src/engine.rs`
pins that case, and `two_spellings_of_one_condition_are_one_check_group` the common one.

`setup_rising`/`hold_rising` name the **clock's** edge (RM p.332), which is why that suffix
stays fixed however many conditions the checks multiply into — the fictitious clock this
model refers everything to only ever rises.

A condition characterised in both input directions still yields **one** setup group and one
hold group, not two: UG p.7-56 asks a constraint group for at least one lookup table, not
for a particular one, and the input's direction is carried structurally, by which of
`rise_constraint`/`fall_constraint` holds the values, rather than by the condition or the
`timing_type`. Both directions of a group are looked up at the **group's own** scope — the
class its source `when`s fell into — and at no post-settled state: the group is the unit
here, one check on one input-to-clock pin pair, whose conditions UG p.7-49–50 requires to
exclude one another, so one condition is one value per direction however many outputs the
pin drives under it. What separates the two directions is the constraint family the values
are written to, not the key they are read at. Each value is that condition's mean arc in
that direction, over every output the pin drives under it, less the mean of the crossings
those arcs are charged — each one the reference of its own output and post-settled state.
`one_check_condition_over_two_outputs_carries_their_mean` in `src/engine.rs` pins that, on a
pin whose single check condition spans two outputs referred at two different states. One
setup group and one hold group is emitted per condition the pin was characterised under; a
condition whose every arc lost its reference — because the state it names carries none — is
emitted as neither, rather than as an empty group with no lookup table to carry.

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

Wherever an original arc is preserved — every arc under `--latch`, and in the default flop
mode a converted output's reset arcs (§10) together with every arc of a cell or an output the
conversion left alone (§1, §2) — whatever `when` and whatever `sdf_cond` the library wrote on
it survive unchanged, and so do its `timing_sense` and every value in its tables. Under
`--latch`, and on any cell or output the conversion left alone, the arc survives entire, its
`timing_type` included. The one exception is a converted output's reset arc in the flop model,
and it is confined to that tag: §10 restates the `timing_type` asynchronously and may split
the arc in two, both halves then carrying the one `when`, the one `sdf_cond` and the one
`timing_sense` the library wrote. The new pseudo-synchronous groups meanwhile
carry the **least restrictive** condition of their state's collision class: a class whose
union equals one of its members — a class of one, a class of equal spellings, or a class one
of whose members covers the rest, as `A * B * C * D` beside `A * B * C` — is stated in that
member's source spelling verbatim, and a class that overlaps without containment carries the
minimised union of its own members, rendered as a sum of product terms. A `when` need not be
a single product term. Preserved arcs keep their own spellings either way.

Several spellings of a condition coexisting in one emitted file is therefore expected, not a
defect, and in either output mode. Whatever the conversion preserves — a reset arc, an arc
of an output it skipped, every arc of a cell it is not a candidate for — keeps the spelling
the source library wrote, whatever that spelling is; the groups pseudosync generates carry
its own regenerated comparison form. All are correct, and nothing here promises
byte-identical spellings between a preserved arc and the pseudo group whose state its
condition collided into.

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

### What becomes of a converted output's reset arcs

The flop model gives a converted output a `rising_edge` arc against the phantom clock, while
the reset arcs kept beside it arrive from the input library under the combinational family of
timing types. An output stating a sequential arc beside combinational ones is not a model the
synthesis tool will hold, so each retained arc is restated under the **asynchronous type
naming the output arrival direction the arc's own tables state**: a fall family (`cell_fall`,
`fall_transition`) under `clear`, a rise family (`cell_rise`, `rise_transition`) under
`preset`. Liberty's two asynchronous output types are named for what each does to the output,
so what the tables measured is what names the type.

The tables are the discriminator, and the cell's `latch`/`latch_bank`/`ff`/`ff_bank` group is
never consulted for it: that group names **one** asynchronous function for the whole cell, so
it cannot describe a dual-rail cell whose reset clears one rail and presets the other. The
library's own practice is the evidence.
`examples/ASCEND_FREEPDK45_ALHO_nom_1.10V_25C.lib:29193` states `timing_type : clear` over a
`cell_fall`, and `:29854` states `timing_type : preset` over a `cell_rise` — both on the one
output bundle at `:28030`, from the same `related_pin : "RN"`, under the same
`timing_sense : positive_unate`, the same `when` and the same `sdf_cond`. The table family is
the only thing that differs between those two arcs, so nothing else can be what tells them
apart. That cell's `latch_bank` at `:36339-36340` meanwhile declares `clear : "!RN"` and no
`preset` at all — an active-low reset, whose deactivating arrival the library itself states
as a `preset`, and which the sequential group therefore does not describe.

A `timing` group states exactly one `timing_type`, so an arc measuring both arrivals becomes
**two** groups, each keeping its own family's tables, with `related_pin`, `timing_sense`,
`when` and `sdf_cond` unchanged on both, and every subgroup belonging to neither family kept
on both — such a subgroup describes the path rather than one of its arrivals, so dropping it
from one half would make two halves of one arc say different things. No order between the two
is specified: which is emitted first is not a property of the emitted library and must not be
relied on. `an_arc_carrying_both_edges_becomes_a_preset_and_a_clear` in `src/reset.rs` pins
the split, asserting the two as a set for that reason;
`a_split_half_keeps_the_condition_and_sense_the_library_wrote` and
`a_subgroup_of_neither_family_is_kept_on_both_halves` pin what both halves carry; and
`flop_mode_states_a_combinational_reset_arc_asynchronously` in `src/engine.rs` pins the whole
of it through a conversion.

An arc already stating `clear` or `preset` is emitted verbatim: the library, or an earlier run
over the same file, has already said which way the output arrives, so the restatement is
idempotent. `an_arc_already_stating_an_asynchronous_type_is_returned_verbatim` in
`src/reset.rs` pins that. `timing_sense` is neither read nor rewritten anywhere in the
restatement, which is why no concept of reset polarity exists anywhere in the tool: what the
tables measured is asked and answered on the output's own side, and the polarity of the pin
that drove it never enters the question. None of this happens under `--latch`, where the arc
keeps the tag the library wrote it under —
`latch_mode_leaves_a_combinational_reset_arc_as_the_library_wrote_it` in `src/engine.rs`.

An arc the rule cannot state is emitted **exactly as the library wrote it**, and a warning
naming the related pin, the output pin, the cell and the library is produced. It is built by
`restate_output_arcs`, which returns it rather than printing it (`src/engine.rs:714-718`),
and printed on standard error by the caller (`src/engine.rs:1319`), inside the `if !latch`
branch that gates the whole restatement (`src/engine.rs:1315`). Four shapes reach that: an
arc carrying no delay and no transition table either way; one whose
`combinational_rise`/`combinational_fall` suffix is contradicted by its own tables; one
stating no `timing_type`; and one stating a type that is neither combinational nor
asynchronous. This is **not a refusal** — §3 gains no case from it, nothing is refused at any
scope over it, and the conversion goes ahead around the arc. An arc this tool cannot read is
a fact about the input cell, and dropping it, or rejecting a library that was accepted
before, would cost the caller timing the library does carry.
`an_arc_that_cannot_be_stated_survives_unchanged_and_warns` in `src/engine.rs` pins that the
arc survives and that the warning is *produced*: it calls `restate_output_arcs` directly and
asserts one warning came back. Nothing in the suite reaches the print, and no test asserts
the message's wording.

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
