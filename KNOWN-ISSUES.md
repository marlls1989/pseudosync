# Known issues

Issues that are known and are being deferred for later solution.

An entry is added when a defect is found and deliberately left unfixed for now, and removed
when that issue is resolved. The file being empty means nothing is currently deferred.

Each entry should say where the defect is, what someone using the tool actually sees, and
how it was found, so that whoever picks it up later does not have to rediscover it.

---

## A delay family with no slew family beside it is representable and cannot occur

**The domain fact.** A characterised `cell_rise` or `cell_fall` delay table is **always**
accompanied by its slew table. Affirmed by this repository's owner, the author of the
method the tool implements. No legal input delivers one without the other.

**Where.** `TimingTables` carries the four families as independent optional tables, so the
type permits a delay with no transition beside it. `select_reference_arc` in `src/arcs.rs`
therefore has a branch refusing that combination — the `edge` closure's
`(delays?, transitions?)`.

**What is seen.** Nothing. The tool is correct today, and the deferred item is not a fault
in its behaviour.

**Why this is registered.** The branch exists, and a branch with no test reads as lost
coverage — reasonably so, on the evidence available to a reviewer, since nothing in the
tree records that the input cannot arrive. A review panel raised exactly that, calling for
the deleted test `arcs::select_reference_arc_requires_all_four_tables` to be restored on
the grounds that its replacement never constructs the case. An attempt to restore it then
failed to produce a test that could discriminate its own killer mutation. Left as it is,
this will be raised again by the next review, from the same evidence, to the same end.

**The fix: make the state unrepresentable, not tested.** An edge should carry its delay and
its slew as one thing — present or absent together — so a delay with no transition cannot
be constructed at all. Then there is no branch refusing it, no untested path for a review
to find, and no test to want: a test for it would pin an input that cannot arrive, which is
worse than no test.

Adding that test is explicitly **not** the fix, and neither is a comment asserting the
domain fact. Both leave the unreachable branch in place for the next reviewer to find, and
this repository does not treat prose as authority.

**Where the pairing belongs.** Not in the Liberty crate: that is a structural parser,
reading text and returning an AST, and it carries no semantics — asking it to guarantee
that a delay arrives with its slew is asking the wrong layer. The pairing is constructed on
the consumer's side of the crate boundary, in pseudosync, where the AST becomes the tool's
own timing representation. A family arriving without its partner then yields no edge, which
is exactly what happens today by a different route — the edge is built and
`select_reference_arc` refuses it — so the observable outcome is unchanged and the branch
disappears. Nothing detects, guards or reports: the pair is simply not constructible, which
keeps this inside the settled rule that nothing defends against malformed input.

The change reaches `TimingTables` and every reader of those four fields.

---

## The emission code has not been reviewed for how it selects what it emits

**Where.** Everywhere the tool builds a group it is going to emit — `src/emit.rs`,
`src/reset.rs`, and the emission paths in `src/engine.rs`.

**What is seen.** Nothing today. This is not a reported fault; it is a review that has not
been done.

**Why this is registered.** These sites were written at different times against different
inputs, and nobody has read them together against a single question: does this code take
the parts the output needs, or does it take whatever arrived and then remove the parts it
does not want? The second reads as equivalent and is not. It is correct only for the inputs
that were in front of whoever wrote it, and it silently carries through anything the author
had not met — so its behaviour on an input nobody has seen is undefined rather than
conservative.

**The example to follow.** `half` in `src/reset.rs`, which builds one half of a reset arc
that measured both directions. It was written to clone the arc and delete the opposite
direction's tables, and it now takes only the tables of the direction it names and discards
everything else. Same result on every input either has met, and one of them cannot go wrong
on an input neither has.

**The fix.** Read each emission site and settle, per site, what the emitted group is
supposed to contain; then have it construct exactly that. Where a site genuinely must pass
something through unexamined, say so where it does it, so the next reader knows it was
decided rather than defaulted. The repository's owner intends to do this pass himself.
