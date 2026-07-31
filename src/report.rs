//! What the conversion cost: the arcs as characterised, the arcs rebuilt from
//! the model, and the residual between them.

use crate::arcs::{
    input_transition, restore_arc, EdgeRef, RefArc, References, Scope, TimingSense, Transition,
    WhenMerge,
};
use crate::conditions::ClassId;
use ndarray::prelude::*;
use std::collections::BTreeMap;

/// One arc exactly as the library characterised it, before any reduction.
///
/// The engine folds a pin pair's `when` conditions into one representative arc
/// to build the model, but the model still has to stand against every condition
/// it claims to cover. Keeping the raw tables lets the report measure it against
/// what was actually measured, so the cost of the `when` reduction shows up
/// instead of being averaged out of its own error term.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ConditionedArc {
    pub(crate) source: String,
    pub(crate) output: String,
    /// The `when` expression this arc was characterised under, if any.
    pub(crate) when: Option<String>,
    /// How this arc's input drives its output. Not optional: an arc whose sense
    /// determines no input direction is skipped before it can be recorded here.
    pub(crate) sense: TimingSense,
    /// The post-settled state each delay family of this arc describes, as a class
    /// within its output. `None` where the arc carries no such family, where it has
    /// no `when` at all -- the catch-all -- or where its `when` could not be read.
    pub(crate) class_rise: Option<ClassId>,
    pub(crate) class_fall: Option<ClassId>,
    /// The condition this arc's checks are grouped under, as a class within its
    /// SOURCE pin. One per arc and not one per edge: a check is stated under the
    /// source `when` alone, which both edges of an arc share. `None` where the arc
    /// states no `when` -- the catch-all -- and under every mode that groups the
    /// checks under nothing at all.
    pub(crate) check_class: Option<ClassId>,
    pub(crate) cell_rise: Option<Array2<f64>>,
    pub(crate) cell_fall: Option<Array2<f64>>,
}

/// One post-settled state a cell's arcs describe, and which arcs describe it.
///
/// Grouped per output and per output edge, because a state is only a collision when
/// two arcs claim the same one of the same edge of the same output.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct StateClass {
    pub(crate) output: String,
    /// The OUTPUT's transition: the table family these arcs were read from.
    pub(crate) edge: Transition,
    /// The condition in Liberty's spelling. `None` marks the catch-all row, which
    /// holds the arcs that state no `when` at all and so cover whatever the
    /// conditioned ones do not.
    pub(crate) condition: Option<String>,
    /// The source pin and `when` of every arc that landed here.
    pub(crate) members: Vec<(String, Option<String>)>,
}

/// One condition an input pin's setup and hold checks are grouped under.
///
/// Deliberately distinct from [`StateClass`]. That groups the states an arc leaves
/// the cell in, per output and output edge, and is what the emitted clock-to-output
/// delays are conditioned on. This groups the conditions the source library
/// characterised one input under, and is what the emitted checks are conditioned on
/// -- the source `when` verbatim, with no literal conjoined. The two are classified
/// separately, number their classes independently, and are never conflated.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CheckClass {
    pub(crate) pin: String,
    /// The source condition in the library's own spelling. `None` marks the
    /// catch-all, which holds the arcs that state no `when` at all.
    pub(crate) condition: Option<String>,
    /// The output each arc that landed here was characterised against, in the order
    /// the library declares them.
    pub(crate) members: Vec<String>,
}

/// How far the pseudo-flop split lands from the arc it replaced.
///
/// An original arc is a slew x load table. The model stores it as a
/// slew-dependent setup constraint on the input plus a load-dependent
/// clock-to-output delay, and reconstructs it as the outer sum of the two --
/// [`restore_arc`]. The residual is what that separable form cannot express.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ArcError {
    pub(crate) cell: String,
    pub(crate) source: String,
    pub(crate) output: String,
    /// "rise" or "fall"
    pub(crate) edge: &'static str,
    /// The `when` condition of the arc being reconstructed, if it had one.
    pub(crate) when: Option<String>,
    pub(crate) original: Array2<f64>,
    pub(crate) reconstructed: Array2<f64>,
    pub(crate) error: Array2<f64>,
}

impl ArcError {
    /// Mean magnitude of the original table, to judge the residual against.
    pub(crate) fn scale(&self) -> f64 {
        self.original.iter().map(|v| v.abs()).sum::<f64>() / self.original.len() as f64
    }

    /// Mean signed residual: positive means the model is pessimistic.
    pub(crate) fn bias(&self) -> f64 {
        self.error.iter().sum::<f64>() / self.error.len() as f64
    }

    pub(crate) fn rms(&self) -> f64 {
        (self.error.iter().map(|v| v * v).sum::<f64>() / self.error.len() as f64).sqrt()
    }

    pub(crate) fn max_abs(&self) -> f64 {
        self.error.iter().fold(0.0_f64, |m, v| m.max(v.abs()))
    }

    /// The residual at each point as a percentage of the arc's value there.
    ///
    /// The statistics judge the whole table against one magnitude, which hides
    /// that a fixed residual matters far more at a fast corner of the table than
    /// at a slow one. Dividing point by point makes the regions comparable.
    ///
    /// The quotient is unbounded where the arc passes through zero, which a real
    /// delay table does: at a slow enough input slew the output arrives before the
    /// input has finished switching, and the characterised delay goes negative.
    /// Near that crossing an ordinary residual divided by a vanishing value is
    /// arbitrarily large, so these figures are read with the percentiles beside
    /// them rather than by their extremes. At a point of exactly zero there is no
    /// quotient at all, and the infinity or NaN is left standing rather than
    /// replaced by a number the data does not support.
    pub(crate) fn relative_error(&self) -> Array2<f64> {
        &self.error / &self.original * 100.0
    }

    /// RMS residual as a percentage of the arc's own magnitude.
    pub(crate) fn rms_percent(&self) -> f64 {
        let scale = self.scale();
        if scale == 0.0 {
            0.0
        } else {
            100.0 * self.rms() / scale
        }
    }
}

/// A candidate cell, or one output of one, that the conversion refused, and why.
///
/// `output` is `None` for a cell-scope refusal, where the whole cell was emitted
/// verbatim. A cell that is not a candidate at all produces no `Refusal`: nothing was
/// asked of the tool, so it has nothing to report about it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Refusal {
    pub(crate) library: String,
    pub(crate) cell: String,
    pub(crate) output: Option<String>,
    pub(crate) reason: String,
}

/// What one library's conversion produced: the cells it converted, and what it
/// refused.
///
/// A run with skips still exits 0, so the report and the standard-error warnings are
/// the only signal a caller has. A refusal reaching neither would be invisible to any
/// script reading the artefacts.
#[derive(Debug, Clone, Default)]
pub(crate) struct LibraryReport {
    pub(crate) cells: Vec<CellReport>,
    pub(crate) refusals: Vec<Refusal>,
}

/// Where one merged delay table belongs: the check it is summed into, the reference
/// it is charged against, and which values of the arc it holds.
///
/// The two scopes are two different partitions and are named rather than positioned,
/// because a key carrying both of them is exactly where they have been confused
/// before. `check` is the class of the SOURCE pin's own `when`, which is what says
/// which emitted check group these values are averaged into; `delay` is the state the
/// OUTPUT settles in, which is what says which clock-to-output delay they are
/// measured from. One source pin driving two outputs under one condition has two
/// entries at one `check` and two different `delay`s, which is the whole reason the
/// two cannot be one field.
///
/// The input's direction is part of the key because that is what a constraint is
/// keyed on, and it is not the output's: a negative-unate arc's `cell_rise` values
/// describe an input that fell. The family stays in the key beside it because the
/// reference the values are charged against is still chosen by the family.
///
/// The fields are declared in the order the map is sorted by, which is the order the
/// constraint arithmetic sums them in. `check` sits after `delay` so that it is a
/// tiebreaker alone: an entry that splits in two because its arcs are checked under
/// different conditions sorts where the single entry did, and every other entry keeps
/// the position it always had.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ConstraintKey {
    pub(crate) src: String,
    pub(crate) outpin: String,
    /// The scope the reference these values are charged against is drawn at.
    pub(crate) delay: Scope,
    /// The scope of the check group these values are summed into.
    pub(crate) check: Scope,
    pub(crate) input_edge: Transition,
    pub(crate) family: &'static str,
}

/// The merged delay tables the constraints are built from.
pub(crate) type ConstraintArcs = BTreeMap<ConstraintKey, Array2<f64>>;

/// One input direction's constraints, keyed by the constrained pin and the scope of
/// the condition its checks are grouped under.
///
/// That scope is the pin's OWN, not the driven output's: a check sits on the pin pair
/// `D -> G` and is stated under the condition the library characterised `D` under, so
/// one value per check group is what a group can carry. Under per-state a pin has one
/// constraint per condition it was characterised under, averaged over the outputs it
/// drives under that condition; under the other two modes there is one group per pin,
/// at [`Scope::Whole`].
pub(crate) type Constraints = BTreeMap<(String, Scope), Array1<f64>>;

/// Everything the reconstruction report shows for one processed cell.
///
/// The engine returns this rather than writing it out itself, so the library
/// performs no I/O and the numbers stay inspectable from tests.
#[derive(Debug, Clone)]
pub(crate) struct CellReport {
    pub(crate) library: String,
    pub(crate) cell: String,
    /// How this cell's `when` conditions were merged.
    pub(crate) when_merge: WhenMerge,
    /// Every arc as characterised, one entry per `when` condition.
    pub(crate) raw_arcs: Vec<ConditionedArc>,
    /// The same arcs after the `when` conditions were folded together, keyed by
    /// source pin, output pin, the direction the INPUT was moving in and the table
    /// family the values were read from. This is what the constraints were actually
    /// derived from.
    pub(crate) constraint_arcs: ConstraintArcs,
    /// Reference arc chosen per output and scope.
    pub(crate) ref_arcs: BTreeMap<(String, Scope), RefArc>,
    /// Reference the constraints were taken against.
    pub(crate) mean_ref: RefArc,
    /// The constraints, keyed on the constrained pin's own transition -- which is
    /// what Liberty's `rise_constraint` and `fall_constraint` are keyed on too.
    pub(crate) setup_input_rise: Constraints,
    pub(crate) setup_input_fall: Constraints,
    /// The same again negated, which is what a hold constraint is.
    pub(crate) hold_input_rise: Constraints,
    pub(crate) hold_input_fall: Constraints,
    /// The slew and load points every table of this cell is indexed at, so a residual
    /// can be read against the regime it occurs in. `None` where the library declares
    /// no such axis, or where the cell's arcs disagreed about it.
    pub(crate) slews: Option<Vec<f64>>,
    pub(crate) loads: Option<Vec<f64>>,
    /// How much error the conversion introduces over each arc as the source
    /// library characterised it, one entry per `when` condition. Those originals
    /// are the only honest baseline: the merged arc is itself a source of error,
    /// so measuring against it would report less error than is introduced.
    pub(crate) arcs: Vec<ArcError>,
    /// The post-settled states this cell's arcs describe, grouped by the function
    /// they denote. Under per-state these are the states the emitted delays are
    /// conditioned on; under the other two modes nothing in the split consults them,
    /// and they say how much of the cell a per-state model would have to distinguish.
    pub(crate) classes: Vec<StateClass>,
    /// The condition each of those classes denotes, in Liberty's spelling, so a
    /// reference filed under a class can be captioned with the state it describes
    /// rather than with a number. Empty where the mode draws one reference per output
    /// and there is no state to name.
    pub(crate) class_conditions: BTreeMap<ClassId, String>,
    /// The conditions each input pin's checks were grouped under. Empty where the
    /// mode emits one unconditioned check per pin, because then the checks were
    /// grouped under nothing.
    pub(crate) check_classes: Vec<CheckClass>,
    /// The condition each of those check classes denotes, keyed by the pin as well as
    /// by the class because the checks are classified per pin and their numbers
    /// restart with each. This is what captions a constraint, which is filed under
    /// its own pin's condition and not under any output's state -- the two are
    /// different partitions, so one map could not caption both.
    pub(crate) check_conditions: BTreeMap<(String, ClassId), String>,
}

/// One half of an arc: the direction the OUTPUT moved in, which raw table records
/// it, and which component of a reference arc pairs with it. Bundled so the three
/// can never be mismatched.
pub(crate) struct Edge {
    name: &'static str,
    /// The output's transition, which is what a delay table family names.
    output: Transition,
    raw: fn(&ConditionedArc) -> Option<&Array2<f64>>,
    /// The post-settled class this edge of an arc fell into, which is what names the
    /// reference it was measured against under a per-state model.
    class: fn(&ConditionedArc) -> Option<ClassId>,
    reference: fn(&RefArc) -> Option<&EdgeRef>,
}

pub(crate) const RISE: Edge = Edge {
    name: "rise",
    output: Transition::Rise,
    raw: |a| a.cell_rise.as_ref(),
    class: |a| a.class_rise,
    reference: |r| r.rise.as_ref(),
};

pub(crate) const FALL: Edge = Edge {
    name: "fall",
    output: Transition::Fall,
    raw: |a| a.cell_fall.as_ref(),
    class: |a| a.class_fall,
    reference: |r| r.fall.as_ref(),
};

/// Measure the reconstruction residual against every condition of every arc.
///
/// Each raw condition is reconstructed from the pair the model actually holds for
/// it: the setup constraint its own pin carries under its own condition, plus the
/// delay of the reference its output's state was drawn at. That is the pair a
/// consumer adds up, so the residual it leaves contains every error source at once:
/// what the separable setup-plus-delay form cannot express, what collapsing the arcs
/// sharing a state into one threw away, and what averaging a check over the several
/// outputs its pin drives threw away with it. It is always taken against this arc's
/// OWN raw table -- measuring against the merged one would report less error than the
/// merge introduced.
pub(crate) fn collect_arc_errors(
    arcs: &[ConditionedArc],
    setup_input_rise: &Constraints,
    setup_input_fall: &Constraints,
    references: &References,
    cell_name: &str,
    edge: &Edge,
    out: &mut Vec<ArcError>,
) {
    for arc in arcs {
        let (source, output) = (&arc.source, &arc.output);
        let Some(original) = (edge.raw)(arc) else {
            continue;
        };
        // The scope this edge of this arc was filed under, which is what says which
        // reference it was measured against.
        let Some(scope) = Scope::of(references.mode, (edge.class)(arc), arc.when.is_none()) else {
            continue;
        };
        // And the scope its own pin's checks were grouped under, which is what says
        // which constraint it contributed to. The two are different partitions: this
        // arc's values were averaged with every other arc the pin drives under the
        // same condition, whatever states those left their own outputs in, so reading
        // the constraint at the delay-side scope would measure the arc against a value
        // no check on this pin carries.
        let Some(check) = Scope::of(references.mode, arc.check_class, arc.when.is_none()) else {
            continue;
        };
        // Which constraint this arc was folded into is the arc's own property: the
        // direction its INPUT was moving in. Reading the map named after the output
        // family instead would reconstruct a negative-unate arc from a vector no part
        // of it contributed to.
        let setup = match input_transition(arc.sense, edge.output) {
            Some(Transition::Rise) => setup_input_rise,
            Some(Transition::Fall) => setup_input_fall,
            None => continue,
        };
        let Some(slew_dependent) = setup.get(&(source.clone(), check)) else {
            continue;
        };
        // Reconstruct with the delay this mode actually emits, so the residual
        // describes the library that ships rather than an idealised split. The
        // original report predated pooling and always used the per-output arc.
        let Some(delay_ref) = references.delay_for(output, &scope) else {
            continue;
        };
        let Some(delay_ref) = (edge.reference)(delay_ref) else {
            continue;
        };

        let reconstructed = restore_arc(slew_dependent, &delay_ref.delay);
        if reconstructed.raw_dim() != original.raw_dim() {
            continue;
        }

        out.push(ArcError {
            cell: cell_name.to_owned(),
            source: source.clone(),
            output: output.clone(),
            edge: edge.name,
            when: arc.when.clone(),
            error: reconstructed.clone() - original,
            original: original.clone(),
            reconstructed,
        });
    }
}

#[cfg(test)]
mod tests {
    //! Behaviour of the `report` module: reconstruction reports and residual measurement.

    use super::*;
    use crate::arcs::{restore_arc, Anchor, ReferenceMode, WhenMerge};
    use crate::engine::{process_library, CellOptions}; // Test-only; a unit test observes its subject through the real engine path rather than a stub.
    use liberty_parser::liberty::Liberty;
    use regex::Regex;

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

    /// Four timing tables for one arc, so `select_reference_arc` accepts it.
    fn arc(related_pin: &str, base: f64) -> String {
        format!(
            r#"
        timing() {{
          related_pin: "{}";
          timing_sense : positive_unate;
          cell_rise(T) {{ values("{}, {}", "{}, {}"); }}
          cell_fall(T) {{ values("{}, {}", "{}, {}"); }}
          rise_transition(T) {{ values("0.1, 0.2", "0.3, 0.4"); }}
          fall_transition(T) {{ values("0.11, 0.21", "0.31, 0.41"); }}
        }}"#,
            related_pin,
            base,
            base + 1.0,
            base + 2.0,
            base + 3.0,
            base + 0.5,
            base + 1.5,
            base + 2.5,
            base + 3.5,
        )
    }

    /// Same as [`arc`] but characterised under a `when` condition.
    fn conditioned_arc(related_pin: &str, base: f64, when: &str) -> String {
        arc(related_pin, base).replace(
            &format!(r#"related_pin: "{}";"#, related_pin),
            &format!(
                "related_pin: \"{}\";\n          when: \"{}\";",
                related_pin, when
            ),
        )
    }

    fn bundle_lib(cell_body: String) -> Liberty {
        liberty_parser::parse_lib(&format!(
            r#"
library(bundle_test) {{
  lu_table_template(T) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}
  cell(DUT) {{
    latch_bank(IQ,IQN,2) {{ enable: "G"; data_in: "D"; }}
    pin(G) {{ direction: input; clock: true; }}
{}
  }}
}}"#,
            cell_body
        ))
        .expect("parse bundle fixture")
    }

    /// The report is the only evidence of how much the split costs, and it was
    /// once removed unnoticed by a refactor because nothing exercised it. These
    /// tests exist so that cannot happen silently again.
    /// A flat cell whose output is genuinely characterised against its input, so
    /// there is a real arc for the split to be measured against.
    fn reportable_lib() -> Liberty {
        bundle_lib(format!(
            r#"
    pin(D) {{ direction: input; }}
    pin(Q) {{
      direction: output;
      function: "IQ";
      {}
    }}"#,
            arc("D", 1.0)
        ))
    }

    /// A dual-rail cell whose two outputs are characterised against deliberately
    /// different arcs, both driven by the same source.
    ///
    /// This is the only cell topology in which the two reference modes can be
    /// told apart: the pooled delay is the mean over the outputs' reference
    /// arcs, so with one output -- or with several outputs whose references
    /// coincide -- the mean IS each output's own arc and the modes are identical
    /// by construction, whatever the code does.
    fn dual_rail_lib() -> Liberty {
        bundle_lib(format!(
            r#"
    pin(D) {{ direction: input; }}
    pin(Q1) {{
      direction: output;
      function: "IQ";
      {}
    }}
    pin(Q2) {{
      direction: output;
      function: "IQN";
      {}
    }}"#,
            arc("D", 1.0),
            arc("D", 21.0)
        ))
    }

    // --- ArcError statistics -------------------------------------------------

    fn arc_error(original: Array2<f64>, error: Array2<f64>) -> ArcError {
        let reconstructed = original.clone() + error.clone();
        ArcError {
            cell: "DUT".to_owned(),
            source: "D".to_owned(),
            output: "Q".to_owned(),
            edge: "rise",
            when: None,
            original,
            reconstructed,
            error,
        }
    }

    /// Killed by: `ArcError::rms` dropped its `.sqrt()`.
    #[test]
    fn arc_error_statistics_are_the_hand_computed_values() {
        // original = [[1,2],[3,4]], error = [[2,2],[2,-2]].
        //
        // A constant-magnitude, non-cancelling error is deliberate: with
        // error = [[1,-1],[1,-1]] the sum is 0, so a bias that divides by the
        // wrong denominator (e.g. 2x the element count) would still read 0.0,
        // and its mean square is 1, so dropping rms's sqrt() would still read
        // 1.0 (sqrt(1) == 1). Both would go unnoticed. This shape avoids both
        // coincidences: bias is genuinely nonzero and mean square is not a
        // fixed point of sqrt.
        let original = Array2::from_shape_vec((2, 2), vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        let error = Array2::from_shape_vec((2, 2), vec![2.0, 2.0, 2.0, -2.0]).unwrap();
        let err = arc_error(original, error);

        // scale = mean(|original|) = (1+2+3+4)/4 = 2.5
        assert_eq!(err.scale(), 2.5);
        // bias = mean(error) = (2+2+2-2)/4 = 1.0
        assert_eq!(err.bias(), 1.0);
        // rms = sqrt(mean(error^2)) = sqrt((4+4+4+4)/4) = sqrt(4) = 2.0
        assert_eq!(err.rms(), 2.0);
        // max_abs = max(|error|) = 2.0
        assert_eq!(err.max_abs(), 2.0);
        // rms_percent = 100 * rms / scale = 100 * 2.0 / 2.5 = 80.0
        assert_eq!(err.rms_percent(), 80.0);
    }

    /// Killed by: `ArcError::rms_percent` divided by `scale` unconditionally, dropping the zero-scale guard.
    #[test]
    fn rms_percent_is_zero_when_the_arc_has_no_magnitude() {
        let original = Array2::from_shape_vec((2, 2), vec![0.0; 4]).unwrap();
        let error = Array2::from_shape_vec((2, 2), vec![5.0, -3.0, 2.0, -1.0]).unwrap();
        let err = arc_error(original, error);
        assert_eq!(err.rms_percent(), 0.0);
    }

    /// Killed by: `process_cell` pushed its `CellReport` with `arcs: Vec::new()`.
    #[test]
    fn every_processed_cell_yields_a_reconstruction_report() {
        let reset = Regex::new("(R|S)N?").unwrap();

        for mode in [ReferenceMode::Pooled, ReferenceMode::PerOutput] {
            let mut lib = reportable_lib();
            let reports = process_library(
                &mut lib[0],
                &opts("G", &reset, false, mode, WhenMerge::Mean),
            );

            let report = reports
                .cells
                .iter()
                .find(|r| r.cell == "DUT")
                .unwrap_or_else(|| panic!("no report for the processed cell in {:?}", mode));

            assert_eq!(report.library, "bundle_test");
            assert!(
                !report.constraint_arcs.is_empty(),
                "the original arcs must be reported, not just the residual"
            );
            assert!(!report.ref_arcs.is_empty(), "{:?}", mode);
            assert!(!report.setup_input_rise.is_empty(), "{:?}", mode);
            assert!(
                !report.arcs.is_empty(),
                "at least one arc must be measured in {:?}",
                mode
            );
        }
    }

    /// The residual is measured against the delay the chosen mode actually
    /// emits, so pooling a cell's outputs shows up as the error it is.
    ///
    /// The mode only reaches the report through the clock-to-output delay, so a
    /// cell whose rails disagree is what makes the two readings separable at
    /// all -- see [`dual_rail_lib`].
    ///
    /// Killed by: `References::delay_for` returned `Some(self.mean)` for `PerOutput`, collapsing the two modes onto one delay.
    #[test]
    fn pooled_and_per_output_residuals_differ_on_a_dual_rail_cell() {
        // Derivation, entirely from the model. The template is 2 slews x 2
        // loads, and `arc(pin, base)` characterises cell_rise as
        // [[base, base+1], [base+2, base+3]], so the fixture's rise arcs are
        //
        //     D->Q1 = [[ 1,  2], [ 3,  4]]      D->Q2 = [[21, 22], [23, 24]]
        //
        // A reference arc is the middle row and middle column of a table, which
        // for a 2x2 is row 1 and column 1. So each output's own reference delay
        // row is
        //
        //     ref(Q1) = [ 3,  4]                ref(Q2) = [23, 24]
        //
        // and the cell-wide mean, which is what `pooled` hands to both outputs,
        // is their elementwise mean
        //
        //     mean = ([3,4] + [23,24]) / 2 = [13, 14]
        //
        // The setup constraint is the mean of the arcs the source drives,
        // sampled at the reference column, minus the reference delay there. D
        // drives both rails, so its driven mean is the cell-wide mean under
        // either mode -- 14 -- and the constraint is the same in both:
        //
        //     mean arc = ([[1,2],[3,4]] + [[21,22],[23,24]]) / 2
        //              = [[11,12],[13,14]]
        //     column 1 = [12, 14]
        //     setup(D) = [12, 14] - 14 = [-2, 0]
        //
        // Everything below therefore isolates the delay alone. The
        // reconstruction is recon[r][c] = setup[r] + delay[c] and the residual
        // is recon - original.
        let rise_error = |mode: ReferenceMode, output: &str| {
            let mut lib = dual_rail_lib();
            let reports = process_library(
                &mut lib[0],
                &opts(
                    "G",
                    &Regex::new("(R|S)N?").unwrap(),
                    false,
                    mode,
                    WhenMerge::Mean,
                ),
            );
            let report = reports.cells.iter().find(|r| r.cell == "DUT").unwrap();
            report
                .arcs
                .iter()
                .find(|a| a.edge == "rise" && a.source == "D" && a.output == output)
                .unwrap_or_else(|| panic!("no rise arc D -> {} in {:?}", output, mode))
                .error
                .clone()
        };

        // Per-output gives each rail its own reference row. These arcs are
        // separable by construction -- arc[r][c] = base + 2r + c -- so setup
        // plus the rail's own delay reproduces it exactly:
        //   Q1: [-2,0] + [3,4]   = [[ 1, 2], [ 3, 4]] - [[ 1, 2], [ 3, 4]] = 0
        //   Q2: [-2,0] + [23,24] = [[21,22], [23,24]] - [[21,22], [23,24]] = 0
        let zero = Array2::zeros((2, 2));
        assert_eq!(
            rise_error(ReferenceMode::PerOutput, "Q1"),
            zero,
            "an arc reconstructed against its own reference must close exactly"
        );
        assert_eq!(rise_error(ReferenceMode::PerOutput, "Q2"), zero);

        // Pooled charges both rails the cell-wide mean instead, so the residual
        // is that mean's distance from the rail's own delay at every point:
        //   Q1: [-2,0] + [13,14] = [[11,12],[13,14]] - [[ 1, 2],[ 3, 4]] = +10
        //   Q2: [-2,0] + [13,14] = [[11,12],[13,14]] - [[21,22],[23,24]] = -10
        // i.e. mean - own = 13-3 = 14-4 = +10 and 13-23 = 14-24 = -10.
        assert_eq!(
            rise_error(ReferenceMode::Pooled, "Q1"),
            Array2::from_elem((2, 2), 10.0),
            "pooling must overcharge the fast rail by mean - own"
        );
        assert_eq!(
            rise_error(ReferenceMode::Pooled, "Q2"),
            Array2::from_elem((2, 2), -10.0),
            "and undercharge the slow rail by the same amount"
        );
    }

    /// The reported reconstruction really is setup + clock-to-output delay added
    /// back together, and the residual really is its distance from the original.
    ///
    /// Killed by: `collect_arc_errors` recorded `reconstructed + original` as the residual instead of `reconstructed - original`.
    #[test]
    fn reported_reconstruction_is_the_outer_sum_of_setup_and_delay() {
        let mut lib = reportable_lib();
        let reports = process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        let report = reports.cells.iter().find(|r| r.cell == "DUT").unwrap();
        assert!(!report.arcs.is_empty(), "fixture must produce arcs");

        for arc in &report.arcs {
            // The fixture is positive unate throughout, so the input's direction is
            // the output's and the edge names both.
            let setup = match arc.edge {
                "rise" => &report.setup_input_rise[&(arc.source.clone(), Scope::Whole)],
                _ => &report.setup_input_fall[&(arc.source.clone(), Scope::Whole)],
            };
            let refarc = &report.ref_arcs[&(arc.output.clone(), Scope::Whole)];
            let edge = match arc.edge {
                "rise" => refarc.rise.as_ref(),
                _ => refarc.fall.as_ref(),
            };
            let delay = &edge.expect("the fixture draws both edges").delay;

            assert_eq!(arc.reconstructed, restore_arc(setup, delay), "{:?}", arc);
            assert_eq!(
                arc.error,
                arc.reconstructed.clone() - &arc.original,
                "{:?}",
                arc
            );
            // Exact at the reference column, but only because this fixture
            // characterises each arc once. The setup is derived from the mean
            // over a pin pair's `when` conditions, so it can only reproduce a
            // raw condition exactly when there is nothing to average -- see
            // when_averaging_error_shows_up_against_the_raw_conditions.
            assert!(arc.when.is_none(), "fixture must be unconditioned");
            let col = refarc.col;
            for r in 0..arc.error.nrows() {
                assert!(
                    arc.error[[r, col]].abs() < 1e-9,
                    "residual must vanish on the reference column, got {}",
                    arc.error[[r, col]]
                );
            }
        }
    }

    /// Measuring against the raw conditions is what makes the `when` reduction
    /// visible: the model holds one setup per pin, so it cannot sit exactly on
    /// two conditions that disagree. Measuring against the mean would hide
    /// precisely the error the mean introduced.
    ///
    /// Killed by: `process_cell` recorded every `ConditionedArc` with `when: None`.
    #[test]
    fn when_averaging_error_shows_up_against_the_raw_conditions() {
        // Two conditions for the same pin pair, deliberately far apart.
        let body = format!(
            r#"
    pin(D) {{ direction: input; }}
    pin(Q) {{
      direction: output;
      function: "IQ";
      {}
      {}
    }}"#,
            conditioned_arc("D", 1.0, "(A * B)"),
            conditioned_arc("D", 100.0, "(A * !B)")
        );

        let mut lib = bundle_lib(body);
        let reports = process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        let report = reports.cells.iter().find(|r| r.cell == "DUT").unwrap();

        // Both conditions are measured, and each is labelled by its own `when`.
        let rise: Vec<&ArcError> = report
            .arcs
            .iter()
            .filter(|a| a.edge == "rise" && a.source == "D")
            .collect();
        assert_eq!(rise.len(), 2, "each condition must be measured separately");
        let mut whens: Vec<&str> = rise.iter().filter_map(|a| a.when.as_deref()).collect();
        whens.sort();
        assert_eq!(whens, vec!["(A * !B)", "(A * B)"]);

        // The raw conditions differ, so one setup cannot satisfy both: at least
        // one must carry a residual the averaged comparison would have hidden.
        let worst = rise.iter().fold(0.0_f64, |m, a| m.max(a.max_abs()));
        assert!(
            worst > 1.0,
            "averaging two disagreeing conditions must leave a residual, got {}",
            worst
        );

        // And the residual is genuinely against the raw table, not the mean.
        for a in &rise {
            assert_eq!(a.error, a.reconstructed.clone() - &a.original);
        }
    }
}
