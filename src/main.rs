use gpoint::GPoint;
use itertools::Itertools;
use ndarray::prelude::*;
use pseudosync::{
    parse_liberty_file, process_library_with_reference, write_liberty_file, ArcError, CellReport,
    ReferenceMode, WhenMerge,
};
use regex::Regex;
use std::{
    error::Error,
    fs::File,
    io::{stderr, BufWriter, Write},
    path::PathBuf,
};
use structopt::StructOpt;

#[cfg(test)]
mod render_tests;

#[derive(Debug, StructOpt)]
struct ProgramOptions {
    #[structopt(short, long)]
    latch: bool,

    #[structopt(short, long, default_value = "G")]
    clock_pin: String,

    #[structopt(short, long, default_value = "(R|S)N?")]
    reset_pin: Regex,

    /// How the clock-to-output reference is drawn on a multi-output cell:
    /// "pooled" gives every output the cell-wide mean, "per-output" gives each
    /// output its own and references each input against the outputs it drives.
    #[structopt(short = "m", long, default_value = "per-output")]
    reference_mode: ReferenceMode,

    /// How the several `when`-conditioned arcs of a pin pair are merged into the
    /// one arc the flop model carries: "mean" is representative, "max" is the
    /// pessimistic envelope, "min" the optimistic one. Merging is elementwise,
    /// per slew/load point.
    #[structopt(short = "w", long, default_value = "mean")]
    when_merge: WhenMerge,

    /// Write the reconstruction report here. It dumps each original arc, the
    /// reference and setup arcs it was split into, the arc rebuilt from that
    /// split, and the residual between them. "-" writes it to stderr.
    #[structopt(parse(from_os_str), short = "R", long)]
    report: Option<PathBuf>,

    /// Limit the report to the per-arc and per-cell error statistics, leaving
    /// out the full tables.
    #[structopt(long)]
    report_summary_only: bool,

    #[structopt(parse(from_os_str))]
    input: PathBuf,

    #[structopt(parse(from_os_str), short, long)]
    output: Option<PathBuf>,
}

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
            let conditions: Vec<&pseudosync::ConditionedArc> = r
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

fn write_report(
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

fn main() -> Result<(), Box<dyn Error>> {
    let opts = ProgramOptions::from_args();

    eprintln!("Parsing liberty file");
    let mut liberty = parse_liberty_file(&opts.input)?;

    let mut reports: Vec<CellReport> = Vec::new();
    for lib in liberty.iter_mut() {
        reports.extend(process_library_with_reference(
            lib,
            &opts.clock_pin,
            &opts.reset_pin,
            opts.latch,
            opts.reference_mode,
            opts.when_merge,
        ));
    }

    if let Some(path) = opts.report.as_deref() {
        if path.as_os_str() == "-" {
            write_report(
                &mut stderr(),
                &reports,
                opts.reference_mode,
                opts.report_summary_only,
            )?;
        } else {
            // Truncates: appending across runs, as the original did, makes it
            // impossible to tell which run a table came from.
            let mut sink = BufWriter::new(File::create(path)?);
            write_report(
                &mut sink,
                &reports,
                opts.reference_mode,
                opts.report_summary_only,
            )?;
            sink.flush()?;
            eprintln!("Wrote reconstruction report to {}", path.display());
        }
    }

    eprintln!("Writing liberty file");
    write_liberty_file(opts.output.as_deref(), &liberty.to_ast())?;

    Ok(())
}
