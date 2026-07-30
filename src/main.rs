mod arcs;
mod emit;
mod engine;
mod liberty_io;
mod pins;
mod render;
mod report;

use crate::arcs::WhenMerge;
use crate::engine::{process_library, ReferenceMode};
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
    let opts = ProgramOptions::from_args();

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
