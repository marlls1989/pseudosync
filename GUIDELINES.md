# pseudosync — project guidelines

## This crate is a CLI, not a library
No public API surface. No `src/lib.rs`. Tests reach private items via
`#[cfg(test)] mod tests` inside the module under test — never by widening visibility.

## Never silence a lint
`#[allow]` is banned anywhere in the tree, including tests. A lint is a defect
report; fix the cause. If a lint appears unfixable, STOP and raise a BLOCKER.

## Tests are semantic and targeted
- One test pins ONE behaviour, with concrete expected values computed independently
  of the code under test.
- A test lives in the module owning the functionality.
- NO golden-file comparison tests.
- NO driving the binary (`Command`, `assert_cmd`).
- NO silent skips: a test that returns early when a fixture is missing is banned;
  make the fixture synthetic, or delete the test.
- A test that cannot fail is worse than no test.

## Test fixtures are invented, never copied from a private library
This repository is public. Fixtures must be synthetic data written for the test.
Never copy cell names, pin names, function expressions, timing values, or any other
content out of a proprietary or customer library into this tree. The
`examples/ASCEND_*` libraries are the only real-library fixtures and are already
public. A private library may be used for local verification whose outputs stay
outside the repository — never as a source of committed content.

## Formatting is mandatory
`cargo fmt` is not optional and not a matter of taste. `cargo fmt --check` is part
of the green bar; a tree that fails it is broken. Never hand-format around rustfmt,
and never propose dropping the check because it currently fails — fix the tree.

## Green bar
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
plus zero `#[allow]` in the tree.

## Refactors preserve behaviour
A commit titled "refactor" that changes emitted output is a defect. If behaviour
must change, that is a separate commit saying so in its message.

## Never claim work is done without running the check
Report the output you got, not the one you expected.
