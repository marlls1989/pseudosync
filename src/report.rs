//! What the conversion cost: the arcs as characterised, the arcs rebuilt from
//! the model, and the residual between them.

use crate::arcs::{restore_arc, RefArc, References, WhenMerge};
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
    pub(crate) timing_type: Option<String>,
    pub(crate) timing_sense: Option<String>,
    pub(crate) cell_rise: Option<Array2<f64>>,
    pub(crate) cell_fall: Option<Array2<f64>>,
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
    pub(crate) timing_type: Option<String>,
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
    /// The same arcs after the `when` conditions were folded together; this is
    /// what the constraints were actually derived from.
    pub(crate) cell_rise_arcs: BTreeMap<(String, String), Array2<f64>>,
    pub(crate) cell_fall_arcs: BTreeMap<(String, String), Array2<f64>>,
    /// Reference arc chosen per output.
    pub(crate) ref_arcs: BTreeMap<String, RefArc>,
    /// Reference the constraints were taken against.
    pub(crate) mean_ref: RefArc,
    pub(crate) setup_rise: BTreeMap<String, Array1<f64>>,
    pub(crate) setup_fall: BTreeMap<String, Array1<f64>>,
    pub(crate) arcs: Vec<ArcError>,
}

/// One half of an arc: which raw table it reads, and which component of a
/// reference arc pairs with it. Bundled so the two can never be mismatched.
pub(crate) struct Edge {
    name: &'static str,
    raw: fn(&ConditionedArc) -> Option<&Array2<f64>>,
    reference: fn(&RefArc) -> &Array1<f64>,
}

pub(crate) const RISE: Edge = Edge {
    name: "rise",
    raw: |a| a.cell_rise.as_ref(),
    reference: |r| &r.cell_rise,
};

pub(crate) const FALL: Edge = Edge {
    name: "fall",
    raw: |a| a.cell_fall.as_ref(),
    reference: |r| &r.cell_fall,
};

/// Measure the reconstruction residual against every condition of every arc.
///
/// The model carries one setup constraint per pin and one delay per output, so
/// each raw condition is reconstructed from that same pair. The residual it
/// leaves therefore contains both error sources at once: what the separable
/// setup-plus-delay form cannot express, and what collapsing the `when`
/// conditions into one representative arc threw away.
pub(crate) fn collect_arc_errors(
    arcs: &[ConditionedArc],
    setup: &BTreeMap<String, Array1<f64>>,
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
        let Some(slew_dependent) = setup.get(source) else {
            continue;
        };
        // Reconstruct with the delay this mode actually emits, so the residual
        // describes the library that ships rather than an idealised split. The
        // original report predated pooling and always used the per-output arc.
        let Some(delay_ref) = references.delay_for(output) else {
            continue;
        };

        let reconstructed = restore_arc(slew_dependent, (edge.reference)(delay_ref));
        if reconstructed.raw_dim() != original.raw_dim() {
            continue;
        }

        out.push(ArcError {
            cell: cell_name.to_owned(),
            source: source.clone(),
            output: output.clone(),
            edge: edge.name,
            when: arc.when.clone(),
            timing_type: arc.timing_type.clone(),
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
    use crate::arcs::{restore_arc, ReferenceMode, WhenMerge};
    use crate::engine::process_library; // Test-only; a unit test observes its subject through the real engine path rather than a stub.
    use liberty_parser::liberty::Liberty;
    use regex::Regex;

    /// Four timing tables for one arc, so `select_reference_arc` accepts it.
    fn arc(related_pin: &str, base: f64) -> String {
        format!(
            r#"
        timing() {{
          related_pin: "{}";
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
            timing_type: None,
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
            let reports = process_library(&mut lib[0], "G", &reset, false, mode, WhenMerge::Mean);

            let report = reports
                .iter()
                .find(|r| r.cell == "DUT")
                .unwrap_or_else(|| panic!("no report for the processed cell in {:?}", mode));

            assert_eq!(report.library, "bundle_test");
            assert!(
                !report.cell_rise_arcs.is_empty(),
                "the original arcs must be reported, not just the residual"
            );
            assert!(!report.ref_arcs.is_empty(), "{:?}", mode);
            assert!(!report.setup_rise.is_empty(), "{:?}", mode);
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
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                mode,
                WhenMerge::Mean,
            );
            let report = reports.iter().find(|r| r.cell == "DUT").unwrap();
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
        );
        let report = reports.iter().find(|r| r.cell == "DUT").unwrap();
        assert!(!report.arcs.is_empty(), "fixture must produce arcs");

        for arc in &report.arcs {
            let setup = match arc.edge {
                "rise" => &report.setup_rise[&arc.source],
                _ => &report.setup_fall[&arc.source],
            };
            let refarc = &report.ref_arcs[&arc.output];
            let delay = match arc.edge {
                "rise" => &refarc.cell_rise,
                _ => &refarc.cell_fall,
            };

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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
        );
        let report = reports.iter().find(|r| r.cell == "DUT").unwrap();

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
