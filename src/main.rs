mod arcs;
mod conditions;
mod emit;
mod engine;
mod liberty_io;
mod pins;
mod render;
mod report;
mod templates;

use crate::arcs::{Anchor, OffsetPlacement, ReferenceMode, WhenMerge};
use crate::engine::{process_library, CellOptions};
use crate::liberty_io::{parse_liberty_file, write_liberty_file, Destination};
use crate::render::write_report;
use crate::report::{CellReport, Refusal};
use crate::templates::undeclared_table_templates;
use regex::Regex;
use std::{error::Error, io::Write, path::PathBuf};
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

    /// Where in each characterised table the value standing for the collapsed
    /// axis is read: "middle" takes the middle row, column and element, so every
    /// number emitted is one the library measured; "average" takes the mean over
    /// that axis instead.
    #[structopt(long, default_value = "middle")]
    anchor: Anchor,

    /// Which half of the split carries the constant the two are separated
    /// around: "setup" leaves it in the setup constraint, "prop" folds it into
    /// the clock-to-output delay. The two halves sum to the same arc either way.
    #[structopt(long, default_value = "setup")]
    offset_placement: OffsetPlacement,

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
/// Both destinations are resolved before anything is parsed or written. The two
/// artefacts may not share one, and refusing that up front is the only place it can
/// be refused harmlessly -- once the library has been written, discovering that the
/// report is about to be written over it is too late, because the product is already
/// gone.
///
/// The library is written before the report, and the order is load-bearing. The
/// library is the product -- it goes into timing closure -- while the report is a
/// diagnostic about how it was built. Were the report written first, a report path
/// that could not be opened -- a mistyped directory being the usual way -- would
/// discard the whole run after every expensive step had already succeeded. In this
/// order a report that cannot be written is still an error and still an exit code,
/// but it does not take the library down with it.
fn run(opts: &ProgramOptions) -> Result<(), Box<dyn Error>> {
    let library_destination = Destination::for_output(opts.output.as_deref());
    let report_destination = opts.report.as_deref().map(Destination::for_report);

    if let (Some(report), Some(path)) = (&report_destination, opts.report.as_deref()) {
        if library_destination.collides_with(report) {
            return Err(format!(
                "--output and --report name the same destination: {}",
                path.display()
            )
            .into());
        }
    }

    eprintln!("Parsing liberty file");
    let mut liberty = parse_liberty_file(&opts.input)?;

    // A candidate cell naming a lookup template the library does not declare makes the
    // library broken: the conversion has nothing to derive its pseudo template pair
    // from, and would emit a cell referencing templates that are not there. Refused
    // here, before any conversion, so a broken library never yields a partial product.
    // Such a file reads and parses without complaint, which is why this is a reason of
    // its own for exit 1 rather than a parse failure.
    for lib in liberty.iter() {
        let undeclared = undeclared_table_templates(lib, &opts.clock_pin);
        if !undeclared.is_empty() {
            return Err(format!(
                "broken library {}: references lookup templates it does not declare: {}",
                lib.name,
                undeclared.join(", ")
            )
            .into());
        }
    }

    let cell_options = CellOptions {
        clock_name: &opts.clock_pin,
        reset_name: &opts.reset_pin,
        latch: opts.latch,
        mode: opts.reference_mode,
        when_merge: opts.when_merge,
        anchor: opts.anchor,
        placement: opts.offset_placement,
    };

    let mut reports: Vec<CellReport> = Vec::new();
    let mut refusals: Vec<Refusal> = Vec::new();
    for lib in liberty.iter_mut() {
        let produced = process_library(lib, &cell_options);
        reports.extend(produced.cells);
        refusals.extend(produced.refusals);
    }

    eprintln!("Writing liberty file");
    write_liberty_file(&library_destination, &liberty.to_ast())?;

    if let Some(destination) = &report_destination {
        let mut sink = destination.open()?;
        write_report(
            &mut sink,
            &reports,
            &refusals,
            opts.reference_mode,
            opts.anchor,
            opts.offset_placement,
            opts.report_summary_only,
        )?;
        // The flush is not optional. A buffered sink writes nothing to its backing
        // store until the buffer fills, and the flush it performs when it drops
        // throws its error away -- so without this, a report small enough to fit the
        // buffer would be reported as written when the disk had refused every byte.
        sink.flush()?;

        if let Destination::File(path) = destination {
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
        timing_sense : positive_unate;
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
            anchor: Anchor::Middle,
            offset_placement: OffsetPlacement::Setup,
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

    /// A candidate cell whose table names a lookup template the library never declares.
    /// `T` is declared and deliberately unused, so the library is not simply empty of
    /// templates -- what is missing is the one the conversion would have to read.
    const UNDECLARED_LIB: &str = r#"
library(undeclared_run_test) {
  lu_table_template(T) {
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }
  cell(CANDIDATE) {
    latch(IQ, IQN) { data_in: "A"; enable: "G"; }
    pin(G) { direction: input; clock: true; }
    pin(A) { direction: input; }
    pin(Q) {
      direction: output;
      function: "IQ";
      timing() {
        related_pin: "A";
        timing_sense : positive_unate;
        timing_type: combinational;
        cell_rise(MISSING) { values("1.0, 2.0", "3.0, 4.0"); }
      }
    }
  }
}
"#;

    /// A library naming a lookup template it does not declare is refused before any
    /// artefact is written.
    ///
    /// Such a file reads and parses without complaint, so nothing upstream of the
    /// conversion has cause to object to it -- which is why this is a reason of its own
    /// for a non-zero exit rather than a parse failure. The refusal has to precede the
    /// library write, or a broken input yields a partial product that looks like a
    /// successful one.
    ///
    /// Killed by: the broken-library gate moved to below the library write -- with the
    /// AST cloned there, since `to_ast` consumes the library and the gate needs one to
    /// walk -- so the output file existed by the time the error was returned. Removing
    /// the gate outright reddens this too, but only through the `expect_err`, which
    /// never reaches the file assertion and so would not show that the refusal precedes
    /// the write.
    #[test]
    fn a_library_naming_an_undeclared_template_is_refused_before_anything_is_written() {
        let dir = TempDir::new().expect("create temp dir");
        let input = dir.path().join("in.lib");
        std::fs::write(&input, UNDECLARED_LIB).expect("write fixture");
        let output = dir.path().join("out.lib");

        let err = run(&options(input, output.clone(), None))
            .expect_err("a broken library is not converted");

        assert_eq!(
            err.to_string(),
            "broken library undeclared_run_test: references lookup templates it does not declare: CANDIDATE pin Q: MISSING"
        );
        assert!(
            !output.exists(),
            "a broken library must not yield a partial product"
        );
    }

    /// Killed by: `run`'s collision check moved to below the library write, so the
    /// shared path came back holding the converted library instead of the sentinel.
    /// Removing the check outright reddens this test too, but only through the
    /// `expect_err` above -- it never reaches the sentinel, so it would not have
    /// shown that the refusal precedes the writes, which is what this test claims.
    #[test]
    fn one_path_for_both_artefacts_is_refused_before_either_is_written() {
        let dir = TempDir::new().expect("create temp dir");
        let (input, _) = fixture(&dir);
        let shared = dir.path().join("both.out");

        // A sentinel already in the shared path. Its survival is what proves the
        // refusal happened before anything was opened: an `Err` on its own would
        // also be returned by a run that wrote the library and then truncated it
        // with the report, which is the defect this pins.
        const SENTINEL: &str = "neither artefact may reach this file\n";
        std::fs::write(&shared, SENTINEL).expect("write sentinel");

        let err = run(&options(input, shared.clone(), Some(shared.clone())))
            .expect_err("the two artefacts may not share a destination");

        assert_eq!(
            err.to_string(),
            format!(
                "--output and --report name the same destination: {}",
                shared.display()
            )
        );
        assert_eq!(
            std::fs::read_to_string(&shared).expect("read the sentinel back"),
            SENTINEL,
            "the shared path was written despite the refusal"
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
