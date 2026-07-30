//! Behaviour of the `pins` module: pin direction predicates, cell qualification
//! and which pins constraints are written to.

use super::*;

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
        process_library(&mut lib[0], "G", &reset_name, false);
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
