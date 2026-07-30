//! Rendering of the reconstruction report: the tables, the per-arc statistics
//! and the sections they are laid out in.

use crate::engine::ReferenceMode;
use crate::report::{ArcError, CellReport, ConditionedArc};
use gpoint::GPoint;
use itertools::Itertools;
use ndarray::prelude::*;
use std::{error::Error, io::Write};

fn g(v: f64) -> String {
    format!("{}", GPoint(v))
}

/// Render a table the way the original report did: one prettytable row per row
/// of the array, values in %g. A 1-D arc renders as the single row it is.
fn dump<D: Dimension>(
    sink: &mut dyn Write,
    label: &str,
    a: &ArrayBase<ndarray::OwnedRepr<f64>, D>,
) {
    let _ = writeln!(sink, "{}", label);
    let mut table = prettytable::Table::new();
    for row in a.rows() {
        table.add_row(prettytable::Row::new(
            row.iter().map(|v| prettytable::Cell::new(&g(*v))).collect(),
        ));
    }
    let _ = writeln!(sink, "{}", table);
}

fn dump_cell(sink: &mut dyn Write, r: &CellReport) {
    let _ = writeln!(sink, "cell {} of library {}", r.cell, r.library);

    // The reduced arcs the constraints were derived from. The per-condition
    // originals are printed with their comparisons below.
    for ((src, dst), v) in &r.cell_rise_arcs {
        dump(sink, &format!("mean rise arc {} -> {}:", src, dst), v);
    }
    for ((src, dst), v) in &r.cell_fall_arcs {
        dump(sink, &format!("mean fall arc {} -> {}:", src, dst), v);
    }

    for (out, v) in &r.ref_arcs {
        dump(
            sink,
            &format!("ref rise arc {} -> {} (row {}):", v.related_pin, out, v.row),
            &v.cell_rise,
        );
        dump(
            sink,
            &format!("ref fall arc {} -> {} (row {}):", v.related_pin, out, v.row),
            &v.cell_fall,
        );
    }

    dump(
        sink,
        &format!(
            "mean ref rise arc (col {}, row {}):",
            r.mean_ref.col, r.mean_ref.row
        ),
        &r.mean_ref.cell_rise,
    );
    dump(sink, "mean ref fall arc:", &r.mean_ref.cell_fall);

    for (k, v) in &r.setup_rise {
        dump(sink, &format!("setup rise arc {}:", k), v);
    }
    for (k, v) in &r.setup_fall {
        dump(sink, &format!("setup fall arc {}:", k), v);
    }

    for a in &r.arcs {
        let condition = match (&a.timing_type, &a.when) {
            (Some(t), Some(w)) => format!("  [{}] when {}", t, w),
            (Some(t), None) => format!("  [{}] unconditioned", t),
            (None, Some(w)) => format!("  when {}", w),
            (None, None) => "  unconditioned".to_owned(),
        };
        let head = format!("{} arc {} -> {}{}", a.edge, a.source, a.output, condition);

        dump(sink, &format!("{}\noriginal:", head), &a.original);
        dump(sink, "reconstructed:", &a.reconstructed);
        dump(sink, "error:", &a.error);
        let _ = writeln!(sink, "{}\n", stat_line(a));
    }
}

/// Statistics of an arbitrary residual, in the same shape as `stat_line`.
fn stats_of(err: &Array2<f64>, reference: &Array2<f64>) -> String {
    let n = err.len();
    let mean = err.iter().sum::<f64>() / n as f64;
    let sd = (err.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
    let rms = (err.iter().map(|v| v * v).sum::<f64>() / n as f64).sqrt();
    let scale = reference.iter().map(|v| v.abs()).sum::<f64>() / n as f64;
    let (min, max) = err
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    format!(
        "stats: n {}  scale {}  bias {}  sd {}  rms {}  min {}  max {}  rms/scale {:.2}%",
        n,
        g(scale),
        g(mean),
        g(sd),
        g(rms),
        g(min),
        g(max),
        if scale == 0.0 {
            0.0
        } else {
            100.0 * rms / scale
        }
    )
}

/// What the `when` reduction alone costs, before the pseudo-flop split.
///
/// Each pin pair is characterised once per operating state and the engine keeps
/// one mean of them. This measures that mean against each state it stands in
/// for, so the reduction's error can be read separately from the error of the
/// separable setup-plus-delay form that consumes it.
fn dump_reduction(sink: &mut dyn Write, r: &CellReport) {
    let _ = writeln!(
        sink,
        "== when-reduction error, cell {} of library {} ==",
        r.cell, r.library
    );
    let _ = writeln!(
        sink,
        "mean arc measured against each condition it replaces, no split involved\n"
    );

    for (edge, means) in [("rise", &r.cell_rise_arcs), ("fall", &r.cell_fall_arcs)] {
        for ((source, output), mean) in means.iter() {
            let conditions: Vec<&ConditionedArc> = r
                .raw_arcs
                .iter()
                .filter(|a| &a.source == source && &a.output == output)
                .filter(|a| {
                    if edge == "rise" {
                        a.cell_rise.is_some()
                    } else {
                        a.cell_fall.is_some()
                    }
                })
                .collect();

            // Printed so the reduction can be checked without trusting it: for
            // all-positive delay tables the mean's own magnitude must equal the
            // mean of the conditions' magnitudes. If those two disagree, the
            // reduction is not averaging what it claims to be averaging.
            let scale_of =
                |t: &Array2<f64>| t.iter().map(|v| v.abs()).sum::<f64>() / t.len() as f64;
            let condition_scales: Vec<f64> = conditions
                .iter()
                .map(|c| {
                    scale_of(if edge == "rise" {
                        c.cell_rise.as_ref().unwrap()
                    } else {
                        c.cell_fall.as_ref().unwrap()
                    })
                })
                .collect();
            let mean_of_scales =
                condition_scales.iter().sum::<f64>() / condition_scales.len().max(1) as f64;

            let _ = writeln!(
                sink,
                "{} arc {} -> {}: mean of {} condition(s)  |  mean scale {}  mean-of-condition scales {}",
                edge,
                source,
                output,
                conditions.len(),
                g(scale_of(mean)),
                g(mean_of_scales)
            );

            let mut worst: f64 = 0.0;
            for c in &conditions {
                let raw = if edge == "rise" {
                    c.cell_rise.as_ref().unwrap()
                } else {
                    c.cell_fall.as_ref().unwrap()
                };
                if raw.raw_dim() != mean.raw_dim() {
                    continue;
                }
                let err = mean - raw;
                worst = worst.max(err.iter().fold(0.0_f64, |m, v| m.max(v.abs())));
                let _ = writeln!(
                    sink,
                    "  {:<44} {}",
                    match (&c.timing_type, &c.when) {
                        (Some(t), Some(w)) => format!("[{}] {}", t, w),
                        (Some(t), None) => format!("[{}] unconditioned", t),
                        (None, Some(w)) => w.clone(),
                        (None, None) => "unconditioned".to_owned(),
                    },
                    stats_of(&err, raw)
                );
            }
            // What is actually being pooled under this one key. Two arcs on the
            // same edge between the same pins are still different arcs if their
            // sense or type differ, and the mean of those describes neither.
            let mut kinds: std::collections::BTreeMap<String, (usize, f64, f64)> =
                std::collections::BTreeMap::new();
            for c in &conditions {
                let raw = if edge == "rise" {
                    c.cell_rise.as_ref().unwrap()
                } else {
                    c.cell_fall.as_ref().unwrap()
                };
                let key = format!(
                    "{} / {}",
                    c.timing_sense.as_deref().unwrap_or("-"),
                    c.timing_type.as_deref().unwrap_or("-")
                );
                let mag = raw.iter().map(|v| v.abs()).sum::<f64>() / raw.len() as f64;
                let neg = raw.iter().filter(|v| **v < 0.0).count() as f64;
                let e = kinds.entry(key).or_insert((0, 0.0, 0.0));
                e.0 += 1;
                e.1 += mag;
                e.2 += neg;
            }
            let spread = condition_scales
                .iter()
                .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
            if spread.0 > 0.0 && spread.1 / spread.0 > 2.0 {
                let _ = writeln!(
                    sink,
                    "  WIDE SPREAD: conditions range {} .. {} ({:.1}x) -- the merged arc \n               represents no single operating state",
                    g(spread.0),
                    g(spread.1),
                    spread.1 / spread.0
                );
            }
            // Liberty's combinational / combinational_dir split is a grouping
            // artifact; the arcs it contains are what matter. Listed only so the
            // spread above can be traced back to states.
            let _ = writeln!(sink, "  conditions by declared kind:");
            for (kind, (n, mag, neg)) in &kinds {
                let _ = writeln!(
                    sink,
                    "    {:<42} x{:<3} |mean| {:>10}  neg/table {:.0}",
                    kind,
                    n,
                    g(mag / *n as f64),
                    neg / *n as f64
                );
            }
            let _ = writeln!(sink, "  worst |mean - condition|: {}\n", g(worst));
        }
    }
}

/// One-line statistical summary of a single comparison.
fn stat_line(a: &ArcError) -> String {
    let n = a.error.len();
    let mean = a.bias();
    // Spread about the mean, distinct from the rms about zero: a large rms with
    // a small sd is a systematic offset, the reverse is scatter.
    let sd = (a.error.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
    let (min, max) = a
        .error
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    format!(
        "stats: n {}  scale {}  bias {}  sd {}  rms {}  min {}  max {}  |max| {}  rms/scale {:.2}%",
        n,
        g(a.scale()),
        g(mean),
        g(sd),
        g(a.rms()),
        g(min),
        g(max),
        g(a.max_abs()),
        a.rms_percent()
    )
}

fn write_summary(sink: &mut dyn Write, arcs: &[&ArcError]) -> Result<(), Box<dyn Error>> {
    let mut table = prettytable::Table::new();
    table.add_row(prettytable::row![
        "cell",
        "arc",
        "edge",
        "condition",
        "scale",
        "bias",
        "rms",
        "|max|",
        "rms/scale"
    ]);
    for a in arcs {
        table.add_row(prettytable::row![
            a.cell,
            format!("{} -> {}", a.source, a.output),
            a.edge,
            a.when.as_deref().unwrap_or("-"),
            g(a.scale()),
            g(a.bias()),
            g(a.rms()),
            g(a.max_abs()),
            format!("{:.2}%", a.rms_percent()),
        ]);
    }
    writeln!(sink, "{}", table)?;

    // Quadratic quantities combine as means of squares, not means of roots.
    let rollup = |name: &str, sel: &dyn Fn(&ArcError) -> bool, sink: &mut dyn Write| {
        let chosen: Vec<&&ArcError> = arcs.iter().filter(|a| sel(a)).collect();
        if chosen.is_empty() {
            return Ok(());
        }
        let n = chosen.len() as f64;
        let rms = (chosen.iter().map(|a| a.rms() * a.rms()).sum::<f64>() / n).sqrt();
        let scale = chosen.iter().map(|a| a.scale()).sum::<f64>() / n;
        let bias = chosen.iter().map(|a| a.bias()).sum::<f64>() / n;
        let worst = chosen.iter().fold(0.0_f64, |m, a| m.max(a.max_abs()));
        let worst_rel = chosen.iter().fold(0.0_f64, |m, a| m.max(a.rms_percent()));
        writeln!(
            sink,
            "{:<26} arcs {:>3}  bias {:>10}  rms {:>10}  |max| {:>10}  rms/scale {:>7.2}%  worst arc {:>7.2}%",
            name,
            chosen.len(),
            g(bias),
            g(rms),
            g(worst),
            if scale == 0.0 { 0.0 } else { 100.0 * rms / scale },
            worst_rel
        )
    };

    // `dedup` would only drop *adjacent* repeats, and the reports accumulate
    // across every library in the file, so one cell defined in two libraries
    // comes back as two non-adjacent runs and would be rolled up twice. Order is
    // first appearance, not sorted: the rollup lines are compared between runs.
    let cells: Vec<&str> = arcs.iter().map(|a| a.cell.as_str()).unique().collect();
    for cell in cells {
        rollup(cell, &|a: &ArcError| a.cell == cell, sink)?;
    }
    rollup("ALL", &|_: &ArcError| true, sink)?;
    Ok(())
}

pub(crate) fn write_report(
    sink: &mut dyn Write,
    reports: &[CellReport],
    mode: ReferenceMode,
    summary_only: bool,
) -> Result<(), Box<dyn Error>> {
    writeln!(sink, "reference mode: {:?}", mode)?;
    if let Some(r) = reports.first() {
        writeln!(sink, "when-arc merge: {:?}", r.when_merge)?;
    }
    writeln!(
        sink,
        "residual of reconstructing each arc as setup + clock-to-output delay\n"
    )?;

    if !summary_only {
        // The reduction is upstream of the split, so it is reported first.
        for r in reports {
            dump_reduction(sink, r);
        }
        for r in reports {
            dump_cell(sink, r);
        }
    }

    let arcs: Vec<&ArcError> = reports.iter().flat_map(|r| r.arcs.iter()).collect();
    write_summary(sink, &arcs)
}

#[cfg(test)]
mod tests {
    //! Behaviour of the report renderers: formatting, statistics and section layout.

    use super::*;
    use crate::arcs::{RefArc, WhenMerge};
    use std::collections::BTreeMap;
    use std::io::Write;

    /// Every renderer takes a `&mut dyn Write`, so a report can be produced into
    /// memory: none of these tests touch the filesystem or the binary.
    fn rendered(render: impl FnOnce(&mut dyn Write)) -> String {
        let mut sink: Vec<u8> = Vec::new();
        render(&mut sink);
        String::from_utf8(sink).expect("the renderers emit utf-8")
    }

    /// The cells of every prettytable row in `out`, whitespace trimmed. Table rows
    /// are the only lines drawn with `|`.
    fn table_rows(out: &str) -> Vec<Vec<&str>> {
        out.lines()
            .filter(|l| l.starts_with('|'))
            .map(|l| l.trim_matches('|').split('|').map(str::trim).collect())
            .collect()
    }

    /// Everything that is not part of a drawn table: the labels and stat lines.
    fn prose_lines(out: &str) -> Vec<&str> {
        out.lines()
            .filter(|l| !l.is_empty() && !l.starts_with('|') && !l.starts_with('+'))
            .collect()
    }

    /// The leading name of each rollup line, in the order they were written.
    fn rollup_names(out: &str) -> Vec<&str> {
        out.lines()
            .filter(|l| !l.starts_with('|') && l.contains(" arcs "))
            .filter_map(|l| l.split_whitespace().next())
            .collect()
    }

    /// The statistics of [`arc_error`], computed by hand from its tables:
    /// error [-7, 1] over an original of magnitude 2. bias (-7+1)/2 = -3;
    /// sd sqrt((16+16)/2) = 4; rms sqrt((49+1)/2) = 5; |max| 7; rms/scale 5/2.
    const ARC_ERROR_STATS: &str =
        "stats: n 2  scale 2  bias -3  sd 4  rms 5  min -7  max 1  |max| 7  rms/scale 250.00%";

    /// A residual whose every statistic is known in advance -- see
    /// [`ARC_ERROR_STATS`].
    fn arc_error(cell: &str) -> ArcError {
        let original = Array2::from_shape_vec((1, 2), vec![2.0, 2.0]).unwrap();
        let error = Array2::from_shape_vec((1, 2), vec![-7.0, 1.0]).unwrap();
        ArcError {
            cell: cell.to_owned(),
            source: "D".to_owned(),
            output: "Q".to_owned(),
            edge: "rise",
            when: Some("(C0)".to_owned()),
            timing_type: Some("combinational".to_owned()),
            reconstructed: &original + &error,
            original,
            error,
        }
    }

    fn ref_arc() -> RefArc {
        RefArc {
            col: 1,
            row: 0,
            related_pin: "G".to_owned(),
            lut_template: "T".to_owned(),
            rise_trans: Array1::from(vec![0.1, 0.2]),
            fall_trans: Array1::from(vec![0.11, 0.21]),
            cell_rise: Array1::from(vec![1.0, 2.0]),
            cell_fall: Array1::from(vec![1.5, 2.5]),
        }
    }

    /// A report for one pin pair characterised under several `when` conditions,
    /// each a two-point rise table, reduced to their elementwise mean -- what
    /// [`WhenMerge::Mean`] leaves the model holding. The conditions are the knob:
    /// their magnitudes set the spread the reduction has to stand in for.
    fn cell_report(conditions: &[[f64; 2]]) -> CellReport {
        let table = |v: [f64; 2]| Array2::from_shape_vec((1, 2), v.to_vec()).unwrap();
        let n = conditions.len() as f64;
        let mean = [
            conditions.iter().map(|c| c[0]).sum::<f64>() / n,
            conditions.iter().map(|c| c[1]).sum::<f64>() / n,
        ];

        let raw_arcs: Vec<ConditionedArc> = conditions
            .iter()
            .enumerate()
            .map(|(i, v)| ConditionedArc {
                source: "D".to_owned(),
                output: "Q".to_owned(),
                when: Some(format!("(C{})", i)),
                timing_type: Some("combinational".to_owned()),
                timing_sense: Some("positive_unate".to_owned()),
                cell_rise: Some(table(*v)),
                cell_fall: None,
            })
            .collect();

        CellReport {
            library: "testlib".to_owned(),
            cell: "DUT".to_owned(),
            when_merge: WhenMerge::Mean,
            raw_arcs,
            cell_rise_arcs: BTreeMap::from([(("D".to_owned(), "Q".to_owned()), table(mean))]),
            cell_fall_arcs: BTreeMap::new(),
            ref_arcs: BTreeMap::from([("Q".to_owned(), ref_arc())]),
            mean_ref: ref_arc(),
            setup_rise: BTreeMap::from([("D".to_owned(), Array1::from(vec![0.5, 0.6]))]),
            setup_fall: BTreeMap::new(),
            arcs: vec![arc_error("DUT")],
        }
    }

    // --- g / dump ----------------------------------------------------------

    /// %g, not Rust's own float Display: six significant digits and no trailing
    /// zeros, which is what keeps the tables readable.
    #[test]
    fn g_renders_floats_in_printf_g_form() {
        assert_eq!(g(4.0), "4");
        assert_eq!(g(1.0 / 3.0), "0.333333");
    }

    #[test]
    fn dump_writes_the_label_then_one_table_row_per_array_row() {
        let a = Array2::from_shape_vec((2, 2), vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        let out = rendered(|s| dump(s, "mean rise arc D -> Q:", &a));

        assert_eq!(out.lines().next(), Some("mean rise arc D -> Q:"));
        assert_eq!(table_rows(&out), vec![vec!["1", "2"], vec!["3", "4"]]);
    }

    /// A 1-D arc -- a reference or a setup constraint -- is one row, not a column.
    #[test]
    fn dump_renders_a_one_dimensional_array_as_a_single_row() {
        let a = Array1::from(vec![1.0, 2.0, 3.0]);
        let out = rendered(|s| dump(s, "setup rise arc D:", &a));

        assert_eq!(out.lines().next(), Some("setup rise arc D:"));
        assert_eq!(table_rows(&out), vec![vec!["1", "2", "3"]]);
    }

    // --- stats_of / stat_line ----------------------------------------------

    /// err [3, -3, 3, -3] against a reference of magnitude 2: bias 0, sd 3,
    /// rms sqrt(36/4) = 3, min -3, max 3, rms/scale 3/2.
    #[test]
    fn stats_of_reports_the_residuals_own_statistics() {
        let err = Array2::from_shape_vec((1, 4), vec![3.0, -3.0, 3.0, -3.0]).unwrap();
        let reference = Array2::from_shape_vec((1, 4), vec![2.0, 2.0, 2.0, 2.0]).unwrap();

        assert_eq!(
            stats_of(&err, &reference),
            "stats: n 4  scale 2  bias 0  sd 3  rms 3  min -3  max 3  rms/scale 150.00%"
        );
    }

    /// A residual against an all-zero reference has no scale to be a percentage
    /// of, and must read as 0% rather than as a division by zero.
    #[test]
    fn stats_of_reports_zero_percent_when_the_reference_has_no_scale() {
        let err = Array2::from_shape_vec((1, 2), vec![1.0, -1.0]).unwrap();
        let reference = Array2::from_shape_vec((1, 2), vec![0.0, 0.0]).unwrap();

        assert_eq!(
            stats_of(&err, &reference),
            "stats: n 2  scale 0  bias 0  sd 1  rms 1  min -1  max 1  rms/scale 0.00%"
        );
    }

    /// The per-arc line reports the arc's own scale, bias, rms, |max| and relative
    /// rms -- the quantities [`ArcError`] exposes.
    #[test]
    fn stat_line_reports_the_arcs_statistics() {
        assert_eq!(stat_line(&arc_error("DUT")), ARC_ERROR_STATS);
    }

    // --- write_summary -----------------------------------------------------

    /// One Liberty file can hold several libraries and the reports accumulate
    /// across all of them, so the arcs of one cell need not be adjacent. Dropping
    /// only adjacent repeats would roll that cell up twice.
    #[test]
    fn write_summary_rolls_up_each_cell_once_even_when_its_arcs_are_not_adjacent() {
        let first = arc_error("cellA");
        let other = arc_error("cellB");
        let again = arc_error("cellA");
        let arcs: Vec<&ArcError> = vec![&first, &other, &again];

        let out = rendered(|s| write_summary(s, &arcs).unwrap());

        assert_eq!(rollup_names(&out), vec!["cellA", "cellB", "ALL"]);
        let cell_a = out
            .lines()
            .find(|l| l.starts_with("cellA"))
            .expect("a rollup line for cellA");
        assert!(
            cell_a.contains("arcs   2"),
            "both of cellA's arcs must be rolled up together: {}",
            cell_a
        );
    }

    /// The rollup order is first appearance, not alphabetical: sorting would
    /// reorder the report and read as a regression when two runs are compared.
    #[test]
    fn write_summary_keeps_the_cell_rollups_in_first_appearance_order() {
        let last_alphabetically = arc_error("zcell");
        let first_alphabetically = arc_error("acell");
        let arcs: Vec<&ArcError> = vec![&last_alphabetically, &first_alphabetically];

        let out = rendered(|s| write_summary(s, &arcs).unwrap());

        assert_eq!(rollup_names(&out), vec!["zcell", "acell", "ALL"]);
    }

    // --- dump_cell / dump_reduction ----------------------------------------

    /// Every table the cell was reduced to gets a labelled section, and each
    /// measured arc gets its original, reconstruction, residual and statistics.
    #[test]
    fn dump_cell_writes_a_labelled_section_for_every_table_it_holds() {
        let out = rendered(|s| dump_cell(s, &cell_report(&[[9.0, 3.0], [11.0, 17.0]])));

        assert_eq!(
            prose_lines(&out),
            vec![
                "cell DUT of library testlib",
                "mean rise arc D -> Q:",
                "ref rise arc G -> Q (row 0):",
                "ref fall arc G -> Q (row 0):",
                "mean ref rise arc (col 1, row 0):",
                "mean ref fall arc:",
                "setup rise arc D:",
                "rise arc D -> Q  [combinational] when (C0)",
                "original:",
                "reconstructed:",
                "error:",
                ARC_ERROR_STATS,
            ]
        );
    }

    /// The reduction is measured against each condition it replaced, condition by
    /// condition. Conditions [9, 3] and [11, 17] have mean [10, 10], magnitudes 6
    /// and 14, and residuals [1, 7] and [-1, -7]: bias +-4, sd 3, rms 5, worst 7.
    #[test]
    fn dump_reduction_measures_the_mean_against_every_condition_it_replaced() {
        let out = rendered(|s| dump_reduction(s, &cell_report(&[[9.0, 3.0], [11.0, 17.0]])));

        assert!(
            out.starts_with("== when-reduction error, cell DUT of library testlib ==\n"),
            "{}",
            out
        );
        // The mean's own magnitude must equal the mean of the conditions'.
        assert!(
            out.contains(
                "rise arc D -> Q: mean of 2 condition(s)  |  mean scale 10  mean-of-condition scales 10\n"
            ),
            "{}",
            out
        );

        let stats_for = |when: &str| {
            out.lines()
                .find(|l| {
                    l.trim_start()
                        .starts_with(&format!("[combinational] {}", when))
                })
                .unwrap_or_else(|| panic!("no line for condition {}: {}", when, out))
                .to_owned()
        };
        assert!(
            stats_for("(C0)").ends_with(
                "stats: n 2  scale 6  bias 4  sd 3  rms 5  min 1  max 7  rms/scale 83.33%"
            ),
            "{}",
            stats_for("(C0)")
        );
        assert!(
            stats_for("(C1)").ends_with(
                "stats: n 2  scale 14  bias -4  sd 3  rms 5  min -7  max -1  rms/scale 35.71%"
            ),
            "{}",
            stats_for("(C1)")
        );

        assert!(out.contains("  worst |mean - condition|: 7\n"), "{}", out);
    }

    /// The marker fires on the ratio between the widest and narrowest condition,
    /// and only past 2x: at exactly 2x the conditions are still close enough that
    /// the mean stands for something.
    #[test]
    fn dump_reduction_flags_a_wide_spread_only_past_two_times() {
        let at_the_threshold =
            rendered(|s| dump_reduction(s, &cell_report(&[[1.0, 1.0], [2.0, 2.0]])));
        assert!(
            !at_the_threshold.contains("WIDE SPREAD"),
            "2x is not past 2x: {}",
            at_the_threshold
        );

        let past_it = rendered(|s| dump_reduction(s, &cell_report(&[[1.0, 1.0], [4.0, 4.0]])));
        assert!(
            past_it.contains("WIDE SPREAD: conditions range 1 .. 4 (4.0x)"),
            "{}",
            past_it
        );
    }

    // --- write_report ------------------------------------------------------

    /// The summary is what the report always carries; the per-arc and per-cell
    /// tables are what `--report-summary-only` leaves out.
    #[test]
    fn write_report_with_summary_only_omits_the_per_arc_sections() {
        let reports = vec![cell_report(&[[1.0, 1.0], [4.0, 4.0]])];
        let out = rendered(|s| {
            write_report(s, &reports, ReferenceMode::PerOutput, true).unwrap();
        });

        assert!(
            out.starts_with("reference mode: PerOutput\nwhen-arc merge: Mean\n"),
            "{}",
            out
        );
        assert_eq!(rollup_names(&out), vec!["DUT", "ALL"]);
        assert!(
            !out.contains("== when-reduction error"),
            "the reduction section is not part of the summary: {}",
            out
        );
        assert!(
            !out.contains("cell DUT of library testlib"),
            "the per-cell tables are not part of the summary: {}",
            out
        );
    }

    #[test]
    fn write_report_in_full_adds_the_reduction_and_per_cell_sections() {
        let reports = vec![cell_report(&[[1.0, 1.0], [4.0, 4.0]])];
        let out = rendered(|s| {
            write_report(s, &reports, ReferenceMode::PerOutput, false).unwrap();
        });

        assert!(
            out.contains("== when-reduction error, cell DUT of library testlib ==\n"),
            "{}",
            out
        );
        assert!(out.contains("\ncell DUT of library testlib\n"), "{}", out);
    }
}
