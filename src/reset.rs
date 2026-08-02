//! How the flop model states a retained asynchronous reset arc.
//!
//! A converted output carries a synthesised `rising_edge` arc against the phantom
//! clock. The reset arcs kept beside it arrive from the input library under the
//! combinational family of timing types, and an output mixing a sequential arc with
//! combinational ones is not a model a synthesiser can hold -- so each retained arc
//! is restated under the asynchronous type that says the same thing sequentially.
//!
//! Liberty's two asynchronous output types are named for the direction each drives
//! the output: `clear` drives it low, `preset` drives it high. So the type follows
//! the arrival direction the arc's own tables state, and nothing else:
//!
//! * a fall family -- `cell_fall`, `fall_transition` -- becomes `clear`;
//! * a rise family -- `cell_rise`, `rise_transition` -- becomes `preset`;
//! * an arc measuring both becomes two arcs, one of each.
//!
//! Why the tables and not the input edge. [`crate::arcs::input_transition`] derives
//! the direction the INPUT was moving in, and that is a different question with a
//! different answer: it says whether the reset pin rose or fell, not which way this
//! output went. Nothing in the tree carries a concept of reset polarity, so an input
//! edge cannot be separated into an assertion and a deactivation anyway -- and the
//! tag does not need it. `timing_type` states what the output does; the tables state
//! what the output did; the two are asked and answered on the same side.
//!
//! The consequence is accepted deliberately: on an active-low reset the rise family
//! is the DEACTIVATION, and it is still tagged `preset`. The repository's own public
//! example is the evidence that the library itself works this way.
//! `examples/ASCEND_FREEPDK45_ALHO_nom_1.10V_25C.lib:29190-29196` states
//! `timing_type : clear` over a `cell_fall`, and :29851-29857 states
//! `timing_type : preset` over a `cell_rise` -- on one output pin, from the same
//! `related_pin : "RN"`, under the same `timing_sense : positive_unate`, the same
//! `when` and the same `sdf_cond`. Neither the sense nor the condition can be what
//! separates those two arcs, because they are identical across both; the table family
//! is the only thing that differs. The same cell's `latch_bank` at :36339-36340
//! declares `clear : "!RN"` and no `preset` at all, which is why nothing here reads
//! the sequential group: that group is a recognition label and cannot describe a
//! dual-rail cell whose reset clears one rail and presets the other.

use crate::arcs::{edge_families, Transition};
use liberty_parser::{
    ast::Value,
    liberty::{Attribute, Group},
};

/// The asynchronous output type naming the direction `edge` drives the output in.
///
/// Liberty's two asynchronous output types are named for what each does to the
/// output -- `preset` drives it high, `clear` drives it low -- so the type follows
/// the arrival direction the arc's own tables state.
fn async_timing_type(edge: Transition) -> &'static str {
    match edge {
        Transition::Rise => "preset",
        Transition::Fall => "clear",
    }
}

/// An arc with no delay and no transition table in either direction: nothing it
/// carries says which way the output arrived, so no type can be chosen for it.
const NO_TABLE: &str = "carries no delay or transition table, so no output direction states it";

/// The whole arc, retagged with the asynchronous type `edge` names.
///
/// Every subgroup and every other attribute survives by clone. `IndexMap::insert`
/// keeps an existing key where it already sat, so the retagged group is the original
/// one with a single value replaced rather than reordered.
fn retagged(arc: &Group, edge: Transition) -> Group {
    let mut restated = arc.clone();
    restated.attributes.insert(
        "timing_type".to_owned(),
        vec![Attribute::Simple(Value::Expression(
            async_timing_type(edge).to_owned(),
        ))],
    );
    restated
}

/// One half of an arc that measured both directions: the tables of `edge` under
/// `edge`'s asynchronous type, and nothing else.
///
/// The half is built from what it needs rather than by shedding what it does not
/// want. It names one arrival, so it carries that arrival's two families and every
/// other subgroup is discarded -- which is what makes it unable to acquire data
/// describing the direction it does not name, WHATEVER the input arc contained. A
/// list of what to drop can only name the shapes whoever wrote it had met, so an
/// unfamiliar subgroup would ride onto both halves unexamined; a list of what to keep
/// has no such gap. The attributes are untouched: `related_pin`, `timing_sense`,
/// `when`, `sdf_cond` and the group name come through [`retagged`] by clone.
///
/// `a_subgroup_of_neither_family_is_discarded_from_both_halves` pins the discard, and
/// `a_split_half_keeps_the_condition_and_sense_the_library_wrote` the attributes that
/// survive it.
fn half(arc: &Group, edge: Transition) -> Group {
    let keep = edge_families(edge);
    let mut restated = retagged(arc, edge);
    restated
        .subgroups
        .retain(|g| keep.contains(&g.type_.as_str()));
    restated
}

/// Restate one retained reset arc under the asynchronous timing type its own tables
/// name, or say why it cannot be stated.
///
/// One arc in, one or two arcs out. An arc measuring both arrivals becomes two,
/// because a `timing` group states exactly one `timing_type`; THE ORDER OF THE TWO
/// IS NOT A PROPERTY of the result.
///
/// `related_pin`, `timing_sense`, `when`, `sdf_cond`, the group name and every table
/// value pass through by clone and are never rewritten. Nothing here consumes a
/// `timing_sense` and nothing writes one, which is precisely what makes the polarity
/// of the reset irrelevant to the answer; nor is the cell's `ff`/`ff_bank`/`latch`/
/// `latch_bank` group ever read.
///
/// Idempotent: an arc that already states `clear` or `preset` is returned verbatim.
///
/// `an_arc_carrying_both_edges_becomes_a_preset_and_a_clear` pins the split, asserting
/// the two as a set because the order is not a property of the result;
/// `a_single_edge_arc_keeps_every_subgroup_it_had` pins that a one-arrival arc is
/// retagged rather than rebuilt; and
/// `an_arc_already_stating_an_asynchronous_type_is_returned_verbatim` the idempotence.
/// `flop_mode_states_a_combinational_reset_arc_asynchronously` and
/// `latch_mode_leaves_a_combinational_reset_arc_as_the_library_wrote_it` in
/// `src/engine.rs` pin both modes through a whole conversion.
pub(crate) fn restate_reset_arc(arc: &Group) -> Result<Vec<Group>, String> {
    let present = |edge: Transition| {
        let families = edge_families(edge);
        arc.iter_subgroups()
            .any(|g| families.contains(&g.type_.as_str()))
    };
    let rise = present(Transition::Rise);
    let fall = present(Transition::Fall);

    // Read structurally, the way `extract_timing_tables_from_arc` reads
    // `timing_sense`: `Value::expr` panics on a value spelled as a quoted string, and
    // both spellings are ordinary Liberty. A value of neither shape names no timing
    // type at all, and is treated exactly as an absent attribute is.
    let timing_type = arc.simple_attribute("timing_type").and_then(|v| match v {
        Value::Expression(text) | Value::String(text) => Some(text.as_str()),
        _ => None,
    });

    match timing_type {
        // Already stated asynchronously -- by the input library, or by an earlier run
        // over the same file. Nothing to derive and nothing to choose.
        Some("clear" | "preset") => Ok(vec![arc.clone()]),

        // The suffixless form names no arrival, so the tables are the whole of the
        // evidence, and either one or both of them may be there.
        Some("combinational") => match (rise, fall) {
            (false, false) => Err(NO_TABLE.to_owned()),
            (true, false) => Ok(vec![retagged(arc, Transition::Rise)]),
            (false, true) => Ok(vec![retagged(arc, Transition::Fall)]),
            (true, true) => Ok(vec![
                half(arc, Transition::Rise),
                half(arc, Transition::Fall),
            ]),
        },

        // The suffixed forms name one arrival themselves. Where the tables agree the
        // suffix is redundant and the answer is the same one the tables give; where
        // they disagree the arc contradicts itself and nothing here may pick a winner.
        Some(t @ "combinational_rise") => match (rise, fall) {
            (_, true) => Err(format!(
                "states {} yet carries a fall-family table, which contradicts the one \
                 arrival the suffix names",
                t
            )),
            (true, false) => Ok(vec![retagged(arc, Transition::Rise)]),
            (false, false) => Err(NO_TABLE.to_owned()),
        },
        Some(t @ "combinational_fall") => match (rise, fall) {
            (true, _) => Err(format!(
                "states {} yet carries a rise-family table, which contradicts the one \
                 arrival the suffix names",
                t
            )),
            (false, true) => Ok(vec![retagged(arc, Transition::Fall)]),
            (false, false) => Err(NO_TABLE.to_owned()),
        },

        None => Err("states no timing_type".to_owned()),
        Some(t) => Err(format!(
            "timing_type {} is neither combinational nor an asynchronous-reset type",
            t
        )),
    }
}

#[cfg(test)]
mod tests {
    //! Behaviour of the `reset` module: which asynchronous type a retained reset arc
    //! is restated under, and which arcs cannot be stated at all.
    //!
    //! Every fixture below is invented. The names are single letters and the values
    //! are round numbers chosen for this file; nothing is taken from any library.

    use super::*;

    /// The one arc of a one-cell library, so a fixture can be written as Liberty text
    /// and read back as the [`Group`] the conversion would hand this module.
    fn timing_arc(body: &str) -> Group {
        let lib = liberty_parser::parse_lib(&format!(
            r#"
library(reset_test) {{
  lu_table_template(T) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}
  cell(DUT) {{
    pin(Q) {{
      direction: output;
      function: "IQ";
      timing() {{
{}
      }}
    }}
  }}
}}"#,
            body
        ))
        .expect("parse the arc fixture");
        let arc = lib[0]
            .get_cell("DUT")
            .expect("DUT")
            .get_pin("Q")
            .expect("Q")
            .iter_subgroups_of_type("timing")
            .next()
            .expect("the fixture's one timing group")
            .clone();
        arc
    }

    /// The `timing_type` a restated arc states, read the same structural way the code
    /// under test reads the input's.
    fn timing_type_of(arc: &Group) -> String {
        match arc
            .simple_attribute("timing_type")
            .expect("a restated arc states a timing_type")
        {
            Value::Expression(text) | Value::String(text) => text.clone(),
            other => panic!("timing_type is not a name: {:?}", other),
        }
    }

    /// The types of the arc's subgroups, in the order it carries them.
    fn subgroup_types(arc: &Group) -> Vec<String> {
        arc.iter_subgroups().map(|g| g.type_.clone()).collect()
    }

    /// The one restated arc stating `timing_type`. Asserts there is exactly one, so a
    /// set of results is addressed by what each member says rather than by position --
    /// the order two halves come back in is not a property of this function.
    fn the_arc_stating<'a>(restated: &'a [Group], timing_type: &str) -> &'a Group {
        let matching: Vec<&Group> = restated
            .iter()
            .filter(|g| timing_type_of(g) == timing_type)
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "expected exactly one {} arc, got {:?}",
            timing_type,
            restated.iter().map(timing_type_of).collect::<Vec<String>>()
        );
        matching[0]
    }

    /// An arc carrying all four table families, so both arrivals are stated at once.
    const BOTH_EDGES: &str = r#"
        related_pin: "R";
        timing_sense : positive_unate;
        timing_type: combinational;
        when : "A * B";
        sdf_cond : "A & B";
        cell_rise(T) { values("1.0, 2.0", "3.0, 4.0"); }
        cell_fall(T) { values("1.5, 2.5", "3.5, 4.5"); }
        rise_transition(T) { values("0.1, 0.2", "0.3, 0.4"); }
        fall_transition(T) { values("0.11, 0.21", "0.31, 0.41"); }"#;

    /// A `timing` group states one `timing_type`, so an arc that measured the output
    /// arriving in both directions cannot be stated as one asynchronous arc: it takes
    /// one `preset` for the rise it measured and one `clear` for the fall.
    ///
    /// Killed by: `half` kept the opposite edge's families instead of its own -- `let keep = edge_families(match edge { Transition::Rise => Transition::Fall, Transition::Fall => Transition::Rise });`. It also reddens `engine::tests::flop_mode_states_a_combinational_reset_arc_asynchronously` and nothing else -- 163 passed, 2 failed. No mutation separates the two: which families a half carries is one rule, and that test is the same rule witnessed through a whole conversion rather than on the function.
    #[test]
    fn an_arc_carrying_both_edges_becomes_a_preset_and_a_clear() {
        let restated = restate_reset_arc(&timing_arc(BOTH_EDGES)).expect("both edges are statable");

        assert_eq!(restated.len(), 2, "one arc per arrival measured");

        // Asserted as a set. Which half comes back first is not a property of the
        // emitted library and must never become one.
        let preset = the_arc_stating(&restated, "preset");
        assert_eq!(
            subgroup_types(preset),
            vec!["cell_rise", "rise_transition"],
            "the preset half keeps the rise family it was named for, and only that"
        );
        let clear = the_arc_stating(&restated, "clear");
        assert_eq!(
            subgroup_types(clear),
            vec!["cell_fall", "fall_transition"],
            "the clear half keeps the fall family it was named for, and only that"
        );
    }

    /// Splitting an arc restates how it is tagged, not what it is about. Both halves
    /// describe the same path under the same condition the library wrote, so the pin
    /// the arc comes from, the sense it was characterised under and the state it holds
    /// in have to come through untouched -- a half that lost its `when` would claim to
    /// hold unconditionally, which is a different arc.
    ///
    /// Killed by: `half` cleared `when` and `sdf_cond` from the retagged clone -- `restated.attributes.shift_remove("when");` and the same for `sdf_cond`.
    #[test]
    fn a_split_half_keeps_the_condition_and_sense_the_library_wrote() {
        let original = timing_arc(BOTH_EDGES);
        let restated = restate_reset_arc(&original).expect("both edges are statable");

        for half in &restated {
            for attribute in ["related_pin", "timing_sense", "when", "sdf_cond"] {
                assert_eq!(
                    half.simple_attribute(attribute),
                    original.simple_attribute(attribute),
                    "{} must survive the split unchanged",
                    attribute
                );
            }
        }
    }

    /// A half is built from the families of the one arrival it names, so a subgroup
    /// belonging to neither family is carried by neither half. Nothing here can say
    /// which arrival such a subgroup describes -- so putting it on a half would state,
    /// on the authority of nothing, that it is true of that direction. The fixture's
    /// `unclassified_table` is invented for this test and names no Liberty group type
    /// the tool knows: what is pinned is that an UNRECOGNISED subgroup is discarded,
    /// which is the property a list of what to shed cannot have.
    ///
    /// Killed by: `half` shed the opposite edge's families in place of keeping its own -- `let keep = edge_families(match edge { Transition::Rise => Transition::Fall, Transition::Fall => Transition::Rise });` with the retain negated to `!keep.contains(&g.type_.as_str())` -- so the unclassified subgroup rode onto both halves. Observed to redden this test alone: every other fixture reaching `half` carries the four families and nothing else, on which shedding the opposite edge's two and keeping its own two select the same subgroups.
    #[test]
    fn a_subgroup_of_neither_family_is_discarded_from_both_halves() {
        let arc = timing_arc(&format!(
            "{}\n        unclassified_table(T) {{ values(\"0.01, 0.02\", \"0.03, 0.04\"); }}",
            BOTH_EDGES
        ));

        let restated = restate_reset_arc(&arc).expect("both edges are statable");

        assert_eq!(restated.len(), 2);
        for half in &restated {
            assert!(
                !subgroup_types(half).contains(&"unclassified_table".to_owned()),
                "the {} half kept a subgroup of neither family: {:?}",
                timing_type_of(half),
                subgroup_types(half)
            );
        }
    }

    /// An arc measuring one arrival is already one arc, so restating it only changes
    /// the tag: there is no second half for anything to be shared with, and every
    /// subgroup it had stays exactly where it was.
    ///
    /// Killed by: the `combinational_fall` single-family case applied the split's filter to the unsplit arc -- `only.subgroups.retain(|g| edge_families(Transition::Fall).contains(&g.type_.as_str()))` on the retagged clone.
    #[test]
    fn a_single_edge_arc_keeps_every_subgroup_it_had() {
        let arc = timing_arc(
            r#"
        related_pin: "R";
        timing_sense : positive_unate;
        timing_type: combinational_fall;
        cell_fall(T) { values("1.5, 2.5", "3.5, 4.5"); }
        fall_transition(T) { values("0.11, 0.21", "0.31, 0.41"); }
        ocv_sigma_cell_rise(T) { values("0.01, 0.02", "0.03, 0.04"); }"#,
        );

        let restated = restate_reset_arc(&arc).expect("one arrival is statable");

        assert_eq!(restated.len(), 1, "one arrival measured, one arc emitted");
        assert_eq!(timing_type_of(&restated[0]), "clear");
        assert_eq!(
            subgroup_types(&restated[0]),
            vec!["cell_fall", "fall_transition", "ocv_sigma_cell_rise"],
            "all three subgroups survive, in the order the library wrote them"
        );
    }

    /// A rise family says the output arrived high, and the asynchronous type that
    /// drives an output high is `preset`.
    ///
    /// Killed by: the combinational rise-only case emitted the other direction -- `(true, false) => Ok(vec![retagged(arc, Transition::Fall)])`.
    #[test]
    fn a_rise_only_combinational_arc_becomes_a_preset() {
        let arc = timing_arc(
            r#"
        related_pin: "R";
        timing_sense : negative_unate;
        timing_type: combinational;
        cell_rise(T) { values("1.0, 2.0", "3.0, 4.0"); }
        rise_transition(T) { values("0.1, 0.2", "0.3, 0.4"); }"#,
        );

        let restated = restate_reset_arc(&arc).expect("one arrival is statable");

        assert_eq!(restated.len(), 1);
        // `negative_unate` on the fixture is deliberate: the sense says the input
        // fell, and the answer is `preset` regardless, because the type names what
        // the OUTPUT did.
        assert_eq!(timing_type_of(&restated[0]), "preset");
    }

    /// An arc already stating an asynchronous type has nothing left to derive -- the
    /// library, or an earlier run over the same file, has already said which way the
    /// output arrives. Returning it verbatim is what makes the restatement idempotent.
    ///
    /// The fixture carries BOTH families deliberately. That is the one shape where
    /// re-deriving would visibly change the arc -- two halves instead of one arc --
    /// so it is what makes "verbatim" something the test can see rather than assume.
    ///
    /// Killed by: the asynchronous arm fell through to the combinational split -- `Some("clear" | "preset" | "combinational") => match (rise, fall) { .. }` -- so the arc came back as two.
    #[test]
    fn an_arc_already_stating_an_asynchronous_type_is_returned_verbatim() {
        let arc = timing_arc(
            r#"
        related_pin: "R";
        timing_sense : positive_unate;
        timing_type: clear;
        when : "A * B";
        sdf_cond : "A & B";
        cell_rise(T) { values("1.0, 2.0", "3.0, 4.0"); }
        cell_fall(T) { values("1.5, 2.5", "3.5, 4.5"); }
        rise_transition(T) { values("0.1, 0.2", "0.3, 0.4"); }
        fall_transition(T) { values("0.11, 0.21", "0.31, 0.41"); }"#,
        );

        let restated = restate_reset_arc(&arc).expect("an asynchronous arc is already stated");

        assert_eq!(restated, vec![arc], "returned byte for byte as it arrived");
    }

    /// The tag is read off the tables, so an arc carrying none of them offers nothing
    /// to read: no delay and no transition in either direction says which way the
    /// output arrived, and the type may not be guessed from anything else.
    ///
    /// Killed by: the `(false, false)` combinational case returned `Ok(vec![retagged(arc, Transition::Fall)])`, tagging a tableless arc `clear` on no evidence.
    #[test]
    fn an_arc_with_no_delay_or_transition_table_is_refused() {
        let arc = timing_arc(
            r#"
        related_pin: "R";
        timing_sense : positive_unate;
        timing_type: combinational;"#,
        );

        assert_eq!(
            restate_reset_arc(&arc),
            Err(NO_TABLE.to_owned()),
            "a tableless arc states no arrival"
        );
    }

    /// A `combinational_rise` suffix names one arrival and a fall family states the
    /// other, so the arc contradicts itself. Nothing here may pick a winner: the arc
    /// is malformed and saying so is the whole answer.
    ///
    /// Killed by: the `combinational_rise` arm dropped its contradiction check -- `(_, true) => Ok(vec![retagged(arc, Transition::Rise)])` -- so the self-contradicting arc came back tagged `preset`.
    #[test]
    fn a_suffix_contradicting_its_tables_is_refused() {
        let arc = timing_arc(
            r#"
        related_pin: "R";
        timing_sense : positive_unate;
        timing_type: combinational_rise;
        cell_fall(T) { values("1.5, 2.5", "3.5, 4.5"); }
        fall_transition(T) { values("0.11, 0.21", "0.31, 0.41"); }"#,
        );

        assert_eq!(
            restate_reset_arc(&arc),
            Err(
                "states combinational_rise yet carries a fall-family table, which \
                 contradicts the one arrival the suffix names"
                    .to_owned()
            )
        );
    }

    /// An arc stating no `timing_type` states no family of type either, so there is
    /// nothing to restate: the tables alone cannot say that a combinational arc was
    /// meant rather than a check or a three-state control.
    ///
    /// Killed by: the `None` arm was folded into the combinational one -- `None | Some("combinational") => ...` -- so a typeless arc carrying a fall family came back tagged `clear`.
    #[test]
    fn an_arc_with_no_timing_type_is_refused() {
        let arc = timing_arc(
            r#"
        related_pin: "R";
        timing_sense : positive_unate;
        cell_fall(T) { values("1.5, 2.5", "3.5, 4.5"); }
        fall_transition(T) { values("0.11, 0.21", "0.31, 0.41"); }"#,
        );

        assert_eq!(
            restate_reset_arc(&arc),
            Err("states no timing_type".to_owned())
        );
    }

    /// A type from outside the combinational family describes something other than a
    /// reset driving an output -- a three-state control here -- and restating it as a
    /// reset would assert a path the library never characterised.
    ///
    /// Killed by: the catch-all arm routed unknown types into the combinational case -- `Some(_) => match (rise, fall) { .. }`, the suffixless arm's body copied verbatim -- so the three-state arc came back tagged `clear`.
    #[test]
    fn an_arc_of_an_unexpected_timing_type_is_refused() {
        let arc = timing_arc(
            r#"
        related_pin: "R";
        timing_sense : positive_unate;
        timing_type: three_state_disable;
        cell_fall(T) { values("1.5, 2.5", "3.5, 4.5"); }
        fall_transition(T) { values("0.11, 0.21", "0.31, 0.41"); }"#,
        );

        assert_eq!(
            restate_reset_arc(&arc),
            Err(
                "timing_type three_state_disable is neither combinational nor an \
                 asynchronous-reset type"
                    .to_owned()
            )
        );
    }
}
