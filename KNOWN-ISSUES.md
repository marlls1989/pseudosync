# Known issues

Issues that are known and are being deferred for later solution.

An entry is added when a defect is found and deliberately left unfixed for now, and removed
when that issue is resolved. The file being empty means nothing is currently deferred.

Each entry should say where the defect is, what someone using the tool actually sees, and
how it was found, so that whoever picks it up later does not have to rediscover it.

---

## A missing delay family beside a present transition family is unpinned

**Where.** `select_reference_arc` in `src/arcs.rs` refuses an edge unless it carries both a
delay family and a transition family — the `edge` closure destructures `(delays?,
transitions?)`. That behaviour is correct. Nothing in the suite pins it.

**What is seen.** Nothing today: the tool behaves correctly. The exposure is that the
behaviour could be weakened and no test would notice. Proven by execution — with

```rust
let (delays, transitions) = (delays.or(transitions)?, transitions?);
```

substituted in that closure, so a transition table silently stands in for a missing delay
table, the whole suite still passes.

**How it was found.** A review panel observed that
`arcs::select_reference_arc_requires_all_four_tables` had been deleted as subsumed by
`a_scope_decides_how_complete_a_reference_has_to_be`, and that the subsumption claim was
false: the survivor's two helpers drop both tables of an edge together, or drop a
transition while keeping its delay — the mirror of the missing case. Neither constructs a
delay missing beside a present transition.

**The trap, for whoever re-pins it.** The obvious fixture — clear `cell_rise`, keep
`rise_transition`, clear both fall families — does **not** discriminate the mutation, and
was confirmed not to by running it. `select_reference_arc` later reads

```rust
let sized = timing_tables.cell_rise.as_ref().or(timing_tables.cell_fall.as_ref())?;
```

which consults the raw delay fields rather than anything the `edge` closure computed. With
both delay families cleared, the function returns `None` there for a reason unrelated to
the mutation, so mutated and unmutated runs are indistinguishable. A fixture that keeps
`cell_fall` present and clears only `fall_transition` lets that lookup succeed, leaving the
two edges as mirrored gaps; the mutation then rescues only the rise side, `Scope::State`
and `Scope::CatchAll` begin accepting, and the test reddens while
`a_scope_decides_how_complete_a_reference_has_to_be` stays green. That construction is
untried.
