//! Construction of the Liberty groups the pseudo-flop model is written as.

use crate::arcs::RefArc;
use crate::conditions::Condition;
use crate::pins::LATCH_REGEX;
use indexmap::IndexMap;
use liberty_parser::{
    ast::Value,
    liberty::{Attribute, Group},
};
use ndarray::prelude::*;
use std::collections::HashSet;

/// What condition a timing group is emitted under.
///
/// One knob rather than an optional `when` beside an optional `sdf_cond`, because
/// those two must never be written apart: a `when` with no `sdf_cond` leaves the SDF
/// side of the same check unconditioned.
pub(crate) enum Guard<'a> {
    /// No condition at all. The group is the only one of its kind on the pin, which
    /// is what a single-reference mode emits.
    Unguarded,
    /// The group holds only in this state.
    Conditioned(&'a Condition),
    /// The group covers whatever the conditioned ones do not.
    CatchAll,
}

/// Write a guard onto a timing group's attributes.
///
/// Called last by every constructor below, so the guard's attributes are appended
/// after the group's own: an [`IndexMap`] keeps insertion order, so an unguarded
/// group is byte for byte the group it was before there were guards.
///
/// A `when` is never written without its `sdf_cond`, and both are rendered from the
/// one [`Condition`] -- the `when` as the library spelled it, the `sdf_cond` from the
/// expression that spelling parsed to -- so the pair states one condition by
/// construction rather than by two translations that could disagree.
fn apply_guard(attributes: &mut IndexMap<String, Vec<Attribute>>, guard: &Guard) {
    match guard {
        Guard::Unguarded => {}
        Guard::Conditioned(condition) => {
            attributes.insert(
                "when".to_owned(),
                vec![Attribute::Simple(Value::String(condition.as_written()))],
            );
            attributes.insert(
                "sdf_cond".to_owned(),
                vec![Attribute::Simple(Value::String(condition.sdf()))],
            );
        }
        Guard::CatchAll => {
            attributes.insert(
                "default_timing".to_owned(),
                vec![Attribute::Simple(Value::Expression("true".to_owned()))],
            );
        }
    }
}

/// Create a constraint table group (rise_constraint or fall_constraint)
fn create_constraint_table_group(
    constraint_type: &str,
    lut_template: &str,
    values: &Array1<f64>,
) -> Group {
    Group {
        type_: constraint_type.to_owned(),
        name: format!("{}_pseudo_constraint", lut_template),
        attributes: IndexMap::from([(
            "values".to_owned(),
            vec![Attribute::Complex(vec![Value::FloatGroup(
                values.iter().cloned().collect(),
            )])],
        )]),
        subgroups: vec![],
    }
}

/// Create a timing table group (cell_rise, cell_fall, rise_transition, fall_transition)
fn create_timing_table_group(table_type: &str, lut_template: &str, values: &Array1<f64>) -> Group {
    Group {
        type_: table_type.to_owned(),
        name: format!("{}_pseudo_delay", lut_template),
        attributes: IndexMap::from([(
            "values".to_owned(),
            vec![Attribute::Complex(vec![Value::FloatGroup(
                values.iter().cloned().collect(),
            )])],
        )]),
        subgroups: vec![],
    }
}

/// Create a setup timing group for an input pin
///
/// `rise_constraint` and `fall_constraint` are keyed on the CONSTRAINED pin's own
/// transition (RM p.336), so the two tables are named for the input's direction and
/// not for the output family the values were characterised in.
pub(crate) fn create_setup_timing_group(
    clock_name: &str,
    ref_arc: &RefArc,
    input_rise: Option<&Array1<f64>>,
    input_fall: Option<&Array1<f64>>,
    guard: &Guard,
) -> Group {
    let mut setup_values = Vec::with_capacity(2);

    if let Some(input_rise) = input_rise {
        setup_values.push(create_constraint_table_group(
            "rise_constraint",
            &ref_arc.lut_template,
            input_rise,
        ));
    }

    if let Some(input_fall) = input_fall {
        setup_values.push(create_constraint_table_group(
            "fall_constraint",
            &ref_arc.lut_template,
            input_fall,
        ));
    }

    let mut group = Group {
        type_: "timing".to_owned(),
        name: "".to_owned(),
        attributes: IndexMap::from([
            (
                "related_pin".to_owned(),
                vec![Attribute::Simple(Value::String(clock_name.to_owned()))],
            ),
            // `setup_rising` names the CLOCK's edge (RM p.332), not the constrained
            // pin's, and the fictitious clock this model refers everything to rises.
            // So the suffix is fixed however many conditions the checks multiply
            // into, and no `setup_falling` is ever emitted.
            (
                "timing_type".to_owned(),
                vec![Attribute::Simple(Value::Expression(
                    "setup_rising".to_owned(),
                ))],
            ),
        ]),
        subgroups: setup_values,
    };
    apply_guard(&mut group.attributes, guard);
    group
}

/// Create a hold timing group for an input pin
///
/// As with the setup group above, the two tables are keyed on the constrained pin's
/// own transition.
pub(crate) fn create_hold_timing_group(
    clock_name: &str,
    ref_arc: &RefArc,
    input_rise: Option<&Array1<f64>>,
    input_fall: Option<&Array1<f64>>,
    guard: &Guard,
) -> Group {
    let mut hold_values = Vec::with_capacity(2);

    if let Some(input_rise) = input_rise {
        hold_values.push(create_constraint_table_group(
            "rise_constraint",
            &ref_arc.lut_template,
            input_rise,
        ));
    }

    if let Some(input_fall) = input_fall {
        hold_values.push(create_constraint_table_group(
            "fall_constraint",
            &ref_arc.lut_template,
            input_fall,
        ));
    }

    let mut group = Group {
        type_: "timing".to_owned(),
        name: "".to_owned(),
        attributes: IndexMap::from([
            (
                "related_pin".to_owned(),
                vec![Attribute::Simple(Value::String(clock_name.to_owned()))],
            ),
            // As with the setup group above, the suffix names the clock's edge.
            (
                "timing_type".to_owned(),
                vec![Attribute::Simple(Value::Expression(
                    "hold_rising".to_owned(),
                ))],
            ),
        ]),
        subgroups: hold_values,
    };
    apply_guard(&mut group.attributes, guard);
    group
}

/// Create a pseudo-synchronous output timing arc
///
/// Each table family is emitted only where the reference carries the edge it names:
/// a state characterised in one direction alone has no delay to state for the other,
/// and inventing one would describe a transition nothing measured.
pub(crate) fn create_pseudo_output_timing_arc(
    clock_name: &str,
    output_transitions: &RefArc,
    mean_delays: &RefArc,
    guard: &Guard,
) -> Group {
    let table = |type_: &str, values: Option<&Array1<f64>>| {
        values.map(|values| create_timing_table_group(type_, &mean_delays.lut_template, values))
    };

    let mut group = Group {
        type_: "timing".to_owned(),
        name: "".to_owned(),
        attributes: IndexMap::from([
            (
                "related_pin".to_owned(),
                vec![Attribute::Simple(Value::String(clock_name.to_owned()))],
            ),
            (
                "timing_sense".to_owned(),
                vec![Attribute::Simple(Value::Expression("non_unate".to_owned()))],
            ),
            (
                "timing_type".to_owned(),
                vec![Attribute::Simple(Value::Expression(
                    "rising_edge".to_owned(),
                ))],
            ),
        ]),
        subgroups: [
            // Use mean_delays.lut_template for consistency, but output's own transition values
            table(
                "rise_transition",
                output_transitions.rise.as_ref().map(|e| &e.transition),
            ),
            table(
                "fall_transition",
                output_transitions.fall.as_ref().map(|e| &e.transition),
            ),
            table("cell_rise", mean_delays.rise.as_ref().map(|e| &e.delay)),
            table("cell_fall", mean_delays.fall.as_ref().map(|e| &e.delay)),
        ]
        .into_iter()
        .flatten()
        .collect(),
    };
    apply_guard(&mut group.attributes, guard);
    group
}

/// Convert latch groups to flip-flop groups
pub(crate) fn convert_latch_to_flipflop(cell: &mut Group) {
    for g in cell
        .iter_subgroups_mut()
        .filter(|g| LATCH_REGEX.is_match(&g.type_))
    {
        g.type_ = LATCH_REGEX.replace(&g.type_, "ff").into();

        if let Some(clock) = g.attributes.remove("enable") {
            g.attributes.insert("clocked_on".to_owned(), clock);
        }

        if let Some(vf) = g.attributes.remove("data_in") {
            g.attributes.insert("next_state".to_owned(), vf);
        }
    }
}

/// The axis attribute of a template the conversion has decided to derive from.
///
/// Only a template that passed `Templates::missing_axis` can become a used template,
/// so by the time this is reached both axes are present. It fails loudly rather than
/// indexing so that relaxing that guarantee later is reported as a defect in
/// pseudosync, instead of surfacing as a panic blamed on the input.
fn template_axis<'a>(template: &'a Group, axis: &str) -> &'a Vec<Attribute> {
    template.attributes.get(axis).unwrap_or_else(|| {
        panic!(
            "lookup template {} became a used template without {}",
            template.name, axis
        )
    })
}

/// Generate pseudo LUT templates for constraints and delays
pub(crate) fn generate_pseudo_lut_templates(
    lib: &Group,
    used_templates: &HashSet<String>,
) -> Vec<Group> {
    lib.iter_subgroups()
        .filter(|g| g.type_ == "lu_table_template" && used_templates.contains(&g.name))
        .flat_map(|g| {
            vec![
                Group {
                    type_: "lu_table_template".to_owned(),
                    name: format!("{}_pseudo_constraint", g.name),
                    attributes: IndexMap::from([
                        (
                            "variable_1".to_owned(),
                            vec![Attribute::Simple(Value::Expression(
                                "constrained_pin_transition".to_owned(),
                            ))],
                        ),
                        ("index_1".to_owned(), template_axis(g, "index_1").clone()),
                    ]),
                    subgroups: vec![],
                },
                Group {
                    type_: "lu_table_template".to_owned(),
                    name: format!("{}_pseudo_delay", g.name),
                    attributes: IndexMap::from([
                        (
                            "variable_1".to_owned(),
                            vec![Attribute::Simple(Value::Expression(
                                "total_output_net_capacitance".to_owned(),
                            ))],
                        ),
                        ("index_1".to_owned(), template_axis(g, "index_2").clone()),
                    ]),
                    subgroups: vec![],
                },
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! Behaviour of the `emit` module: pseudo LUT templates and latch-to-flip-flop conversion.

    use super::*;
    use crate::arcs::EdgeRef;
    use crate::arcs::{Anchor, ReferenceMode, WhenMerge};
    use crate::engine::{process_library, CellOptions}; // Test-only; a unit test observes its subject through the real engine path rather than a stub.
    use indexmap::IndexMap;
    use liberty_parser::{
        ast::Value,
        liberty::{Attribute, Group, Liberty},
    };
    use regex::Regex;
    use std::collections::HashSet;

    /// The conversion knobs, with the anchor at the default the command line
    /// supplies. The anchor is exercised where it is decided, in `arcs`; here it
    /// only has to stay out of the way.
    fn opts<'a>(
        clock_name: &'a str,
        reset_name: &'a Regex,
        latch: bool,
        mode: ReferenceMode,
        when_merge: WhenMerge,
    ) -> CellOptions<'a> {
        CellOptions {
            clock_name,
            reset_name,
            latch,
            mode,
            when_merge,
            anchor: Anchor::Middle,
        }
    }

    fn lut_template(name: &str, index_1: [f64; 2], index_2: [f64; 2]) -> Group {
        // A minimal lu_table_template carrying index_1 and index_2, the two
        // attributes generate_pseudo_lut_templates clones.
        let index = |values: [f64; 2]| {
            vec![Attribute::Complex(
                values.into_iter().map(Value::Float).collect(),
            )]
        };
        Group {
            type_: "lu_table_template".to_owned(),
            name: name.to_owned(),
            attributes: IndexMap::from([
                ("index_1".to_owned(), index(index_1)),
                ("index_2".to_owned(), index(index_2)),
            ]),
            subgroups: vec![],
        }
    }

    fn simple_expr(value: &str) -> Vec<Attribute> {
        vec![Attribute::Simple(Value::Expression(value.to_owned()))]
    }

    fn simple_string(value: &str) -> Vec<Attribute> {
        vec![Attribute::Simple(Value::String(value.to_owned()))]
    }

    // --- generate_pseudo_lut_templates ------------------------------------

    /// Killed by: `generate_pseudo_lut_templates` indexed the derived delay template from `index_1` instead of `index_2`.
    #[test]
    fn generate_pseudo_lut_templates_emits_constraint_and_delay_pair() {
        let lib = Group {
            type_: "library".to_owned(),
            name: "L".to_owned(),
            attributes: IndexMap::new(),
            subgroups: vec![
                // Two used templates with pairwise disjoint indices, so a derived
                // template that read the wrong source is visible rather than
                // accidentally right.
                lut_template("delay_template_3x3", [0.1, 0.2], [1.0, 2.0]),
                lut_template("delay_template_7x7", [0.3, 0.4], [3.0, 4.0]),
                lut_template("unused_template", [0.5, 0.6], [5.0, 6.0]),
                // a non-template subgroup must be ignored
                Group {
                    type_: "cell".to_owned(),
                    name: "C".to_owned(),
                    attributes: IndexMap::new(),
                    subgroups: vec![],
                },
            ],
        };
        let used: HashSet<String> = [
            "delay_template_3x3".to_owned(),
            "delay_template_7x7".to_owned(),
        ]
        .into_iter()
        .collect();

        let out = generate_pseudo_lut_templates(&lib, &used);

        // Each *used* template expands into exactly two derived templates, in the
        // order the library declares its sources; the unused one yields nothing.
        let names: Vec<&str> = out.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "delay_template_3x3_pseudo_constraint",
                "delay_template_3x3_pseudo_delay",
                "delay_template_7x7_pseudo_constraint",
                "delay_template_7x7_pseudo_delay",
            ]
        );

        // The two halves of a pair take their index from opposite sides of their own
        // source: the constraint is indexed by slew (index_1), the delay by load
        // (index_2).
        for (source, constraint, delay) in [(0usize, 0usize, 1usize), (1, 2, 3)] {
            let source = &lib.subgroups[source];

            assert_eq!(
                out[constraint].attributes["variable_1"],
                simple_expr("constrained_pin_transition")
            );
            assert_eq!(
                out[constraint].attributes["index_1"], source.attributes["index_1"],
                "{}",
                out[constraint].name
            );

            assert_eq!(
                out[delay].attributes["variable_1"],
                simple_expr("total_output_net_capacitance")
            );
            assert_eq!(
                out[delay].attributes["index_1"], source.attributes["index_2"],
                "{}",
                out[delay].name
            );
        }
    }

    fn sample_lib() -> Liberty {
        liberty_parser::parse_lib(
            r#"
library(test) {
  cell(LCELL) {
    latch(IQ, IQN) { clear: "!RN"; enable: "G"; data_in: "D"; }
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

    /// Killed by: `convert_latch_to_flipflop` removed the key `"enable_not_a_key"`, leaving `enable` unrenamed.
    #[test]
    fn convert_latch_to_flipflop_renames_group_and_attributes() {
        let mut lib = sample_lib();
        let cell = lib[0].get_cell_mut("LCELL").unwrap();

        convert_latch_to_flipflop(cell);

        assert_eq!(cell.iter_subgroups_of_type("latch").count(), 0);
        let g = cell
            .iter_subgroups()
            .find(|g| g.type_ == "ff")
            .expect("latch became ff");

        // The state variables name the group, and the retype leaves them alone.
        assert_eq!(g.name, "IQ, IQN");
        // enable -> clocked_on and data_in -> next_state, each keeping its own
        // expression. Only the values catch a crossed pair, since both keys are
        // present either way.
        assert_eq!(g.simple_attribute("clocked_on").unwrap().string(), "G");
        assert_eq!(g.simple_attribute("next_state").unwrap().string(), "D");
        assert!(g.simple_attribute("enable").is_none());
        assert!(g.simple_attribute("data_in").is_none());
        // Attributes a flip-flop spells the same way as a latch are carried across
        // untouched.
        assert_eq!(g.simple_attribute("clear").unwrap().string(), "!RN");
    }

    // --- timing table and timing arc emission -----------------------------

    /// A reference arc whose four tables hold pairwise distinct values, so an
    /// emitted table can be traced back to the exact field it was read from.
    fn ref_arc(lut_template: &str, tables: [[f64; 2]; 4]) -> RefArc {
        RefArc {
            col: 0,
            row: 0,
            related_pin: "CK".to_owned(),
            lut_template: lut_template.to_owned(),
            anchor: Anchor::Middle,
            rise: Some(EdgeRef {
                delay: Array1::from(tables[2].to_vec()),
                transition: Array1::from(tables[0].to_vec()),
                crossing: tables[2][0],
            }),
            fall: Some(EdgeRef {
                delay: Array1::from(tables[3].to_vec()),
                transition: Array1::from(tables[1].to_vec()),
                crossing: tables[3][0],
            }),
        }
    }

    /// The same reference with one edge removed, for the single-edge assertions.
    fn one_edge(mut arc: RefArc, edge: &str) -> RefArc {
        match edge {
            "rise" => arc.fall = None,
            _ => arc.rise = None,
        }
        arc
    }

    /// The text of one simple attribute, or `None` where the group carries none.
    ///
    /// Read structurally rather than through `Value::string`, which panics on a value
    /// spelled as a bare expression -- and a `default_timing` is one.
    fn attribute(group: &Group, name: &str) -> Option<String> {
        group.simple_attribute(name).map(|v| match v {
            Value::Expression(text) | Value::String(text) => text.clone(),
            other => panic!("attribute {} is not a text value: {:?}", name, other),
        })
    }

    /// The attribute keys of a group, in the order they were inserted.
    fn attribute_order(group: &Group) -> Vec<&str> {
        group.attributes.keys().map(String::as_str).collect()
    }

    /// The numbers a table group carries, which must be exactly one `FloatGroup`.
    fn table_values(group: &Group) -> Vec<f64> {
        match group
            .complex_attribute("values")
            .expect("table group carries a values attribute")
            .as_slice()
        {
            [Value::FloatGroup(values)] => values.clone(),
            other => panic!("expected exactly one FloatGroup, got {:?}", other),
        }
    }

    /// The pseudo-flop output arc draws on two different arcs, and which number
    /// comes from which one is the whole model: the transitions are the output's
    /// own, the clock-to-output delays are the reference's. Substituting a cell-wide
    /// pooled mean for those delays is a defect no shape or count can catch, which is why
    /// the two sources here are given deliberately disjoint value ranges -- 1.x against
    /// 9.x -- that no swap between them can survive.
    ///
    /// Killed by: `create_pseudo_output_timing_arc` emitted `mean_delays.rise.transition` as the rise_transition table instead of `output_transitions.rise.transition`.
    #[test]
    fn pseudo_output_arc_takes_transitions_from_the_output_and_delays_from_the_reference() {
        let transitions = ref_arc("tplA", [[1.1, 1.2], [1.3, 1.4], [1.5, 1.6], [1.7, 1.8]]);
        let delays = ref_arc("tplB", [[9.1, 9.2], [9.3, 9.4], [9.5, 9.6], [9.7, 9.8]]);

        let group = create_pseudo_output_timing_arc("G", &transitions, &delays, &Guard::Unguarded);

        assert_eq!(group.type_, "timing");
        assert_eq!(group.attributes["related_pin"], simple_string("G"));
        assert_eq!(group.attributes["timing_sense"], simple_expr("non_unate"));
        assert_eq!(group.attributes["timing_type"], simple_expr("rising_edge"));

        // Exactly four tables, in this order.
        let types: Vec<&str> = group.subgroups.iter().map(|g| g.type_.as_str()).collect();
        assert_eq!(
            types,
            vec![
                "rise_transition",
                "fall_transition",
                "cell_rise",
                "cell_fall"
            ]
        );

        let edge = |arc: &RefArc, rise: bool| {
            let edge = if rise { &arc.rise } else { &arc.fall };
            edge.as_ref().expect("the fixture draws both edges").clone()
        };
        // The transitions are the output's own ...
        assert_eq!(
            table_values(&group.subgroups[0]),
            edge(&transitions, true).transition.to_vec()
        );
        assert_eq!(
            table_values(&group.subgroups[1]),
            edge(&transitions, false).transition.to_vec()
        );
        // ... while the delays are the reference's. Swapping the two is the defect.
        assert_eq!(
            table_values(&group.subgroups[2]),
            edge(&delays, true).delay.to_vec()
        );
        assert_eq!(
            table_values(&group.subgroups[3]),
            edge(&delays, false).delay.to_vec()
        );

        // Every table indexes the reference's template, not the output's.
        let expected_name = format!("{}_pseudo_delay", delays.lut_template);
        assert_eq!(expected_name, "tplB_pseudo_delay");
        for table in &group.subgroups {
            assert_eq!(table.name, expected_name, "{}", table.type_);
        }
    }

    /// Two outputs whose four tables are pairwise disjoint, so an emitted table can
    /// be traced back to the output it belongs to. Row 1 -- the second line of each
    /// table -- is what `select_reference_arc` samples from a 2x2 table.
    fn dual_output_latch_lib() -> Liberty {
        liberty_parser::parse_lib(
            r#"
library(pseudo_arc_test) {
  lu_table_template(T) {
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }
  cell(DUT) {
    latch(IQ, IQN) { enable: "G"; data_in: "D"; }
    pin(G) { direction: input; clock: true; }
    pin(D) { direction: input; }
    pin(Q) {
      direction: output;
      function: "IQ";
      timing() {
        related_pin: "D";
        timing_sense : positive_unate;
        cell_rise(T) { values("1.0, 2.0", "3.0, 4.0"); }
        cell_fall(T) { values("5.0, 6.0", "7.0, 8.0"); }
        rise_transition(T) { values("0.1, 0.2", "0.3, 0.4"); }
        fall_transition(T) { values("0.5, 0.6", "0.7, 0.8"); }
      }
    }
    pin(QN) {
      direction: output;
      function: "IQN";
      timing() {
        related_pin: "D";
        timing_sense : positive_unate;
        cell_rise(T) { values("10.0, 20.0", "30.0, 40.0"); }
        cell_fall(T) { values("50.0, 60.0", "70.0, 80.0"); }
        rise_transition(T) { values("1.1, 1.2", "1.3, 1.4"); }
        fall_transition(T) { values("1.5, 1.6", "1.7, 1.8"); }
      }
    }
  }
}
"#,
        )
        .expect("parse dual-output fixture")
    }

    /// The constructor is exercised directly above; this is the same arc seen where
    /// it has to arrive, on the pin, after the whole engine has run.
    ///
    /// Only the engine decides *which* reference an output is handed, so handing every
    /// output the cell-wide mean is invisible to the constructor and visible only here. The
    /// two outputs are characterised an order of magnitude apart, which no cell-wide mean
    /// can land on.
    ///
    /// Killed by: `process_cell`'s phase 3 handed `PerOutput` the cell-wide `&mean_ref_arc` as its delays, pooling what should be each output's own.
    #[test]
    fn each_output_pin_gains_a_clock_arc_built_from_its_own_reference() {
        let mut lib = dual_output_latch_lib();

        process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );

        let cell = lib[0].get_cell("DUT").expect("DUT");
        // Row 1 of each of the pin's own tables, in the order the arc emits them.
        let expected = [
            ("Q", [[0.3, 0.4], [0.7, 0.8], [3.0, 4.0], [7.0, 8.0]]),
            ("QN", [[1.3, 1.4], [1.7, 1.8], [30.0, 40.0], [70.0, 80.0]]),
        ];

        for (pin_name, tables) in expected {
            let pin = cell
                .get_pin(pin_name)
                .unwrap_or_else(|| panic!("pin {} not found", pin_name));

            // In flip-flop mode the original input arc is erased, so the clock arc
            // is the only timing group the pin is left with.
            let arcs: Vec<&Group> = pin.iter_subgroups_of_type("timing").collect();
            assert_eq!(arcs.len(), 1, "{} timing groups", pin_name);
            let arc = arcs[0];

            assert_eq!(
                arc.attributes["related_pin"],
                simple_string("G"),
                "{}",
                pin_name
            );
            assert_eq!(
                arc.attributes["timing_sense"],
                simple_expr("non_unate"),
                "{}",
                pin_name
            );
            assert_eq!(
                arc.attributes["timing_type"],
                simple_expr("rising_edge"),
                "{}",
                pin_name
            );

            let types: Vec<&str> = arc.subgroups.iter().map(|g| g.type_.as_str()).collect();
            assert_eq!(
                types,
                vec![
                    "rise_transition",
                    "fall_transition",
                    "cell_rise",
                    "cell_fall"
                ],
                "{}",
                pin_name
            );

            for (table, values) in arc.subgroups.iter().zip(tables) {
                assert_eq!(table.name, "T_pseudo_delay", "{} {}", pin_name, table.type_);
                assert_eq!(
                    table_values(table),
                    values.to_vec(),
                    "{} {}",
                    pin_name,
                    table.type_
                );
            }
        }
    }

    /// Killed by: `create_constraint_table_group` named the table `{}_pseudo_cnstrnt`.
    #[test]
    fn create_constraint_table_group_names_the_table_pseudo_constraint() {
        let values = Array1::from(vec![2.5, 3.5, 4.5]);

        let group = create_constraint_table_group("rise_constraint", "tplC", &values);

        assert_eq!(group.type_, "rise_constraint");
        assert_eq!(group.name, "tplC_pseudo_constraint");
        assert_eq!(table_values(&group), vec![2.5, 3.5, 4.5]);
        assert!(group.subgroups.is_empty());
    }

    /// Killed by: `create_timing_table_group` named the table `{}_pseudo_dly`.
    #[test]
    fn create_timing_table_group_names_the_table_pseudo_delay_and_keeps_value_order() {
        let values = Array1::from(vec![7.25, 8.5, 9.75]);

        let group = create_timing_table_group("cell_rise", "tplD", &values);

        assert_eq!(group.type_, "cell_rise");
        assert_eq!(group.name, "tplD_pseudo_delay");
        // Order is load-bearing: a LUT value is located by its position alone.
        assert_eq!(table_values(&group), vec![7.25, 8.5, 9.75]);
        assert!(group.subgroups.is_empty());
    }

    /// Killed by: `create_setup_timing_group` declared `timing_type: setup_falling`.
    #[test]
    fn create_setup_timing_group_targets_the_clock_with_setup_rising() {
        let arc = ref_arc("tplE", [[0.0, 0.0]; 4]);

        let group = create_setup_timing_group("CK", &arc, None, None, &Guard::Unguarded);

        assert_eq!(group.type_, "timing");
        assert_eq!(group.attributes["related_pin"], simple_string("CK"));
        assert_eq!(group.attributes["timing_type"], simple_expr("setup_rising"));
    }

    /// Killed by: `create_setup_timing_group` emitted the rise setup as a `fall_constraint`.
    #[test]
    fn create_setup_timing_group_emits_a_constraint_table_only_for_each_side_given() {
        let arc = ref_arc("tplE", [[0.0, 0.0]; 4]);
        let rise = Array1::from(vec![1.0, 2.0]);
        let fall = Array1::from(vec![3.0, 4.0]);
        let types =
            |g: &Group| -> Vec<String> { g.subgroups.iter().map(|s| s.type_.clone()).collect() };

        let rise_only = create_setup_timing_group("CK", &arc, Some(&rise), None, &Guard::Unguarded);
        assert_eq!(types(&rise_only), vec!["rise_constraint"]);
        assert_eq!(table_values(&rise_only.subgroups[0]), vec![1.0, 2.0]);

        let fall_only = create_setup_timing_group("CK", &arc, None, Some(&fall), &Guard::Unguarded);
        assert_eq!(types(&fall_only), vec!["fall_constraint"]);
        assert_eq!(table_values(&fall_only.subgroups[0]), vec![3.0, 4.0]);

        // Both sides present: rise leads, so the emitted order stays stable.
        let both =
            create_setup_timing_group("CK", &arc, Some(&rise), Some(&fall), &Guard::Unguarded);
        assert_eq!(types(&both), vec!["rise_constraint", "fall_constraint"]);
        assert_eq!(table_values(&both.subgroups[0]), vec![1.0, 2.0]);
        assert_eq!(table_values(&both.subgroups[1]), vec![3.0, 4.0]);

        let neither = create_setup_timing_group("CK", &arc, None, None, &Guard::Unguarded);
        assert!(neither.subgroups.is_empty());
    }

    /// Killed by: `create_hold_timing_group` declared `timing_type: hold_falling`.
    #[test]
    fn create_hold_timing_group_targets_the_clock_with_hold_rising() {
        let arc = ref_arc("tplE", [[0.0, 0.0]; 4]);

        let group = create_hold_timing_group("CK", &arc, None, None, &Guard::Unguarded);

        assert_eq!(group.type_, "timing");
        assert_eq!(group.attributes["related_pin"], simple_string("CK"));
        assert_eq!(group.attributes["timing_type"], simple_expr("hold_rising"));
    }

    /// Killed by: `create_hold_timing_group` emitted the rise hold as a `fall_constraint`.
    #[test]
    fn create_hold_timing_group_emits_a_constraint_table_only_for_each_side_given() {
        let arc = ref_arc("tplE", [[0.0, 0.0]; 4]);
        let rise = Array1::from(vec![-1.0, -2.0]);
        let fall = Array1::from(vec![-3.0, -4.0]);
        let types =
            |g: &Group| -> Vec<String> { g.subgroups.iter().map(|s| s.type_.clone()).collect() };

        let rise_only = create_hold_timing_group("CK", &arc, Some(&rise), None, &Guard::Unguarded);
        assert_eq!(types(&rise_only), vec!["rise_constraint"]);
        assert_eq!(table_values(&rise_only.subgroups[0]), vec![-1.0, -2.0]);

        let fall_only = create_hold_timing_group("CK", &arc, None, Some(&fall), &Guard::Unguarded);
        assert_eq!(types(&fall_only), vec!["fall_constraint"]);
        assert_eq!(table_values(&fall_only.subgroups[0]), vec![-3.0, -4.0]);

        // Both sides present: rise leads, so the emitted order stays stable.
        let both =
            create_hold_timing_group("CK", &arc, Some(&rise), Some(&fall), &Guard::Unguarded);
        assert_eq!(types(&both), vec!["rise_constraint", "fall_constraint"]);
        assert_eq!(table_values(&both.subgroups[0]), vec![-1.0, -2.0]);
        assert_eq!(table_values(&both.subgroups[1]), vec![-3.0, -4.0]);

        let neither = create_hold_timing_group("CK", &arc, None, None, &Guard::Unguarded);
        assert!(neither.subgroups.is_empty());
    }

    // --- guards -------------------------------------------------------------

    /// A conditioned group states its condition twice and in two languages, and the
    /// two must always be written together.
    ///
    /// `when` is what Liberty conditions the group on and `sdf_cond` is what the SDF
    /// annotation carries; a group with one and not the other conditions the delay
    /// calculator and leaves the annotator unconditioned, or the reverse. Both are
    /// rendered from the ONE `Condition`, so the pair cannot say two different
    /// things: the `when` is the source spelling verbatim and the `sdf_cond` is the
    /// expression that spelling parsed to.
    ///
    /// Killed by: `apply_guard` wrote the `when` as `condition.liberty()` rather than as `condition.as_written()`, which normalised the source spelling to `!M * P`. Observed to redden this test alone: it is the only one whose source spelling differs from the rendering, which is exactly the difference it is here to catch. (Dropping the `sdf_cond` insert altogether reddens this and six per-state neighbours, because `guard_of` refuses a group stated by halves anywhere.)
    #[test]
    fn a_conditioned_group_carries_its_when_and_its_sdf_cond_together() {
        // A source spelling the renderer would not produce, so a `when` taken from
        // the rendering rather than from the library is visible.
        let condition = Condition::parse("(!M)*(P)").expect("a parenthesised conjunction");
        let arc = ref_arc("tplF", [[0.0, 0.0]; 4]);

        for group in [
            create_setup_timing_group("CK", &arc, None, None, &Guard::Conditioned(&condition)),
            create_hold_timing_group("CK", &arc, None, None, &Guard::Conditioned(&condition)),
        ] {
            // The library's own text, byte for byte.
            assert_eq!(attribute(&group, "when").as_deref(), Some("(!M)*(P)"));
            // ... and the same condition as SDF states it: M low and P high.
            assert_eq!(
                attribute(&group, "sdf_cond").as_deref(),
                Some("M == 1'B0 && P == 1'B1")
            );
            assert_eq!(attribute(&group, "default_timing"), None);
        }
    }

    /// The catch-all states no condition at all: it is the group Liberty falls back
    /// to when no conditioned one applies, which `default_timing` is what says.
    ///
    /// Killed by: `apply_guard`'s `CatchAll` arm inserted a `when` of `"1"` beside the `default_timing`, which is a conditioned group claiming to hold always rather than a fallback. That also reddens the two engine tests whose fixtures carry a catch-all; `an_unguarded_group_gains_nothing_and_keeps_its_attribute_order` stays green under it, which is the sibling that separates the catch-all arm from the unguarded one.
    #[test]
    fn a_catch_all_group_states_a_default_and_no_condition() {
        let arc = ref_arc("tplF", [[0.0, 0.0]; 4]);
        let group = create_setup_timing_group("CK", &arc, None, None, &Guard::CatchAll);

        assert_eq!(attribute(&group, "default_timing").as_deref(), Some("true"));
        assert_eq!(attribute(&group, "when"), None);
        assert_eq!(attribute(&group, "sdf_cond"), None);
    }

    /// An unguarded group is the group it was before there were guards: the same
    /// attributes, in the same order, and nothing appended.
    ///
    /// Order is what this pins beyond the absence: the guard's attributes are
    /// appended last, so a mode that emits one unguarded group per pin emits the
    /// bytes it always did.
    ///
    /// Killed by: `apply_guard` inserted `default_timing` for `Guard::Unguarded` as well as for `Guard::CatchAll`, which appended an attribute to every group the default mode emits.
    #[test]
    fn an_unguarded_group_gains_nothing_and_keeps_its_attribute_order() {
        let arc = ref_arc("tplF", [[0.0, 0.0]; 4]);

        assert_eq!(
            attribute_order(&create_setup_timing_group(
                "CK",
                &arc,
                None,
                None,
                &Guard::Unguarded
            )),
            vec!["related_pin", "timing_type"]
        );
        assert_eq!(
            attribute_order(&create_pseudo_output_timing_arc(
                "G",
                &arc,
                &arc,
                &Guard::Unguarded
            )),
            vec!["related_pin", "timing_sense", "timing_type"]
        );

        // A guard appends; it never reorders or replaces what was there.
        let condition = Condition::parse("M").expect("a pin name is a condition");
        assert_eq!(
            attribute_order(&create_pseudo_output_timing_arc(
                "G",
                &arc,
                &arc,
                &Guard::Conditioned(&condition)
            )),
            vec![
                "related_pin",
                "timing_sense",
                "timing_type",
                "when",
                "sdf_cond"
            ]
        );
    }

    /// A state characterised in one direction alone emits the two tables of that
    /// direction and no others.
    ///
    /// The four tables are two pairs, and each pair belongs to one output edge: a
    /// `combinational_rise` group carries the rise families alone, and inventing the
    /// fall ones would state a delay and a slew for a transition nothing measured.
    ///
    /// Killed by: `create_pseudo_output_timing_arc` fell back to the other edge's tables where one was absent -- `mean_delays.rise.as_ref().or(mean_delays.fall.as_ref())` for the `cell_rise` table -- so a fall-only state emitted a `cell_rise` copied from its fall delay. That also reddens `per_state_emits_one_conditioned_clock_arc_per_state_and_a_catch_all_last`, which asks the same question of the whole engine path; every other emit test hands both edges, where the fallback never fires.
    #[test]
    fn a_single_edge_reference_emits_that_edge_alone() {
        let arc = ref_arc("tplG", [[1.1, 1.2], [1.3, 1.4], [1.5, 1.6], [1.7, 1.8]]);

        let rise = one_edge(arc.clone(), "rise");
        let group = create_pseudo_output_timing_arc("G", &rise, &rise, &Guard::Unguarded);
        let types: Vec<&str> = group.subgroups.iter().map(|g| g.type_.as_str()).collect();
        assert_eq!(types, vec!["rise_transition", "cell_rise"]);
        assert_eq!(table_values(&group.subgroups[0]), vec![1.1, 1.2]);
        assert_eq!(table_values(&group.subgroups[1]), vec![1.5, 1.6]);

        let fall = one_edge(arc, "fall");
        let group = create_pseudo_output_timing_arc("G", &fall, &fall, &Guard::Unguarded);
        let types: Vec<&str> = group.subgroups.iter().map(|g| g.type_.as_str()).collect();
        assert_eq!(types, vec!["fall_transition", "cell_fall"]);
        assert_eq!(table_values(&group.subgroups[0]), vec![1.3, 1.4]);
        assert_eq!(table_values(&group.subgroups[1]), vec![1.7, 1.8]);
    }
}
