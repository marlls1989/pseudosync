//! Behaviour of the report renderers: formatting, statistics and section layout.

use super::*;
use pseudosync::{ConditionedArc, RefArc};
use std::collections::BTreeMap;

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
        stats_for("(C0)")
            .ends_with("stats: n 2  scale 6  bias 4  sd 3  rms 5  min 1  max 7  rms/scale 83.33%"),
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
    let at_the_threshold = rendered(|s| dump_reduction(s, &cell_report(&[[1.0, 1.0], [2.0, 2.0]])));
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
