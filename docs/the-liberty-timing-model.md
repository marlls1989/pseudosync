# The Liberty timing model: what the input states and what the output must

Background for the rest of `docs/`. Everything pseudosync does is a rearrangement of Liberty timing
groups, so the vocabulary here — arc, family, template, unateness, state-dependence — is assumed by
`the-pseudo-synchronous-split.md`, `reference-selection.md`, `post-settled-states.md` and
`emitting-the-two-models.md` without further introduction. It describes the format, not this tool;
what the tool does with it starts in the split document.

Liberty is the standard-cell timing format the commercial synthesis and static-timing tools read. A
library declares its cells, a cell declares its pins, and a pin declares the timing arcs that reach
it.

## 1. An arc is a `timing` group inside the pin it arrives at

A timing arc lives inside the pin it describes the arrival at, and names the pin it departs from:

```
pin (Z) {
  direction : output;
  timing () {
    related_pin  : "A";
    timing_sense : positive_unate;
    timing_type  : combinational;
    cell_rise       (delay_template)      { index_1(...); index_2(...); values(...); }
    rise_transition (delay_template)      { ... }
    cell_fall       (delay_template)      { ... }
    fall_transition (delay_template)      { ... }
  }
}
```

The group is anonymous; `related_pin` is what identifies the path. One pin may carry many `timing`
groups, from different sources or under different conditions, and the pair (source pin, arriving pin)
is what Liberty calls a pin pair. That pair, not the group, is the unit several of the format's rules
are stated over — which is why two arcs of one output can conflict where two arcs of different
outputs cannot.

## 2. Four table families: two delays and two slews

A characterised arc carries up to four tables, in two kinds:

- **Delays** — `cell_rise` and `cell_fall`: how long the arrival takes.
- **Transitions** — `rise_transition` and `fall_transition`: how fast the arriving edge itself moves,
  which becomes the input slew of whatever the pin drives next.

Each family is named for the direction the **output** moves in. A `cell_rise` table measures the
output rising, whichever way the input that caused it was moving.

The four are independent groups. Nothing in the format requires an arc to carry all four, and a
library may split them across several `timing` groups conditioned differently — a pin characterised
`combinational_rise` under one condition and `combinational_fall` under another states two of the
families in each.

## 3. A template declares the axes; a table names its template

A table's numbers mean nothing without the breakpoints they were measured at, and those live in a
separate `lu_table_template` declared at library scope:

```
lu_table_template (delay_template_7x7) {
  variable_1 : input_net_transition;
  variable_2 : total_output_net_capacitance;
  index_1 ("0.004, 0.008, ...");   /* seven input slews */
  index_2 ("0.5, 1.0, ...");       /* seven output loads */
}
```

A combinational delay table is typically two-dimensional in exactly this way: **input slew** along
one axis, **output load** along the other. The template names the variables and gives the
breakpoints; the table names the template and supplies the values.

One-dimensional templates are ordinary Liberty, not a degenerate case. A quantity that depends on one
variable is declared with one axis, and the templates pseudosync itself generates are of that shape.

A template's identity for any purpose that transforms tables together is the template **and the
dimensions the table actually carries** — the lengths of its index lists. Two tables indexed
differently cannot be combined whatever names they were declared under, because their values stand
at different points.

## 4. Unateness names the output's direction relative to the input's

`timing_sense` states how the arriving edge relates to the departing one (RM p.328):

| `timing_sense` | Meaning |
|---|---|
| `positive_unate` | the output moves the same way the input did |
| `negative_unate` | the output moves the opposite way |
| `non_unate` | the relationship is not determined by direction alone |

Because the tables are named for the output's direction, `timing_sense` is the only attribute that
recovers the **input's** direction from a table's name: invert the sense and the family tells you
which way the pin that caused the arrival was moving. Liberty states no default, so an arc with no
`timing_sense` determines nothing about its input's direction, and neither does a `non_unate` one.

## 5. `timing_type` names what kind of arc it is

Three groups of `timing_type` matter here:

- **Combinational** — `combinational`, `combinational_rise`, `combinational_fall`: a path straight
  through, which is how a latch's transparent behaviour is characterised.
- **Sequential** — `rising_edge`/`falling_edge` for a clock-to-output delay, `setup_rising` and
  `hold_rising` for the checks that constrain a data pin against a clock. The `_rising`/`_falling`
  suffix on a check names the **clock's** edge (RM p.332), never the constrained pin's.
- **Asynchronous** — `clear` and `preset`, named for what the arriving signal does to the output:
  `clear` drives it low, `preset` drives it high.

A `timing` group states exactly one `timing_type`.

A constraint group carries its values in `rise_constraint` and `fall_constraint` rather than in the
delay families, and it is *that* choice which records the constrained pin's own direction (RM p.336).
Direction is therefore carried structurally on a check, by which table holds the value, and never by
the `timing_type` suffix. UG p.7-56 asks such a group for at least one lookup table, not for a
particular one, so a group carrying only one direction's table is well formed.

## 6. State-dependent arcs, and the requirement on their conditions

An arc may be qualified by a `when`, making it apply only in the states the Boolean expression
describes, with an `sdf_cond` stating the same condition in the form a Verilog timing check reads:

```
timing () {
  related_pin : "A";
  when        : "B";
  sdf_cond    : "B == 1'B1";
  ...
}
```

UG pp. 7-49–50 requires that such conditions be **mutually exclusive**: "You must define mutually
exclusive conditions for state-dependent timing arcs", no more than one of which may be met at any
time. The requirement is per pin pair.

A `when` need not name every pin of the cell. Two conditions over different pin subsets can therefore
share satisfying assignments without being the same function, which makes overlap — rather than
equality of spelling or even of function — the question the requirement actually asks.

An arc with no `when` is the catch-all. Marking it `default_timing` states that it applies wherever
the conditioned groups do not, and it is read after them.

## 7. Why a C-element does not fit the flip-flop shape

A C-element is characterised the way any combinational path is: for each input, a `timing` group at
the output carrying the four families over slew × load. That describes the cell accurately, and it is
useless to a synchronous timing tool, which has no model for it and will not place it in a timing
path it can close.

A flip-flop is characterised on different axes entirely. Its data pins carry `setup_rising` and
`hold_rising` constraint groups against a clock pin — indexed by **slew**, since a constraint is a
requirement on when a signal arrived relative to another — and its output carries a `rising_edge`
delay from that clock, indexed by **load**, since a delay is a function of what it drives.

The two descriptions have no axis in common and no pin in common: the flop's description is organised
around a clock, and a C-element has none. Producing the second from the first is what
`the-pseudo-synchronous-split.md` describes.
