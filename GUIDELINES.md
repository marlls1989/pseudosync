# Contributing guidelines

The standing conventions for working on pseudosync, kept in one place so they need not be
rediscovered from the code or restated on every change. They apply to every contribution,
human or automated; the automated development tooling reads this file verbatim and treats
each rule as binding. If a change genuinely cannot be made without breaking one of these,
that is a decision to raise and settle explicitly — not to work around quietly.

What the tool *does* is not here — that is in `docs/`.

## Who changes these rules

A worker executing a step never edits this file. These rules are the constraints the work
is done under, not part of the work. A rule that seems wrong or missing is raised, never
committed.

Only the repository's owner changes these rules, or someone he has authorised for that
change, and the change is made outside the work it governs — never inside a step that the
rules are constraining. A commit that edits this file was therefore made deliberately and
by that route, not by a worker reaching outside its brief.

## What pseudosync is

A command-line tool that rewrites Liberty standard-cell timing files, re-describing a
latch-based asynchronous cell on the axes a flip-flop is characterised on so that
synchronous synthesis and timing tools will use it. It implements the method of the Pulsar
paper; `README.md` states the method and `docs/conversion-policy.md` states the contract.

**It is a binary, not a library.** No public API surface, no `lib.rs`. Every module sits
behind a private `mod`, and tests reach private items from a `#[cfg(test)]` module inside
the module under test — never by widening visibility to suit a test.

## Language and style

- British spelling everywhere it is written by us — identifiers, comments, documentation,
  commit messages and user-facing output (`analyse`, `serialise`, `behaviour`,
  `optimisation`).

## Writing comments, docs and messages

This covers everything we write in prose: code comments, doc comments, `README.md`, the
files under `docs/`, `CHANGELOG.md`, and commit and PR messages.

- **Describe what the thing is and does, in the present.** State the behaviour and the
  reason for it. Don't explain something by contrast with an approach that was considered
  and dropped — the reader is looking at what exists, not at its history — and don't narrate
  the intermediate states a value moved through to reach the one that matters.
- **Say it plainly; drop the superlatives.** No "powerful", "robust", "simply", "just",
  "significantly", "seamlessly". They add length and confidence without adding information,
  and a claim of ease often turns out to be untrue on the case that doesn't fit. State the
  fact and let it stand.
- **Don't dress up routine detail.** An ordinary implementation choice doesn't need to be
  announced as though it were the point; give it the weight it actually carries.
- **Write for the reader's context, not your own.** Introduce a term specific to this
  project or otherwise non-obvious before leaning on it, and where a name belongs to a
  particular tool or theory, say so. You finish a change holding a great deal of context the
  reader does not have; the prose has to bridge that gap rather than assume it away.
- A comment that only restates the code earns nothing. Spend the words on a non-obvious
  invariant and why it holds.

### Nothing enters the documentation without evidence

Every claim written into `docs/` carries what backs it: a page of the Liberty Reference
Manual or User Guide, a section of the Pulsar paper, a named test, or a measurement
actually run and reported.

A claim with no such backing is left out. A gap is correct; an inference written in a
confident voice is not. The documentation is derived work and holds no authority — a
sentence invented in it has been read as policy and implemented as if it were a decision
before now, which is how a defect gets manufactured rather than merely described.

This binds reading as well as writing: that the documentation states something is never
the justification for a test, an expected value, or a behaviour.

## Saying you do not know beats sounding certain

An admitted gap costs a question. A confident sentence resting on an assumption costs
whatever gets built on it, and it costs it later, when the assumption is buried under work
that took it as settled. Prefer the gap, every time.

So: never state an assumption in the voice of a fact. Keep what you verified apart from
what you believe — "I ran X and it printed Y" and "I think this does Y" are different
claims, and the second must never be dressed as the first. Where two readings of an
instruction lead to materially different work, say so and ask which is meant rather than
picking the likelier one and proceeding smoothly. Where you could not determine something,
say that you could not, and say what you tried.

A partial answer with its uncertain part named is worth more here than a whole one with an
invention inside it, and it is not a failure to deliver — it is the deliverable.

### What the doubt in these rules is aimed at

Several rules here instruct doubt: assume the code is broken, justify an expected value
from the model rather than from observation, let nothing into the documentation without
evidence, test past the point where it feels sufficient. They are aimed at two things — the
code, and whatever the person applying them asserts. They are not aimed at the repository's
owner.

His account of the domain — what the characterisation flow can produce, what the downstream
tools accept, what earlier work has already proven, which inputs are legal — is knowledge
this repository does not contain and that cannot be derived from it. Reading the code
establishes what it does, never what it should do. So where that reading and his account
disagree, **his account is the tiebreaker and the reading is the thing to re-examine.**

Ask once when something is genuinely unclear, and say plainly what you do not know. His
answer then settles it: it is not evidence to be weighed against your own analysis. Do not
re-test it, do not carry it forward as a risk, do not soften it into an observation inside a
brief, a plan or a review scope, and do not write a test to pin it. Demanding evidence for
what you assert is the rule here; demanding it from him is not.

## Read the documentation before guessing

When you need to know how something behaves — a crate, a library, an external tool, a file
format, a standard, an algorithm — read its documentation first, rather than inferring the
behaviour from source, a type signature, a prototype or observed output. This holds whatever
the thing is: the rustdoc for a dependency, the Liberty Reference Manual and User Guide for
the format we read and emit, the Pulsar paper for the method. A signature or a sample tells
you what happens to work; the documentation tells you what is actually guaranteed, and the
gap between the two is where the subtle bugs live.

### Verify what you are handed

A commit sha, file list or line number supplied by anything other than the repository is
checked against the repository before work rests on it.

## Correctness means semantic equivalence, not identical bytes

A change is correct when it produces the same library — the same cells, arcs, constraints
and templates, with the same content — not when the emitted file is byte-for-byte
identical.

The `examples/*` outputs are generated artifacts. Regenerate them with the tool; never edit
one by hand, and never regenerate one just to make a diff go away.

### Golden comparison is banned as a test, permitted as an instrument

The objection is not to comparing bytes. It is that a committed golden file makes the
recorded output the definition of correct, so a defect frozen into it is then defended by
the suite — and that it invites bending the work to satisfy a diff instead of to be
right.

Comparing against known-good output is legitimate as a throwaway instrument during
behaviour-preserving work, where it confirms an argument already made. Never as a
committed test, and never as a guide to development.

### Refactors preserve behaviour

A commit titled "refactor" that changes emitted output is a defect. If behaviour must
change, that is a separate commit whose message says so.

## Prefer real types to stand-ins

- A closed set of kinds is an `enum`, not a string token. Model it so that picking the
  variant *is* the classification, and an impossible combination can't be built in the first
  place.
- A key or record whose fields are told apart only by position — especially when several
  share a type — should be a named struct. If swapping two fields would still compile while
  silently changing behaviour, the fields need names.
- Where a state cannot legally occur, make it unrepresentable rather than testing that it is
  refused. A branch rejecting an impossible input reads as lost coverage to every later
  reviewer, and a test for it pins an input that cannot arrive.
- Choose each collection for how it is used: a hash map or set where the access is
  membership, lookup or grouping; an ordered map only where the iteration order is claimed by
  an external format. Don't reach for an ordered container just for its iteration order when
  nothing reads that order.

## Lints point at missing design

The tree builds clean under `clippy -D warnings`, and it stays that way by fixing what the
lint names rather than silencing it. A `too_many_arguments` or `type_complexity` warning is
telling you a type is missing — introduce it.

Reach for `#[allow]` only as a genuine last resort, and when you do, say in place what the
lint wanted, why the proper form can't be had here, and what the attribute buys. Suppressing
a lint to preserve a shortcut is the same move as loosening a test until it passes. And a
clean clippy run is only evidence of quality once you've checked the green isn't coming from
a suppression.

## Formatting and the green bar

`cargo fmt` is not a matter of taste. A tree that fails `cargo fmt --check` is broken.
Never hand-format around rustfmt, and never propose relaxing the check because the tree
currently fails it — fix the tree.

```
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

### Never claim work is done without running the check

Report the output you got, not the one you expected. "Should pass" is not a result.

## Work out the rule before adding a guard

If some input misbehaves and the temptation is to add a check that rejects or narrows it,
first understand why. A guard bolted on to dodge a limitation you haven't pinned down tends
to be wrong, and a rule that turns away otherwise-valid input is a real behaviour change —
raise it as one rather than letting it accrete silently at the edge of the code.

## Testing

### Tests are semantic and targeted

- One test pins one behaviour.
- A test lives in the module that owns the functionality.
- No golden-file comparison tests.
- No driving the binary from a test.
- No silent skips: a test that returns early when a fixture is missing is banned. Make
  the fixture synthetic, or delete the test.
- A test that cannot fail is worse than no test.

### Assume the code is broken

The working assumption when testing is that the code is wrong and the job is to find out
how. Anything that treats current behaviour as the reference has abandoned that
assumption before it starts.

### An expected value is justified by the model, never by observation

Never obtain an expected value by running the code. An expectation taken from observed
output can detect *change* but never *wrongness*, because it defines whatever the code
does as correct. "That is what it does" is not a justification; neither is "that is what
it has always done".

Derive each expected value from what the domain requires, and state the derivation.
Where a value cannot be justified from the model, that is a finding about the tool —
report it, rather than quietly recording what it currently emits.

### Every test records the mutation that proved it can fail

A passing test proves nothing until you can say what would have made it fail. So every
test carries a one-line record of a change to the code under test that was **actually
applied, actually run, and observed to turn that test red**. Imagining a plausible
mutation is not running one, and a recorded mutation that was never executed is worse
than none, because it reads as evidence and is none.

The record must discriminate. A mutation that reddens this test and three of its
neighbours has not shown that this test pins anything of its own. And if the mutation
you expected to kill a test leaves it green, that is a finding about the test — not a
licence to write down a different mutation. Fix it or delete it.

### Correct by construction, then verify

Reason out why a change is right before checking it. A passing comparison confirms an
argument; it never substitutes for having one. If the bytes match but nobody can say why
the change could not have altered them, all that has been learned is that this input did
not expose a difference.

Then verify anyway, and thoroughly. There is no such thing as too much testing.

### Test fixtures are invented, never copied from a private library

This repository is public. Fixtures are synthetic data written for the test. Never copy
cell names, pin names, function expressions, timing values or any other content out of a
proprietary or customer library into this tree. A private library may be used for local
verification whose outputs stay outside the repository — never as a source of committed
content.

## Git

A commit message ends with the description of the change — never an AI-attribution line or
session trailer.
