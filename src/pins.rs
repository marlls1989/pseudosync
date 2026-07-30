//! Pin direction predicates, cell qualification, and which pin groups own
//! or receive timing.

use lazy_static::lazy_static;
use liberty_parser::{ast::Value, liberty::Group};
use regex::Regex;

lazy_static! {
    pub(crate) static ref LATCH_REGEX: Regex = Regex::new(r"^latch").unwrap();
}

/// Tests if the cell contains a latch group and a pin with the expected clock_pin name
pub(crate) fn cell_qualifies(cell: &Group, clock_name: &str) -> bool {
    cell.subgroups
        .iter()
        .any(|group| LATCH_REGEX.is_match(&group.type_))
        && cell.iter_pins().any(|pin| pin.name == clock_name)
}

/// Check if a pin is an output pin
pub(crate) fn is_output_pin(pin: &Group) -> bool {
    (pin.type_ == "pin" || pin.type_ == "bundle")
        && pin
            .simple_attribute("direction")
            .map(|x| match x {
                Value::String(v) => v == "output",
                Value::Expression(v) => v == "output",
                _ => false,
            })
            .unwrap_or(false)
}

/// Check if a pin is an input pin
fn is_input_pin(pin: &Group) -> bool {
    (pin.type_ == "pin" || pin.type_ == "bundle")
        & pin
            .simple_attribute("direction")
            .map(|x| match x {
                Value::String(v) => v == "input",
                Value::Expression(v) => v == "input",
                _ => false,
            })
            .unwrap_or(false)
}

/// Check if a group owns timing arcs directly, rather than delegating them to
/// member `pin` subgroups.
fn owns_timing_arcs(group: &Group) -> bool {
    group.subgroups.iter().any(|g| g.type_ == "timing")
}

/// Collect the leaf pin groups of a cell matching a direction predicate.
///
/// Liberty allows two ways of writing a `bundle`. Either the bundle owns its
/// `timing` groups and the member `pin` subgroups carry only per-member trivia,
/// or the bundle is a plain container and each member `pin` holds its own arcs.
/// A cell-level `pin` is always a leaf; a `bundle` is a leaf only in the first
/// form. In the second, the members are the leaves and inherit the bundle's
/// `direction`, which they do not carry themselves — so the direction predicate
/// is applied to the bundle and never re-tested on a member.
pub(crate) fn timing_leaves(cell: &Group, direction: fn(&Group) -> bool) -> Vec<&Group> {
    cell.subgroups
        .iter()
        .filter(|g| direction(g))
        .flat_map(|g| {
            if g.type_ == "bundle" && !owns_timing_arcs(g) {
                g.subgroups.iter().filter(|s| s.type_ == "pin").collect()
            } else {
                vec![g]
            }
        })
        .collect()
}

/// Mutable counterpart of [`timing_leaves`]
pub(crate) fn timing_leaves_mut(
    cell: &mut Group,
    direction: fn(&Group) -> bool,
) -> Vec<&mut Group> {
    cell.subgroups
        .iter_mut()
        .filter(|g| direction(g))
        .flat_map(|g| {
            if g.type_ == "bundle" && !owns_timing_arcs(g) {
                g.subgroups
                    .iter_mut()
                    .filter(|s| s.type_ == "pin")
                    .collect()
            } else {
                vec![g]
            }
        })
        .collect()
}

/// Collect the input groups that setup/hold constraints should be written to.
///
/// Which name the constraints are keyed by is decided by the library, not by the
/// structure: the keys are the `related_pin` strings harvested from the output
/// arcs. A bundle may be named there directly, in which case it takes a single
/// shared constraint, or its members may be named individually, in which case
/// each member takes its own. `has_constraints` reports whether a name was
/// harvested, so only groups the library actually characterised are returned —
/// which also leaves the clock pin, and any input with no arc to an output,
/// untouched rather than carrying an empty constraint.
pub(crate) fn constraint_targets_mut<'a>(
    cell: &'a mut Group,
    has_constraints: &dyn Fn(&str) -> bool,
) -> Vec<&'a mut Group> {
    cell.subgroups
        .iter_mut()
        .filter(|g| is_input_pin(g))
        .flat_map(|g| {
            // Resolved before borrowing so a bundle and its members are never
            // both handed out.
            if g.type_ == "bundle" && !has_constraints(&g.name) {
                g.subgroups
                    .iter_mut()
                    .filter(|s| s.type_ == "pin" && has_constraints(&s.name))
                    .collect()
            } else if has_constraints(&g.name) {
                vec![g]
            } else {
                vec![]
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! Behaviour of the `pins` module: pin direction predicates, cell qualification
    //! and which pins constraints are written to.

    use super::*;
    use crate::arcs::WhenMerge;
    use crate::engine::{process_library, ReferenceMode};
    use liberty_parser::liberty::{Group, Liberty};
    use regex::Regex;

    fn sample_lib() -> Liberty {
        liberty_parser::parse_lib(
            r#"
library(test) {
  cell(LCELL) {
    latch(IQ, IQN) { enable: "G"; data_in: "D"; }
    pin(D) { direction: input; }
    pin(G) { direction: input; }
    pin(Q) { direction: output; function: "IQ"; }
  }
  cell(COMB) {
    pin(A) { direction: input; }
    pin(Y) { direction: output; function: "A"; }
  }
}
"#,
        )
        .expect("parse sample lib")
    }

    #[test]
    fn pin_direction_predicates() {
        let lib = sample_lib();
        let cell = lib[0].get_cell("LCELL").unwrap();
        assert!(is_output_pin(cell.get_pin("Q").unwrap()));
        assert!(!is_output_pin(cell.get_pin("D").unwrap()));
        assert!(is_input_pin(cell.get_pin("D").unwrap()));
        assert!(!is_input_pin(cell.get_pin("Q").unwrap()));
    }

    #[test]
    fn cell_qualifies_needs_a_latch_group_and_the_clock_pin() {
        let lib = sample_lib();
        let lcell = lib[0].get_cell("LCELL").unwrap();
        let comb = lib[0].get_cell("COMB").unwrap();
        assert!(cell_qualifies(lcell, "G"));
        assert!(!cell_qualifies(lcell, "CLK")); // no pin named CLK
        assert!(!cell_qualifies(comb, "G")); // no latch group
    }

    /// One cell per way of failing [`cell_qualifies`], each fully characterised.
    ///
    /// The timing tables matter as much as the disqualifying feature: a cell the
    /// engine accepts but cannot find a reference arc in is left byte-identical too,
    /// so without a complete arc on every output the comparison below could not tell
    /// a filtered cell from a processed one and would pass whatever the filter did.
    fn non_qualifying_lib() -> Liberty {
        liberty_parser::parse_lib(
            r#"
library(non_qualifying_test) {
  delay_model: table_lookup;

  lu_table_template(T) {
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }

  cell(NORMAL_AND) {
    area: 1.0;
    pin(A) { direction: input; }
    pin(B) { direction: input; }
    pin(Y) {
      direction: output;
      function: "A & B";
      timing() {
        related_pin: "A";
        cell_rise(T) { values("0.1, 0.2", "0.2, 0.3"); }
        cell_fall(T) { values("0.11, 0.21", "0.21, 0.31"); }
        rise_transition(T) { values("0.01, 0.02", "0.02, 0.03"); }
        fall_transition(T) { values("0.011, 0.021", "0.021, 0.031"); }
      }
    }
  }

  cell(LATCH_NO_CLOCK) {
    latch(IQ) { enable: "G"; data_in: "D"; }
    pin(D) { direction: input; }
    pin(Q) {
      direction: output;
      function: "IQ";
      timing() {
        related_pin: "D";
        cell_rise(T) { values("0.3, 0.4", "0.4, 0.5"); }
        cell_fall(T) { values("0.31, 0.41", "0.41, 0.51"); }
        rise_transition(T) { values("0.03, 0.04", "0.04, 0.05"); }
        fall_transition(T) { values("0.031, 0.041", "0.041, 0.051"); }
      }
    }
  }

  cell(NO_LATCH_WITH_CLOCK) {
    pin(CLK) { direction: input; clock: true; }
    pin(A) { direction: input; }
    pin(Y) {
      direction: output;
      function: "A";
      timing() {
        related_pin: "A";
        cell_rise(T) { values("0.5, 0.6", "0.6, 0.7"); }
        cell_fall(T) { values("0.51, 0.61", "0.61, 0.71"); }
        rise_transition(T) { values("0.05, 0.06", "0.06, 0.07"); }
        fall_transition(T) { values("0.051, 0.061", "0.061, 0.071"); }
      }
    }
  }
}
"#,
        )
        .expect("parse non-qualifying lib")
    }

    /// A cell the filter rejects is not touched at all -- not its pins, not its
    /// arcs, not its latch group. Whole-cell equality is the assertion because the
    /// claim is about everything the engine did not do.
    #[test]
    fn cells_the_filter_rejects_come_through_untouched() {
        let original = non_qualifying_lib();
        let mut processed = non_qualifying_lib();

        process_library(
            &mut processed[0],
            "CLK",
            &Regex::new(r"RST").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
        );

        for (cell_name, why) in [
            ("NORMAL_AND", "no latch group and no clock pin"),
            ("LATCH_NO_CLOCK", "a latch but no pin named CLK"),
            ("NO_LATCH_WITH_CLOCK", "the clock pin but no latch group"),
        ] {
            let before = original[0].get_cell(cell_name).expect(cell_name);
            let after = processed[0].get_cell(cell_name).expect(cell_name);
            assert_eq!(
                format!("{:?}", before),
                format!("{:?}", after),
                "{} has {} and must not be processed",
                cell_name,
                why
            );
        }
    }

    /// A setup or hold group with no constraint table is not usable timing, so an
    /// input either takes a populated pair or takes nothing.
    fn constraint_is_populated(pin: &Group, timing_type: &str) -> bool {
        pin.iter_subgroups_of_type("timing")
            .filter(|t| {
                t.simple_attribute("timing_type")
                    .map(|tt| tt.expr() == timing_type)
                    .unwrap_or(false)
            })
            .any(|t| {
                t.iter_subgroups_of_type("rise_constraint").next().is_some()
                    || t.iter_subgroups_of_type("fall_constraint").next().is_some()
            })
    }

    /// Every input the engine wrote anything to, in declaration order.
    fn constrained_inputs(cell: &Group) -> Vec<&str> {
        cell.iter_pins()
            .filter(|p| is_input_pin(p))
            .filter(|p| {
                p.simple_attribute("nextstate_type").is_some()
                    || p.iter_subgroups_of_type("timing").any(|t| {
                        t.simple_attribute("timing_type")
                            .map(|tt| tt.expr() == "setup_rising" || tt.expr() == "hold_rising")
                            .unwrap_or(false)
                    })
            })
            .map(|p| p.name.as_str())
            .collect()
    }

    /// Constraints reach exactly the inputs the library characterised against an
    /// output, however many other inputs the cell declares.
    ///
    /// The RACELEM family spells its data expression with a varying number of M and
    /// P control terms, and each term is a real pin -- declared, an input, and never
    /// characterised. Whichever variant is compiled, `A` is the only pin with an arc
    /// to `Q`, so the constrained set must stay exactly `{A}`: the clock has nothing
    /// to be constrained against, the reset is excluded, and a control pin picking
    /// up an empty setup group would be timing no tool can use.
    #[test]
    fn constraints_reach_only_the_inputs_characterised_against_an_output() {
        let variations: [(&[&str], &[&str], &str, &str); 6] = [
            (
                &["M1", "M2"],
                &["P1", "P2"],
                "A*IQ+A*P1*P2+IQ*M1+IQ*M2",
                "full_racelem",
            ),
            (&["M1"], &["P1", "P2"], "A*IQ+A*P1*P2+IQ*M1", "single_m"),
            (&["M1", "M2"], &["P1"], "A*IQ+A*P1+IQ*M1+IQ*M2", "single_p"),
            (&[], &["P1", "P2"], "A*IQ+A*P1*P2", "no_m"),
            (&["M1", "M2"], &[], "A*IQ+IQ*M1+IQ*M2", "no_p"),
            (
                &["M1", "M2", "M3"],
                &["P1"],
                "A*IQ+A*P1+IQ*M1+IQ*M2+IQ*M3",
                "three_m",
            ),
        ];
        let reset_name = Regex::new(r"(R|S)N?").unwrap();

        for (m_pins, p_pins, data_in, variation) in variations {
            let controls: String = m_pins
                .iter()
                .chain(p_pins.iter())
                .map(|pin| format!("pin({}) {{ direction: input; }}", pin))
                .collect::<Vec<_>>()
                .join("\n    ");

            let lib_str = format!(
                r#"
library({}_test) {{
  delay_model: table_lookup;

  lu_table_template(test_template) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}

  cell(RACELEM_VARIANT) {{
    latch(IQ,IQN) {{ clear: "!RN"; data_in: "{}"; enable: "G"; }}

    pin(G) {{ direction: input; clock: true; }}
    pin(RN) {{ direction: input; }}
    pin(A) {{ direction: input; }}
    {}
    pin(Q) {{
      direction: output;
      function: "IQ";
      timing() {{
        related_pin: "A";
        cell_rise(test_template) {{ values("0.2, 0.3", "0.3, 0.4"); }}
        cell_fall(test_template) {{ values("0.18, 0.28", "0.28, 0.38"); }}
        rise_transition(test_template) {{ values("0.04, 0.08", "0.08, 0.12"); }}
        fall_transition(test_template) {{ values("0.035, 0.075", "0.075, 0.115"); }}
      }}
    }}
  }}
}}
"#,
                variation, data_in, controls
            );

            let mut lib = liberty_parser::parse_lib(&lib_str)
                .unwrap_or_else(|_| panic!("parse {} variation", variation));
            process_library(
                &mut lib[0],
                "G",
                &reset_name,
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            );
            let cell = lib[0].get_cell("RACELEM_VARIANT").expect("RACELEM_VARIANT");

            assert_eq!(
                constrained_inputs(cell),
                vec!["A"],
                "variation {}",
                variation
            );

            // A drove an output, so what it took has to be usable: the marker plus a
            // populated pair, not an empty shell that would satisfy the set above.
            let a = cell.get_pin("A").expect("A");
            assert_eq!(
                a.simple_attribute("nextstate_type").unwrap().expr(),
                "data",
                "variation {}",
                variation
            );
            for timing_type in ["setup_rising", "hold_rising"] {
                assert!(
                    constraint_is_populated(a, timing_type),
                    "A needs a populated {} in variation {}",
                    timing_type,
                    variation
                );
            }
        }
    }
}
