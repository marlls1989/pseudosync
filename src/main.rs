use liberty_parse::{
    self,
    ast::{LibertyAst, Value},
    liberty::{Cell, Group, Liberty, Library, Pin},
};
use ndarray::prelude::*;
use regex::Regex;
use simple_error::simple_error;
use std::{
    collections::HashMap,
    error::Error,
    fs::File,
    io::{stdin, stdout, BufWriter, Read, Write},
    path::{Path, PathBuf},
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

    #[structopt(parse(from_os_str))]
    input: PathBuf,

    #[structopt(parse(from_os_str), short, long)]
    output: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq)]
struct TimingSpec {
    template_name: String,
    capacitance: Array1<f64>,
    slew: Array1<f64>,
    table: Array2<f64>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let opts = ProgramOptions::from_args();

    let mut liberty = parse_liberty_file(&opts.input)?;

    for lib in liberty.iter_mut() {
        process_library(lib, &opts.clock_pin, &opts.reset_pin, opts.latch);
    }

    write_liberty_file(opts.output.as_deref(), &liberty.to_ast())?;

    Ok(())
}

fn parse_liberty_file(path: &Path) -> Result<Liberty, Box<dyn Error>> {
    let mut input_stream: Box<dyn Read> = if path.as_os_str() == "-" {
        Box::new(stdin())
    } else {
        Box::new(File::open(&path)?)
    };

    let mut buf = String::new();
    input_stream.read_to_string(&mut buf)?;
    let lib = liberty_parse::parse_lib(&buf).map_err(|e| simple_error!("{}", e))?;

    Ok(lib)
}

fn write_liberty_file(path: Option<&Path>, liberty: &LibertyAst) -> Result<(), Box<dyn Error>> {
    let mut output_stream = {
        let output: Box<dyn Write> = if let Some(path) = path {
            Box::new(File::create(path)?)
        } else {
            Box::new(stdout())
        };
        BufWriter::new(output)
    };

    writeln!(output_stream, "{}", liberty)?;

    Ok(())
}

/// Tests if the cell contains a latch group and a pin with the expected clock_pin name
fn cell_qualifies(cell: &Cell, clock_name: &str) -> bool {
    cell.groups.iter().any(|group| group.type_ == "latch")
        && cell
            .pins
            .iter()
            .any(|(pin_name, _pin)| pin_name == clock_name)
}

fn is_output_pin(pin: &Pin) -> bool {
    pin.simple_attributes
        .get("direction")
        .map(|x| {
            if let Value::String(v) = x {
                v == "output"
            } else {
                false
            }
        })
        .unwrap_or(false)
}

fn mean_timingtable<'a, I>(groups: I) -> Option<Array2<f64>>
where
    I: IntoIterator<Item = &'a Group>,
{
    let mut n = 0.0;
    groups
        .into_iter()
        .map(|g| {
            n += 1.0;
            let ref v = g.complex_attributes["values"];
            let m: Vec<f64> = v
                .iter()
                .flat_map(|v| match v {
                    Value::FloatGroup(x) => x.clone(),
                    Value::Float(x) => vec![*x],
                    _ => panic!("characterisation table must comprise only numeric values"),
                })
                .collect();
            Array2::from_shape_vec((v.len(), m.len() / v.len()), m).unwrap()
        })
        .reduce(|a, b| (a + b))
        .map(|x| x / n)
}

fn process_library(lib: &mut Library, clock_name: &str, reset_name: &Regex, latch: bool) {
    struct RefArc {
        col: usize,
        line: usize,
        rise_trans: Array1<f64>,
        fall_trans: Array1<f64>,
        cell_rise: Array1<f64>,
        cell_fall: Array1<f64>,
    }
    //let lut_template = HashMap::new();

    for (_cell_name, cell) in lib
        .cells
        .iter_mut()
        .filter(|(_, x)| cell_qualifies(x, clock_name))
    {
        //let setup_constraint = HashMap::new();
        for (outpin_name, outpin) in cell.pins.iter_mut().filter(|(_, pin)| is_output_pin(pin)) {
            let mut ref_arc: Option<RefArc> = None;

            let mut cell_rise_arcs: HashMap<String, Array2<f64>> = HashMap::new();
            let mut cell_fall_arcs: HashMap<String, Array2<f64>> = HashMap::new();
            let mut rise_trans_arcs: HashMap<String, Array2<f64>> = HashMap::new();
            let mut fall_trans_arcs: HashMap<String, Array2<f64>> = HashMap::new();

            for timing_group in outpin.groups.iter().filter(|g| g.type_ == "timing") {
                let related_pin = timing_group.simple_attributes["related_pin"].string();

                // If related_pin is a reset pin, then arcs are preserved.
                if reset_name.is_match(&related_pin) {
                    continue;
                }

                let (cell_rise, others): (Vec<&Group>, Vec<&Group>) = timing_group
                    .groups
                    .iter()
                    .partition(|g| g.type_ == "cell_rise");
                let cell_rise = mean_timingtable(cell_rise);

                let (cell_fall, others): (Vec<&Group>, Vec<&Group>) =
                    others.into_iter().partition(|g| g.type_ == "cell_fall");
                let cell_fall = mean_timingtable(cell_fall);

                let (rise_trans, others): (Vec<&Group>, Vec<&Group>) = others
                    .into_iter()
                    .partition(|g| g.type_ == "rise_transition");
                let rise_trans = mean_timingtable(rise_trans);

                let fall_trans =
                    mean_timingtable(others.into_iter().filter(|g| g.type_ == "fall_transition"));

                if let (
                    Some(cell_rise),
                    Some(cell_fall),
                    Some(rise_trans),
                    Some(fall_trans),
                    None,
                ) = (&cell_rise, &cell_fall, &rise_trans, &fall_trans, &ref_arc)
                {
                    let col = cell_rise.len_of(Axis(1)) / 2;
                    let line = cell_rise.len_of(Axis(0)) / 2;
                    ref_arc = Some(RefArc {
                        col,
                        line,
                        cell_fall: cell_fall.slice(s![col, ..]).to_owned(),
                        cell_rise: cell_rise.slice(s![col, ..]).to_owned(),
                        rise_trans: rise_trans.slice(s![col, ..]).to_owned(),
                        fall_trans: fall_trans.slice(s![col, ..]).to_owned(),
                    });
                }

                if let Some(cell_rise) = cell_rise {
                    cell_rise_arcs.insert(related_pin.clone(), cell_rise);
                }

                if let Some(cell_fall) = cell_fall {
                    cell_fall_arcs.insert(related_pin.clone(), cell_fall);
                }

                if let Some(fall_trans) = fall_trans {
                    fall_trans_arcs.insert(related_pin.clone(), fall_trans);
                }

                if let Some(rise_trans) = rise_trans {
                    rise_trans_arcs.insert(related_pin.clone(), rise_trans);
                }
            }
        }
    }
}
