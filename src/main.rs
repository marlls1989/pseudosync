use itertools::Itertools;
use liberty_parse::{
    self,
    ast::{LibertyAst, Value},
    liberty::{Cell, Group, Liberty, Library, Pin},
};
use maplit::*;
use ndarray::prelude::*;
use regex::Regex;
use simple_error::simple_error;
use std::{
    collections::{BTreeMap, HashSet},
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
struct RefArc {
    col: usize,
    row: usize,
    lut_template: String,
    rise_trans: Array1<f64>,
    fall_trans: Array1<f64>,
    cell_rise: Array1<f64>,
    cell_fall: Array1<f64>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let opts = ProgramOptions::from_args();

    eprintln!("Parsing liberty file");
    let mut liberty = parse_liberty_file(&opts.input)?;

    for lib in liberty.iter_mut() {
        process_library(lib, &opts.clock_pin, &opts.reset_pin, opts.latch);
    }

    eprintln!("Writing liberty file");
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
        .map(|x| match x {
            Value::String(v) => v == "output",
            Value::Expression(v) => v == "output",
            _ => false,
        })
        .unwrap_or(false)
}

fn is_input_pin(pin: &Pin) -> bool {
    pin.simple_attributes
        .get("direction")
        .map(|x| match x {
            Value::String(v) => v == "input",
            Value::Expression(v) => v == "input",
            _ => false,
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

fn mean_reference_arc<'a, I>(ref_arcs: I) -> Option<RefArc>
where
    I: IntoIterator<Item = RefArc>,
{
    let mut n = 0.0;
    ref_arcs
        .into_iter()
        .map(|x| {
            n += 1.0;
            x
        })
        .reduce(|a, b| {
            assert_eq!(a.col, b.col);
            assert_eq!(a.row, b.row);
            assert_eq!(&a.lut_template, &b.lut_template);
            RefArc {
                col: a.col,
                row: a.row,
                lut_template: a.lut_template,
                rise_trans: a.rise_trans + b.rise_trans,
                fall_trans: a.fall_trans + b.fall_trans,
                cell_rise: a.cell_rise + b.cell_rise,
                cell_fall: a.cell_fall + b.cell_fall,
            }
        })
        .map(|mut x| {
            x.rise_trans /= n;
            x.fall_trans /= n;
            x.cell_fall /= n;
            x.cell_rise /= n;
            x
        })
}

fn restore_arc(slew_dependent: &Array1<f64>, capacitance_dependent: &Array1<f64>) -> Array2<f64> {
    let cap: Array2<f64> =
        Array::ones((slew_dependent.len(), capacitance_dependent.len())) * capacitance_dependent;
    let slw: Array2<f64> =
        Array::ones((capacitance_dependent.len(), slew_dependent.len())) * slew_dependent;

    cap + slw.t()
}

fn process_library(lib: &mut Library, clock_name: &str, reset_name: &Regex, latch: bool) {
    eprintln!("Processing library {}", lib.name);

    let mut lut_templates: HashSet<String> = HashSet::new();

    for (cell_name, cell) in lib
        .cells
        .iter_mut()
        .filter(|(_, x)| cell_qualifies(x, clock_name))
    {
        eprintln!("Processing cell {}", cell_name);

        let mut ref_arcs: BTreeMap<String, RefArc> = BTreeMap::new();

        // Map related_pin to timing table
        let mut cell_rise_arcs: BTreeMap<(String, String), Array2<f64>> = BTreeMap::new();
        let mut cell_fall_arcs: BTreeMap<(String, String), Array2<f64>> = BTreeMap::new();

        // process each output pin of the cell individually
        for (outpin_name, outpin) in cell.pins.iter_mut().filter(|(_, pin)| is_output_pin(pin)) {
            // for each timing group, capture the characterisation timing tables
            // this loop preserves the original liberty file
            for timing_group in outpin.groups.iter().filter(|g| g.type_ == "timing") {
                let related_pin = timing_group.simple_attributes["related_pin"].string();

                // If related_pin is a reset pin, ignore the arc
                if reset_name.is_match(&related_pin) {
                    continue;
                }

                let mut lut_template = None;

                let (cell_rise, others): (Vec<&Group>, Vec<&Group>) = timing_group
                    .groups
                    .iter()
                    .partition(|g| g.type_ == "cell_rise");
                if let (Some(group), None) = (cell_rise.first(), &lut_template) {
                    lut_template = Some(group.name.clone())
                }
                let cell_rise = mean_timingtable(cell_rise);

                let (cell_fall, others): (Vec<&Group>, Vec<&Group>) =
                    others.into_iter().partition(|g| g.type_ == "cell_fall");
                if let (Some(group), None) = (cell_fall.first(), &lut_template) {
                    lut_template = Some(group.name.clone())
                }
                let cell_fall = mean_timingtable(cell_fall);

                let (rise_trans, others): (Vec<&Group>, Vec<&Group>) = others
                    .into_iter()
                    .partition(|g| g.type_ == "rise_transition");
                if let (Some(group), None) = (rise_trans.first(), &lut_template) {
                    lut_template = Some(group.name.clone())
                }
                let rise_trans = mean_timingtable(rise_trans);

                let fall_trans: Vec<&Group> = others
                    .into_iter()
                    .filter(|g| g.type_ == "fall_transition")
                    .collect();
                if let (Some(group), None) = (fall_trans.first(), &lut_template) {
                    lut_template = Some(group.name.clone())
                }
                let fall_trans = mean_timingtable(fall_trans);

                if let (
                    Some(lut_template),
                    Some(cell_rise),
                    Some(cell_fall),
                    Some(rise_trans),
                    Some(fall_trans),
                    None,
                ) = (
                    lut_template,
                    &cell_rise,
                    &cell_fall,
                    &rise_trans,
                    &fall_trans,
                    ref_arcs.get(outpin_name),
                ) {
                    eprintln!(
                        "  Pin {} selected as reference arc for output {}",
                        related_pin, outpin_name
                    );
                    let col = cell_rise.len_of(Axis(1)) / 2;
                    let row = cell_rise.len_of(Axis(0)) / 2;
                    ref_arcs.insert(
                        outpin_name.clone(),
                        RefArc {
                            col,
                            row,
                            lut_template,
                            cell_fall: cell_fall.slice(s![row, ..]).to_owned(),
                            cell_rise: cell_rise.slice(s![row, ..]).to_owned(),
                            rise_trans: rise_trans.slice(s![row, ..]).to_owned(),
                            fall_trans: fall_trans.slice(s![row, ..]).to_owned(),
                        },
                    );
                }

                if let Some(cell_rise) = cell_rise {
                    cell_rise_arcs.insert((related_pin.clone(), outpin_name.clone()), cell_rise);
                }

                if let Some(cell_fall) = cell_fall {
                    cell_fall_arcs.insert((related_pin.clone(), outpin_name.clone()), cell_fall);
                }
            } // timing_group

            if let Some(ref_arc) = ref_arcs.get(outpin_name) {
                // if creating a pseudo_flop model, erase the original arcs and
                if !latch {
                    outpin.groups.retain(|x| {
                        x.type_ != "timing"
                            || reset_name.is_match(&x.simple_attributes["related_pin"].string())
                    });
                }

                outpin.groups.push(Group {
                    type_: "timing".to_owned(),
                    name: "".to_owned(),
                    simple_attributes: btreemap! {
                        "related_pin".to_owned() => Value::String(clock_name.to_owned()),
                        "timing_sense".to_owned() => Value::Expression("non_unate".to_owned()),
                        "timing_type".to_owned() => Value::Expression("rising_edge".to_owned()),
                    },
                    complex_attributes: BTreeMap::new(),
                    groups: vec![
                        Group {
                            type_: "rise_trans".to_owned(),
                            name: format!("{}_pseudo_delay", ref_arc.lut_template),
                            simple_attributes: BTreeMap::new(),
                            complex_attributes: btreemap! {
                                "values".to_owned() =>
                                vec![Value::FloatGroup(
                                    ref_arc.rise_trans.iter().cloned().collect(),
                                )],
                            },
                            groups: vec![],
                        },
                        Group {
                            type_: "fall_trans".to_owned(),
                            name: format!("{}_pseudo_delay", ref_arc.lut_template),
                            simple_attributes: BTreeMap::new(),
                            complex_attributes: btreemap! {
                                "values".to_owned() =>
                                vec![Value::FloatGroup(
                                    ref_arc.fall_trans.iter().cloned().collect(),
                                )],
                            },
                            groups: vec![],
                        },
                        Group {
                            type_: "cell_rise".to_owned(),
                            name: format!("{}_pseudo_delay", ref_arc.lut_template),
                            simple_attributes: BTreeMap::new(),
                            complex_attributes: btreemap! {
                                "values".to_owned() =>
                                vec![Value::FloatGroup(
                                    ref_arc.cell_rise.iter().cloned().collect(),
                                )],
                            },
                            groups: vec![],
                        },
                        Group {
                            type_: "cell_fall".to_owned(),
                            name: format!("{}_pseudo_delay", ref_arc.lut_template),
                            simple_attributes: BTreeMap::new(),
                            complex_attributes: btreemap! {
                                "values".to_owned() =>
                                vec![Value::FloatGroup(
                                    ref_arc.cell_fall.iter().cloned().collect(),
                                )],
                            },
                            groups: vec![],
                        },
                    ],
                });
            } else {
                eprintln!(
                    "Failed to process outpin {} in cell {} of library {}: no usable reference arc could be found", 
                    outpin_name, cell_name, lib.name
                );
                continue;
            }
        } // outpin

        if let Some(ref_arc) = mean_reference_arc(ref_arcs.clone().into_values()) {
            let setup_rise: BTreeMap<String, Array1<f64>> = cell_rise_arcs
                .clone()
                .into_iter()
                .group_by(|((src, _), _)| src.clone())
                .into_iter()
                // derive the mean arc from the input to each output
                .filter_map(|(k, v)| {
                    let mut n = 0.0;

                    v.into_iter()
                        .map(|x| {
                            n += 1.0;
                            x
                        })
                        .reduce(|(k, a), (_, b)| (k, a + b))
                        .map(|(_, v)| (k, v / n))
                })
                //extract the setup constraint from the mean arc
                .map(|(k, v)| {
                    (
                        k,
                        v.slice(s![.., ref_arc.col]).to_owned() - ref_arc.rise_trans[ref_arc.col],
                    )
                })
                .collect();

            let setup_fall: BTreeMap<String, Array1<f64>> = cell_fall_arcs
                .clone()
                .into_iter()
                .group_by(|((src, _), _)| src.clone())
                .into_iter()
                // derive the mean arc from the input to each output
                .filter_map(|(k, v)| {
                    let mut n = 0.0;

                    v.into_iter()
                        .map(|x| {
                            n += 1.0;
                            x
                        })
                        .reduce(|(k, a), (_, b)| (k, a + b))
                        .map(|(_, v)| (k, v / n))
                })
                //extract the setup constraint from the mean arc
                .map(|(k, v)| {
                    (
                        k,
                        v.slice(s![.., ref_arc.col]).to_owned() - ref_arc.rise_trans[ref_arc.col],
                    )
                })
                .collect();

            // Insert timing constraints for every pin
            for (inpin_name, inpin) in cell
                .pins
                .iter_mut()
                .filter(|(k, v)| is_input_pin(v) && !reset_name.is_match(k))
            {
                let constraint_values = match (
                    setup_fall.get(inpin_name),
                    setup_rise.get(inpin_name),
                ) {
                    (Some(setup_fall), Some(setup_rise)) => vec![
                        Group {
                            type_: "rise_constraint".to_owned(),
                            name: format!("{}_pseudo_constraint", ref_arc.lut_template),
                            complex_attributes: btreemap! {
                            "values".to_owned() => vec![Value::FloatGroup(setup_rise.iter().cloned().collect())],
                            },
                            simple_attributes: BTreeMap::new(),
                            groups: vec![],
                        },
                        Group {
                            type_: "fall_constraint".to_owned(),
                            name: format!("{}_pseudo_constraint", ref_arc.lut_template),
                            complex_attributes: btreemap! {
                            "values".to_owned() => vec![Value::FloatGroup(setup_fall.iter().cloned().collect())],
                            },
                            simple_attributes: BTreeMap::new(),
                            groups: vec![],
                        },
                    ],
                    (Some(setup_fall), None) => vec![Group {
                        type_: "fall_constraint".to_owned(),
                        name: format!("{}_pseudo_constraint", ref_arc.lut_template),
                        complex_attributes: btreemap! {
                            "values".to_owned() => vec![Value::FloatGroup(setup_fall.iter().cloned().collect())],
                        },
                        simple_attributes: BTreeMap::new(),
                        groups: vec![],
                    }],
                    (None, Some(setup_rise)) => vec![Group {
                        type_: "rise_constraint".to_owned(),
                        name: format!("{}_pseudo_constraint", ref_arc.lut_template),
                        complex_attributes: btreemap! {
                            "values".to_owned() => vec![Value::FloatGroup(setup_rise.iter().cloned().collect())],
                        },
                        simple_attributes: BTreeMap::new(),
                        groups: vec![],
                    }],
                    (None, None) => continue,
                };
                inpin.simple_attributes.insert(
                    "nextstate_type".to_owned(),
                    Value::Expression("data".to_owned()),
                );
                inpin.groups.push(Group {
                    type_: "timing".to_owned(),
                    name: "".to_owned(),
                    simple_attributes: btreemap! {
                        "related_pin".to_owned() => Value::String(clock_name.to_owned()),
                        "timing_type".to_owned() => Value::Expression("setup_rising".to_owned()),
                    },
                    complex_attributes: BTreeMap::new(),
                    groups: constraint_values,
                });
            } // inpin
              // storing lut_template name for later inclusing in liberty file
            lut_templates.insert(ref_arc.lut_template);
            // fixing latch group on ff model
            if !latch {
                for g in cell.groups.iter_mut().filter(|g| g.type_ == "latch") {
                    g.type_ = "ff".to_owned();

                    if let Some(clock) = g.simple_attributes.remove("enable") {
                        g.simple_attributes.insert("clocked_on".to_owned(), clock);
                    }

                    if let Some(vf) = g.simple_attributes.remove("data_in") {
                        g.simple_attributes.insert("next_state".to_owned(), vf);
                    }
                }
            }
            let rise_error: BTreeMap<(String, String), Array2<f64>> = cell_rise_arcs
                .iter()
                .map(|((src, dst), val)| {
                    let ref capacitance_dependent = ref_arcs[dst].cell_rise;
                    let ref slew_dependent = setup_rise[src];
                    let reconstructed_arc = restore_arc(slew_dependent, capacitance_dependent);

                    ((src.clone(), dst.clone()), reconstructed_arc - val)
                })
                .collect();
            let rise_error: BTreeMap<(String, String), Array2<f64>> = cell_fall_arcs
                .iter()
                .map(|((src, dst), val)| {
                    let ref capacitance_dependent = ref_arcs[dst].cell_fall;
                    let ref slew_dependent = setup_fall[src];
                    let reconstructed_arc = restore_arc(slew_dependent, capacitance_dependent);

                    ((src.clone(), dst.clone()), reconstructed_arc - val)
                })
                .collect();
        } else {
            eprintln!(
                "Failed to process cell {} of library {}: no reference arc found",
                cell_name, lib.name
            );
            continue;
        }
    } // cell
    let mut new_lut_templates: Vec<Group> = lib
        .groups
        .iter()
        .filter(|g| g.type_ == "lu_table_template" && lut_templates.contains(&g.name))
        .flat_map(|g| {
            vec![
                Group {
                    type_: "lu_table_template".to_owned(),
                    name: format!("{}_pseudo_constraint", g.name),
                    simple_attributes: btreemap! {
                        "variable_1".to_owned() => Value::Expression("constrained_pin_transition".to_owned()),
                    },
                    complex_attributes: btreemap! {
                        "index_1".to_owned() => g.complex_attributes["index_1"].clone(),
                    },
                    groups: vec![],
                },
                Group {
                    type_: "lu_table_template".to_owned(),
                    name: format!("{}_pseudo_delay", g.name),
                    simple_attributes: btreemap! {
                        "variable_1".to_owned() => Value::Expression("total_output_net_capacitance".to_owned()),
                    },
                    complex_attributes: btreemap! {
                        "index_1".to_owned() => g.complex_attributes["index_2"].clone(),
                    },
                    groups: vec![],
                },
            ]
        }).collect();
    lib.groups.append(&mut new_lut_templates);
}
