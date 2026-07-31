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
