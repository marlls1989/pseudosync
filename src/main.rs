mod arcs;
mod emit;
mod engine;
mod liberty_io;
mod pins;
mod render;
mod report;

use crate::arcs::{ReferenceMode, WhenMerge};
use crate::engine::process_library;
use crate::liberty_io::{parse_liberty_file, write_liberty_file};
use crate::render::write_report;
use crate::report::CellReport;
use regex::Regex;
use std::{
    error::Error,
    fs::File,
    io::{stderr, BufWriter, Write},
    path::PathBuf,
};
use structopt::StructOpt;

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

fn main() -> Result<(), Box<dyn Error>> {
    run(&ProgramOptions::from_args())
}

/// One conversion: parse the input, rebuild every library in it, then write the
/// two artefacts out.
///
/// The library is written before the report, and the order is load-bearing. The
/// library is the product -- it goes into timing closure -- while the report is a
/// diagnostic about how it was built. Writing the report first meant that a
/// report path that could not be opened, a mistyped directory being the usual
/// way, discarded the whole run after every expensive step had already
/// succeeded. A report that cannot be written is still an error and still an
/// exit code; what it no longer does is take the library down with it.
fn run(opts: &ProgramOptions) -> Result<(), Box<dyn Error>> {
    eprintln!("Parsing liberty file");
    let mut liberty = parse_liberty_file(&opts.input)?;

    let mut reports: Vec<CellReport> = Vec::new();
    for lib in liberty.iter_mut() {
        reports.extend(process_library(
            lib,
            &opts.clock_pin,
            &opts.reset_pin,
            opts.latch,
            opts.reference_mode,
            opts.when_merge,
        ));
    }

    eprintln!("Writing liberty file");
    write_liberty_file(opts.output.as_deref(), &liberty.to_ast())?;

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

    Ok(())
}

#[cfg(test)]
mod tests {
    //! The order a run writes its two artefacts in, which decides which of them
    //! survives when the other cannot be written.

    use super::*;
    use tempfile::TempDir;

    /// A latch with the one characterised data arc the conversion needs, so a run
    /// over it reaches the report stage with something to report.
    const ORDERING_LIB: &str = r#"
library(ordering_test) {
  lu_table_template(T) {
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }
  cell(ORDER_LATCH) {
    latch(IQ, IQN) { data_in: "A"; enable: "G"; }
    pin(G) { direction: input; clock: true; }
    pin(A) { direction: input; }
    pin(Q) {
      direction: output;
      function: "IQ";
      timing() {
        related_pin: "A";
        timing_type: combinational;
        cell_rise(T) { values("1.0, 2.0", "3.0, 4.0"); }
        cell_fall(T) { values("1.5, 2.5", "3.5, 4.5"); }
        rise_transition(T) { values("0.1, 0.2", "0.3, 0.4"); }
        fall_transition(T) { values("0.11, 0.21", "0.31, 0.41"); }
      }
    }
  }
}
"#;

    /// The defaults the command line would have supplied, with only the three
    /// paths under test varying.
    fn options(input: PathBuf, output: PathBuf, report: Option<PathBuf>) -> ProgramOptions {
        ProgramOptions {
            latch: true,
            clock_pin: "G".to_owned(),
            reset_pin: Regex::new("(R|S)N?").expect("reset pin pattern"),
            reference_mode: ReferenceMode::PerOutput,
            when_merge: WhenMerge::Mean,
            report,
            report_summary_only: false,
            input,
            output: Some(output),
        }
    }

    /// Lay the fixture down and return the input and output paths beside it.
    fn fixture(dir: &TempDir) -> (PathBuf, PathBuf) {
        let input = dir.path().join("in.lib");
        std::fs::write(&input, ORDERING_LIB).expect("write fixture");
        (input, dir.path().join("out.lib"))
    }

    /// Killed by: `run` wrote the report before the library, restoring the ordering the fix removed.
    #[test]
    fn a_report_that_cannot_be_opened_still_leaves_the_library_written() {
        let dir = TempDir::new().expect("create temp dir");
        let (input, output) = fixture(&dir);

        // Under a directory that does not exist, so `File::create` cannot succeed
        // and no run would be right to create the directory on the user's behalf.
        let report = dir.path().join("no-such-dir").join("run.rpt");

        let err = run(&options(input, output.clone(), Some(report.clone())))
            .expect_err("a report that cannot be opened is still an error");

        assert!(!report.exists(), "the report was not written: {}", err);
        // The point of the fix: the library outlives the report's failure, rather
        // than the whole run being discarded once the expensive work is done.
        assert!(
            output.exists(),
            "the library must survive a report that cannot be written: {}",
            err
        );
        let written = parse_liberty_file(&output).expect("reparse the written library");
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].name, "ordering_test");
        assert!(
            written[0].get_cell("ORDER_LATCH").is_some(),
            "the converted cell is in the surviving library"
        );
    }

    /// Killed by: `write_report` opened with `"ref mode: {:?}"` instead of `"reference mode: {:?}"`.
    #[test]
    fn a_report_path_that_can_be_opened_gets_both_artefacts() {
        let dir = TempDir::new().expect("create temp dir");
        let (input, output) = fixture(&dir);
        let report = dir.path().join("run.rpt");

        run(&options(input, output.clone(), Some(report.clone()))).expect("run with a good report");

        assert!(output.exists(), "the library is written");
        // Reordering the writes must not have cost the report: it is still
        // produced, and still carries the header the renderer opens with.
        let text = std::fs::read_to_string(&report).expect("read the report");
        assert!(
            text.starts_with("reference mode: PerOutput\n"),
            "the report is the rendered one: {:?}",
            text.get(..80)
        );
    }
}
