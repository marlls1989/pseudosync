use pseudosync::{parse_liberty_file, process_library, write_liberty_file};
use regex::Regex;
use std::{error::Error, path::PathBuf};
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
struct ProgramOptions {
    #[structopt(short, long, default_value = "(R|S)N?")]
    reset_pin: Regex,

    #[structopt(parse(from_os_str))]
    input: PathBuf,

    #[structopt(parse(from_os_str), short, long)]
    output: Option<PathBuf>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let opts = ProgramOptions::from_args();

    eprintln!("Parsing liberty file");
    let mut liberty = parse_liberty_file(&opts.input)?;

    for lib in liberty.iter_mut() {
        process_library(lib, &opts.reset_pin);
    }

    eprintln!("Writing liberty file");
    write_liberty_file(opts.output.as_deref(), &liberty.to_ast())?;

    Ok(())
}
