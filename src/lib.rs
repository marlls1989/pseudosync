//! Pseudosync library for converting Liberty file latches to flip-flops
//!
//! This library provides functions to process Liberty files and convert latch-based
//! cells to flip-flop-based cells with pseudo-synchronous timing constraints.

use indexmap::IndexMap;
use itertools::Itertools;
use lazy_static::lazy_static;
use liberty_parse::{
    self,
    ast::{LibertyAst, Value},
    liberty::{Attribute, Group, Liberty},
};
use ndarray::prelude::*;
use regex::Regex;
use simple_error::simple_error;
use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fs::{File, OpenOptions},
    io::{stdin, stdout, BufWriter, Read, Write},
    path::Path,
    sync::RwLock,
};

lazy_static! {
    static ref LATCH_REGEX: Regex = Regex::new(r"^latch").unwrap();
    static ref DEBUG_FILE: Result<RwLock<BufWriter<File>>, std::io::Error> = OpenOptions::new()
        .create(true)
        .append(true)
        .open("pseudosync.txt")
        .map(|f| RwLock::new(BufWriter::new(f)));
}

#[derive(Debug, Clone, PartialEq)]
pub struct RefArc {
    pub col: usize,
    pub row: usize,
    pub related_pin: String,
    pub lut_template: String,
    pub rise_trans: Array1<f64>,
    pub fall_trans: Array1<f64>,
    pub cell_rise: Array1<f64>,
    pub cell_fall: Array1<f64>,
}

/// Timing tables extracted from a timing group
#[derive(Debug, Clone)]
struct TimingTables {
    lut_template: String,
    cell_rise: Option<Array2<f64>>,
    cell_fall: Option<Array2<f64>>,
    rise_trans: Option<Array2<f64>>,
    fall_trans: Option<Array2<f64>>,
}

/// Parse a Liberty file from the given path
pub fn parse_liberty_file(path: &Path) -> Result<Liberty, Box<dyn Error>> {
    let mut input_stream: Box<dyn Read> = if path.as_os_str() == "-" {
        Box::new(stdin())
    } else {
        Box::new(File::open(path)?)
    };

    let mut buf = String::new();
    input_stream.read_to_string(&mut buf)?;
    let lib = liberty_parse::parse_lib(&buf).map_err(|e| simple_error!("{}", e))?;

    Ok(lib)
}

/// Write a Liberty AST to the specified path or stdout
pub fn write_liberty_file(path: Option<&Path>, liberty: &LibertyAst) -> Result<(), Box<dyn Error>> {
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
pub fn cell_qualifies(cell: &Group, clock_name: &str) -> bool {
    cell.subgroups
        .iter()
        .any(|group| LATCH_REGEX.is_match(&group.type_))
        && cell.iter_pins().any(|pin| pin.name == clock_name)
}

/// Check if a pin is an output pin
pub fn is_output_pin(pin: &Group) -> bool {
    (pin.type_ == "pin" || pin.type_ == "bundle")
        && pin
            .simple_attribute("direction")
            .map(|x| match x {
                Value::String(v) => v == "output",
                Value::Expression(v) => v == "output",
                _ => false,
            })
            .unwrap_or(false)
}

/// Check if a pin is an input pin
pub fn is_input_pin(pin: &Group) -> bool {
    (pin.type_ == "pin" || pin.type_ == "bundle")
        & pin
            .simple_attribute("direction")
            .map(|x| match x {
                Value::String(v) => v == "input",
                Value::Expression(v) => v == "input",
                _ => false,
            })
            .unwrap_or(false)
}

/// Calculate the mean of timing tables from multiple groups
pub fn mean_timingtable<'a, I>(groups: I) -> Option<Array2<f64>>
where
    I: IntoIterator<Item = &'a Group>,
{
    let mut n = 0.0;
    groups
        .into_iter()
        .map(|g| {
            n += 1.0;
            let v = g.complex_attribute("values").unwrap();
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
        .reduce(|a, b| a + b)
        .map(|x| x / n)
}

/// Calculate the mean reference arc from multiple RefArc instances
pub fn mean_reference_arc<I>(ref_arcs: I) -> Option<RefArc>
where
    I: IntoIterator<Item = RefArc>,
{
    let mut n = 0.0;
    ref_arcs
        .into_iter()
        .inspect(|_x| {
            n += 1.0;
        })
        .reduce(|a, b| {
            assert_eq!(a.col, b.col);
            assert_eq!(a.row, b.row);
            assert_eq!(&a.lut_template, &b.lut_template);
            RefArc {
                col: a.col,
                row: a.row,
                related_pin: a.related_pin,
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

/// Restore a 2D timing arc from 1D slew and capacitance dependent arrays
pub fn restore_arc(
    slew_dependent: &Array1<f64>,
    capacitance_dependent: &Array1<f64>,
) -> Array2<f64> {
    let cap: Array2<f64> =
        Array::ones((slew_dependent.len(), capacitance_dependent.len())) * capacitance_dependent;
    let slw: Array2<f64> =
        Array::ones((capacitance_dependent.len(), slew_dependent.len())) * slew_dependent;

    cap + slw.t()
}

/// Create a constraint table group (rise_constraint or fall_constraint)
fn create_constraint_table_group(
    constraint_type: &str,
    lut_template: &str,
    values: &Array1<f64>,
) -> Group {
    Group {
        type_: constraint_type.to_owned(),
        name: format!("{}_pseudo_constraint", lut_template),
        attributes: IndexMap::from([(
            "values".to_owned(),
            vec![Attribute::Complex(vec![Value::FloatGroup(
                values.iter().cloned().collect(),
            )])],
        )]),
        subgroups: vec![],
    }
}

/// Create a timing table group (cell_rise, cell_fall, rise_transition, fall_transition)
fn create_timing_table_group(table_type: &str, lut_template: &str, values: &Array1<f64>) -> Group {
    Group {
        type_: table_type.to_owned(),
        name: format!("{}_pseudo_delay", lut_template),
        attributes: IndexMap::from([(
            "values".to_owned(),
            vec![Attribute::Complex(vec![Value::FloatGroup(
                values.iter().cloned().collect(),
            )])],
        )]),
        subgroups: vec![],
    }
}

/// Create a setup timing group for an input pin
fn create_setup_timing_group(
    clock_name: &str,
    ref_arc: &RefArc,
    setup_rise: Option<&Array1<f64>>,
    setup_fall: Option<&Array1<f64>>,
) -> Group {
    let mut setup_values = Vec::with_capacity(2);

    if let Some(setup_rise) = setup_rise {
        setup_values.push(create_constraint_table_group(
            "rise_constraint",
            &ref_arc.lut_template,
            setup_rise,
        ));
    }

    if let Some(setup_fall) = setup_fall {
        setup_values.push(create_constraint_table_group(
            "fall_constraint",
            &ref_arc.lut_template,
            setup_fall,
        ));
    }

    Group {
        type_: "timing".to_owned(),
        name: "".to_owned(),
        attributes: IndexMap::from([
            (
                "related_pin".to_owned(),
                vec![Attribute::Simple(Value::String(clock_name.to_owned()))],
            ),
            (
                "timing_type".to_owned(),
                vec![Attribute::Simple(Value::Expression(
                    "setup_rising".to_owned(),
                ))],
            ),
        ]),
        subgroups: setup_values,
    }
}

/// Create a hold timing group for an input pin
fn create_hold_timing_group(
    clock_name: &str,
    ref_arc: &RefArc,
    hold_rise: Option<&Array1<f64>>,
    hold_fall: Option<&Array1<f64>>,
) -> Group {
    let mut hold_values = Vec::with_capacity(2);

    if let Some(hold_rise) = hold_rise {
        hold_values.push(create_constraint_table_group(
            "rise_constraint",
            &ref_arc.lut_template,
            hold_rise,
        ));
    }

    if let Some(hold_fall) = hold_fall {
        hold_values.push(create_constraint_table_group(
            "fall_constraint",
            &ref_arc.lut_template,
            hold_fall,
        ));
    }

    Group {
        type_: "timing".to_owned(),
        name: "".to_owned(),
        attributes: IndexMap::from([
            (
                "related_pin".to_owned(),
                vec![Attribute::Simple(Value::String(clock_name.to_owned()))],
            ),
            (
                "timing_type".to_owned(),
                vec![Attribute::Simple(Value::Expression(
                    "hold_rising".to_owned(),
                ))],
            ),
        ]),
        subgroups: hold_values,
    }
}

/// Create a pseudo-synchronous output timing arc
fn create_pseudo_output_timing_arc(
    clock_name: &str,
    output_transitions: &RefArc,
    mean_delays: &RefArc,
) -> Group {
    Group {
        type_: "timing".to_owned(),
        name: "".to_owned(),
        attributes: IndexMap::from([
            (
                "related_pin".to_owned(),
                vec![Attribute::Simple(Value::String(clock_name.to_owned()))],
            ),
            (
                "timing_sense".to_owned(),
                vec![Attribute::Simple(Value::Expression("non_unate".to_owned()))],
            ),
            (
                "timing_type".to_owned(),
                vec![Attribute::Simple(Value::Expression(
                    "rising_edge".to_owned(),
                ))],
            ),
        ]),
        subgroups: vec![
            // Use mean_delays.lut_template for consistency, but output's own transition values
            create_timing_table_group(
                "rise_transition",
                &mean_delays.lut_template,
                &output_transitions.rise_trans,
            ),
            create_timing_table_group(
                "fall_transition",
                &mean_delays.lut_template,
                &output_transitions.fall_trans,
            ),
            create_timing_table_group(
                "cell_rise",
                &mean_delays.lut_template,
                &mean_delays.cell_rise,
            ),
            create_timing_table_group(
                "cell_fall",
                &mean_delays.lut_template,
                &mean_delays.cell_fall,
            ),
        ],
    }
}

/// Extract timing tables from a timing group
fn extract_timing_tables_from_arc(timing_group: &Group) -> Option<TimingTables> {
    let mut lut_template = None;

    let (cell_rise_groups, others): (Vec<&Group>, Vec<&Group>) = timing_group
        .iter_subgroups()
        .partition(|g| g.type_ == "cell_rise");
    if let (Some(group), None) = (cell_rise_groups.first(), &lut_template) {
        lut_template = Some(group.name.clone())
    }
    let cell_rise = mean_timingtable(cell_rise_groups);

    let (cell_fall_groups, others): (Vec<&Group>, Vec<&Group>) =
        others.into_iter().partition(|g| g.type_ == "cell_fall");
    if let (Some(group), None) = (cell_fall_groups.first(), &lut_template) {
        lut_template = Some(group.name.clone())
    }
    let cell_fall = mean_timingtable(cell_fall_groups);

    let (rise_trans_groups, others): (Vec<&Group>, Vec<&Group>) = others
        .into_iter()
        .partition(|g| g.type_ == "rise_transition");
    if let (Some(group), None) = (rise_trans_groups.first(), &lut_template) {
        lut_template = Some(group.name.clone())
    }
    let rise_trans = mean_timingtable(rise_trans_groups);

    let fall_trans_groups: Vec<&Group> = others
        .into_iter()
        .filter(|g| g.type_ == "fall_transition")
        .collect();
    if let (Some(group), None) = (fall_trans_groups.first(), &lut_template) {
        lut_template = Some(group.name.clone())
    }
    let fall_trans = mean_timingtable(fall_trans_groups);

    // Require at least one timing table to be present
    if cell_rise.is_none() && cell_fall.is_none() && rise_trans.is_none() && fall_trans.is_none() {
        return None;
    }

    Some(TimingTables {
        lut_template: lut_template?,
        cell_rise,
        cell_fall,
        rise_trans,
        fall_trans,
    })
}

/// Select a reference arc from timing tables (uses middle row)
/// Returns None if the timing tables don't have all required data
fn select_reference_arc(related_pin: &str, timing_tables: &TimingTables) -> Option<RefArc> {
    // Require all four timing tables for the reference arc
    let cell_rise = timing_tables.cell_rise.as_ref()?;
    let cell_fall = timing_tables.cell_fall.as_ref()?;
    let rise_trans = timing_tables.rise_trans.as_ref()?;
    let fall_trans = timing_tables.fall_trans.as_ref()?;

    let col = cell_rise.len_of(Axis(1)) / 2;
    let row = cell_rise.len_of(Axis(0)) / 2;

    Some(RefArc {
        col,
        row,
        lut_template: timing_tables.lut_template.clone(),
        related_pin: related_pin.to_owned(),
        cell_fall: cell_fall.slice(s![row, ..]).to_owned(),
        cell_rise: cell_rise.slice(s![row, ..]).to_owned(),
        rise_trans: rise_trans.slice(s![row, ..]).to_owned(),
        fall_trans: fall_trans.slice(s![row, ..]).to_owned(),
    })
}

/// Calculate setup constraints for all input pins
fn calculate_setup_constraints(
    cell_rise_arcs: &BTreeMap<(String, String), Array2<f64>>,
    cell_fall_arcs: &BTreeMap<(String, String), Array2<f64>>,
    ref_arc: &RefArc,
) -> (BTreeMap<String, Array1<f64>>, BTreeMap<String, Array1<f64>>) {
    let setup_rise: BTreeMap<String, Array1<f64>> = cell_rise_arcs
        .clone()
        .into_iter()
        .group_by(|((src, _), _)| src.clone())
        .into_iter()
        // derive the mean arc from the input to each output
        .filter_map(|(k, v)| {
            let mut n = 0.0;

            v.into_iter()
                .inspect(|_x| {
                    n += 1.0;
                })
                .reduce(|(k, a), (_, b)| (k, a + b))
                .map(|(_, v)| (k, v / n))
        })
        //extract the setup constraint from the mean arc
        .map(|(k, v)| {
            (
                k,
                v.slice(s![.., ref_arc.col]).to_owned() - ref_arc.cell_rise[ref_arc.col],
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
                .inspect(|_x| {
                    n += 1.0;
                })
                .reduce(|(k, a), (_, b)| (k, a + b))
                .map(|(_, v)| (k, v / n))
        })
        //extract the setup constraint from the mean arc
        .map(|(k, v)| {
            (
                k,
                v.slice(s![.., ref_arc.col]).to_owned() - ref_arc.cell_fall[ref_arc.col],
            )
        })
        .collect();

    (setup_rise, setup_fall)
}

/// Calculate hold constraints from setup constraints (negated)
fn calculate_hold_constraints(
    setup_rise: &BTreeMap<String, Array1<f64>>,
    setup_fall: &BTreeMap<String, Array1<f64>>,
) -> (BTreeMap<String, Array1<f64>>, BTreeMap<String, Array1<f64>>) {
    let hold_rise = setup_rise
        .iter()
        .map(|(k, v)| (k.clone(), v.clone() * -1.0))
        .collect();

    let hold_fall = setup_fall
        .iter()
        .map(|(k, v)| (k.clone(), v.clone() * -1.0))
        .collect();

    (hold_rise, hold_fall)
}

/// Add pseudo-synchronous timing to an output pin
fn add_pseudo_timing_to_output_pin(
    outpin: &mut Group,
    clock_name: &str,
    reset_name: &Regex,
    output_transitions: &RefArc,
    mean_delays: &RefArc,
    latch: bool,
) {
    // If creating a pseudo_flop model, erase the original arcs
    if !latch {
        outpin.subgroups.retain(|x| {
            x.type_ != "timing"
                || reset_name.is_match(
                    &x.simple_attribute("related_pin")
                        .map_or("".to_owned(), |x| x.string()),
                )
        });
    }

    // Add the new pseudo-synchronous timing arc:
    // - Use this output's own transitions (decoupled from input)
    // - Use mean cell_rise/cell_fall delays (averaged across outputs)
    outpin.subgroups.push(create_pseudo_output_timing_arc(
        clock_name,
        output_transitions,
        mean_delays,
    ));
}

/// Add setup and hold constraints to an input pin
fn add_constraints_to_input_pin(
    inpin: &mut Group,
    clock_name: &str,
    ref_arc: &RefArc,
    setup_rise: &BTreeMap<String, Array1<f64>>,
    setup_fall: &BTreeMap<String, Array1<f64>>,
    hold_rise: &BTreeMap<String, Array1<f64>>,
    hold_fall: &BTreeMap<String, Array1<f64>>,
) {
    let inpin_name = inpin.name.as_str();

    // Mark pin as data input
    inpin.attributes.insert(
        "nextstate_type".to_owned(),
        vec![Attribute::Simple(Value::Expression("data".to_owned()))],
    );

    // Add setup constraint
    inpin.subgroups.push(create_setup_timing_group(
        clock_name,
        ref_arc,
        setup_rise.get(inpin_name),
        setup_fall.get(inpin_name),
    ));

    // Add hold constraint
    inpin.subgroups.push(create_hold_timing_group(
        clock_name,
        ref_arc,
        hold_rise.get(inpin_name),
        hold_fall.get(inpin_name),
    ));
}

/// Convert latch groups to flip-flop groups
fn convert_latch_to_flipflop(cell: &mut Group) {
    for g in cell
        .iter_subgroups_mut()
        .filter(|g| LATCH_REGEX.is_match(&g.type_))
    {
        g.type_ = LATCH_REGEX.replace(&g.type_, "ff").into();

        if let Some(clock) = g.attributes.remove("enable") {
            g.attributes.insert("clocked_on".to_owned(), clock);
        }

        if let Some(vf) = g.attributes.remove("data_in") {
            g.attributes.insert("next_state".to_owned(), vf);
        }
    }
}

/// Generate pseudo LUT templates for constraints and delays
fn generate_pseudo_lut_templates(lib: &Group, used_templates: &HashSet<String>) -> Vec<Group> {
    lib.iter_subgroups()
        .filter(|g| g.type_ == "lu_table_template" && used_templates.contains(&g.name))
        .flat_map(|g| {
            vec![
                Group {
                    type_: "lu_table_template".to_owned(),
                    name: format!("{}_pseudo_constraint", g.name),
                    attributes: IndexMap::from([
                        (
                            "variable_1".to_owned(),
                            vec![Attribute::Simple(Value::Expression(
                                "constrained_pin_transition".to_owned(),
                            ))],
                        ),
                        ("index_1".to_owned(), g.attributes["index_1"].clone()),
                    ]),
                    subgroups: vec![],
                },
                Group {
                    type_: "lu_table_template".to_owned(),
                    name: format!("{}_pseudo_delay", g.name),
                    attributes: IndexMap::from([
                        (
                            "variable_1".to_owned(),
                            vec![Attribute::Simple(Value::Expression(
                                "total_output_net_capacitance".to_owned(),
                            ))],
                        ),
                        ("index_1".to_owned(), g.attributes["index_2"].clone()),
                    ]),
                    subgroups: vec![],
                },
            ]
        })
        .collect()
}

/// Process a single cell to add pseudo-synchronous timing
fn process_cell(
    cell: &mut Group,
    clock_name: &str,
    reset_name: &Regex,
    latch: bool,
    lib_name: &str,
) -> Option<String> {
    let cell_name = cell.name.clone();
    eprintln!("Processing cell {}", cell_name);

    let mut ref_arcs: BTreeMap<String, RefArc> = BTreeMap::new();
    let mut cell_rise_arcs: BTreeMap<(String, String), Array2<f64>> = BTreeMap::new();
    let mut cell_fall_arcs: BTreeMap<(String, String), Array2<f64>> = BTreeMap::new();

    // Phase 1: Extract timing data from all output pins
    for outpin in cell.iter_subgroups().filter(|pin| is_output_pin(pin)) {
        let outpin_name = &outpin.name;

        // Process each timing group in the output pin
        for timing_group in outpin.iter_subgroups_of_type("timing") {
            let related_pin = timing_group
                .simple_attribute("related_pin")
                .unwrap()
                .string();

            // Skip reset pins
            if reset_name.is_match(&related_pin) {
                continue;
            }

            // Extract timing tables from this arc
            if let Some(timing_tables) = extract_timing_tables_from_arc(timing_group) {
                // Select reference arc if we don't have one for this output yet
                // This captures the transition data for THIS specific output pin
                // Only use arcs that have all four timing tables
                if !ref_arcs.contains_key(outpin_name) {
                    if let Some(ref_arc) = select_reference_arc(&related_pin, &timing_tables) {
                        eprintln!(
                            "  Pin {} selected as reference arc for output {}",
                            related_pin, outpin_name
                        );
                        ref_arcs.insert(outpin_name.clone(), ref_arc);
                    }
                }

                // Store the full timing arcs for constraint calculation (if present)
                if let Some(cell_rise) = timing_tables.cell_rise {
                    cell_rise_arcs.insert((related_pin.clone(), outpin_name.clone()), cell_rise);
                }
                if let Some(cell_fall) = timing_tables.cell_fall {
                    cell_fall_arcs.insert((related_pin.clone(), outpin_name.clone()), cell_fall);
                }
            }
        }
    }

    // Phase 2: Calculate mean reference arc for delays and constraints
    let mean_ref_arc = mean_reference_arc(ref_arcs.values().cloned())?;

    // Phase 3: Add pseudo timing to each output pin
    for outpin in cell.iter_subgroups_mut().filter(|pin| is_output_pin(pin)) {
        let outpin_name = &outpin.name;

        if let Some(output_transitions) = ref_arcs.get(outpin_name) {
            add_pseudo_timing_to_output_pin(
                outpin,
                clock_name,
                reset_name,
                output_transitions,
                &mean_ref_arc,
                latch,
            );
        } else {
            eprintln!(
                "Failed to process outpin {} in cell {} of library {}: no usable reference arc could be found",
                outpin_name, cell_name, lib_name
            );
        }
    }

    // Phase 4: Calculate setup/hold constraints using mean reference arc
    let ref_arc = mean_ref_arc;

    let (setup_rise, setup_fall) =
        calculate_setup_constraints(&cell_rise_arcs, &cell_fall_arcs, &ref_arc);

    let (hold_rise, hold_fall) = calculate_hold_constraints(&setup_rise, &setup_fall);

    // Phase 5: Add constraints to all input pins
    for inpin in cell
        .iter_subgroups_mut()
        .filter(|v| is_input_pin(v) && !reset_name.is_match(&v.name))
    {
        add_constraints_to_input_pin(
            inpin,
            clock_name,
            &ref_arc,
            &setup_rise,
            &setup_fall,
            &hold_rise,
            &hold_fall,
        );
    }

    // Phase 6: Convert latch to flip-flop if needed
    if !latch {
        convert_latch_to_flipflop(cell);
    }

    // Return the lut_template name for library-level template generation
    Some(ref_arc.lut_template)
}

/// Process a library to convert latches to flip-flops or add pseudo-synchronous timing
pub fn process_library(lib: &mut Group, clock_name: &str, reset_name: &Regex, latch: bool) {
    eprintln!("Processing library {}", lib.name);

    let mut lut_templates: HashSet<String> = HashSet::new();
    let lib_name = lib.name.clone();

    // Process each qualifying cell
    for cell in lib
        .iter_cells_mut()
        .filter(|x| cell_qualifies(x, clock_name))
    {
        if let Some(template_name) = process_cell(cell, clock_name, reset_name, latch, &lib_name) {
            lut_templates.insert(template_name);
        } else {
            eprintln!(
                "Failed to process cell {} of library {}: no reference arc found",
                cell.name, lib_name
            );
        }
    }

    // Generate and prepend pseudo LUT templates
    let mut new_lut_templates = generate_pseudo_lut_templates(lib, &lut_templates);
    new_lut_templates.append(&mut lib.subgroups);
    lib.subgroups = new_lut_templates;
}

#[cfg(test)]
mod tests {
    //! Unit tests for the private/pure engine functions that the black-box
    //! integration suites in `tests/` cannot reach directly.
    use super::*;

    fn lut_template(name: &str) -> Group {
        // A minimal lu_table_template carrying index_1 and index_2, the two
        // attributes generate_pseudo_lut_templates clones.
        Group {
            type_: "lu_table_template".to_owned(),
            name: name.to_owned(),
            attributes: IndexMap::from([
                (
                    "index_1".to_owned(),
                    vec![Attribute::Complex(vec![Value::Float(0.1), Value::Float(0.2)])],
                ),
                (
                    "index_2".to_owned(),
                    vec![Attribute::Complex(vec![Value::Float(1.0), Value::Float(2.0)])],
                ),
            ]),
            subgroups: vec![],
        }
    }

    fn simple_expr(value: &str) -> Vec<Attribute> {
        vec![Attribute::Simple(Value::Expression(value.to_owned()))]
    }

    // --- restore_arc -------------------------------------------------------

    #[test]
    fn restore_arc_is_the_outer_sum_of_the_1d_arcs() {
        // slew (row) = [1, 2], cap (col) = [10, 20]
        // result[r][c] = slew[r] + cap[c]
        let slew = Array1::from(vec![1.0, 2.0]);
        let cap = Array1::from(vec![10.0, 20.0]);
        let got = restore_arc(&slew, &cap);
        let expected =
            Array2::from_shape_vec((2, 2), vec![11.0, 21.0, 12.0, 22.0]).unwrap();
        assert_eq!(got, expected);
    }

    // --- select_reference_arc ---------------------------------------------

    fn nine(base: f64) -> Array2<f64> {
        // 3x3 table whose middle row (index 1) is [base+3, base+4, base+5]
        Array2::from_shape_vec(
            (3, 3),
            (0..9).map(|i| base + i as f64).collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn select_reference_arc_picks_the_middle_row_and_column() {
        let tt = TimingTables {
            lut_template: "T".to_owned(),
            cell_rise: Some(nine(0.0)),
            cell_fall: Some(nine(100.0)),
            rise_trans: Some(nine(200.0)),
            fall_trans: Some(nine(300.0)),
        };
        let arc = select_reference_arc("CK", &tt).expect("all four tables present");
        assert_eq!(arc.row, 1);
        assert_eq!(arc.col, 1);
        assert_eq!(arc.related_pin, "CK");
        assert_eq!(arc.lut_template, "T");
        // middle row of cell_rise == [3,4,5]
        assert_eq!(arc.cell_rise, Array1::from(vec![3.0, 4.0, 5.0]));
        assert_eq!(arc.cell_fall, Array1::from(vec![103.0, 104.0, 105.0]));
    }

    #[test]
    fn select_reference_arc_requires_all_four_tables() {
        let tt = TimingTables {
            lut_template: "T".to_owned(),
            cell_rise: Some(nine(0.0)),
            cell_fall: None, // missing -> no reference arc
            rise_trans: Some(nine(200.0)),
            fall_trans: Some(nine(300.0)),
        };
        assert!(select_reference_arc("CK", &tt).is_none());
    }

    // --- calculate_setup / hold constraints --------------------------------

    #[test]
    fn setup_constraint_is_input_arc_minus_reference_delay() {
        // One input->output rise arc for source pin "D"; column `col` is what
        // the reference samples. ref.cell_rise[col] is subtracted off.
        let col = 1usize;
        // 3x3 arc whose column 1 is [25, 35, 45]
        let arc = Array2::from_shape_vec(
            (3, 3),
            vec![0.0, 25.0, 0.0, 0.0, 35.0, 0.0, 0.0, 45.0, 0.0],
        )
        .unwrap();
        let mut cell_rise_arcs: BTreeMap<(String, String), Array2<f64>> = BTreeMap::new();
        cell_rise_arcs.insert(("D".to_owned(), "Q".to_owned()), arc);
        let cell_fall_arcs: BTreeMap<(String, String), Array2<f64>> = BTreeMap::new();

        let ref_arc = RefArc {
            col,
            row: 1,
            related_pin: "CK".to_owned(),
            lut_template: "T".to_owned(),
            rise_trans: Array1::from(vec![0.0, 0.0, 0.0]),
            fall_trans: Array1::from(vec![0.0, 0.0, 0.0]),
            cell_rise: Array1::from(vec![10.0, 20.0, 30.0]), // [col]=20
            cell_fall: Array1::from(vec![0.0, 0.0, 0.0]),
        };

        let (setup_rise, setup_fall) =
            calculate_setup_constraints(&cell_rise_arcs, &cell_fall_arcs, &ref_arc);

        // [25,35,45] - 20 = [5,15,25]
        assert_eq!(setup_rise["D"], Array1::from(vec![5.0, 15.0, 25.0]));
        assert!(setup_fall.is_empty());

        // hold = -setup
        let (hold_rise, hold_fall) = calculate_hold_constraints(&setup_rise, &setup_fall);
        assert_eq!(hold_rise["D"], Array1::from(vec![-5.0, -15.0, -25.0]));
        assert!(hold_fall.is_empty());
    }

    // --- generate_pseudo_lut_templates ------------------------------------

    #[test]
    fn generate_pseudo_lut_templates_emits_constraint_and_delay_pair() {
        let lib = Group {
            type_: "library".to_owned(),
            name: "L".to_owned(),
            attributes: IndexMap::new(),
            subgroups: vec![
                lut_template("delay_template_3x3"),
                lut_template("unused_template"),
                // a non-template subgroup must be ignored
                Group {
                    type_: "cell".to_owned(),
                    name: "C".to_owned(),
                    attributes: IndexMap::new(),
                    subgroups: vec![],
                },
            ],
        };
        let used: HashSet<String> = ["delay_template_3x3".to_owned()].into_iter().collect();

        let out = generate_pseudo_lut_templates(&lib, &used);

        // Only the *used* template expands, into exactly two derived templates.
        let names: Vec<&str> = out.iter().map(|g| g.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "delay_template_3x3_pseudo_constraint",
                "delay_template_3x3_pseudo_delay",
            ]
        );

        let constraint = &out[0];
        assert_eq!(
            constraint.attributes["variable_1"],
            simple_expr("constrained_pin_transition")
        );
        // constraint takes its index from the source index_1
        assert_eq!(
            constraint.attributes["index_1"],
            lib.subgroups[0].attributes["index_1"]
        );

        let delay = &out[1];
        assert_eq!(
            delay.attributes["variable_1"],
            simple_expr("total_output_net_capacitance")
        );
        // delay takes its index from the source index_2
        assert_eq!(
            delay.attributes["index_1"],
            lib.subgroups[0].attributes["index_2"]
        );
    }

    // --- pin predicates over a parsed cell --------------------------------

    fn sample_lib() -> Liberty {
        liberty_parse::parse_lib(
            r#"
library(test) {
  cell(LCELL) {
    latch(IQ, IQN) { enable: "G"; data_in: "D"; }
    pin(D) { direction: input; }
    pin(G) { direction: input; }
    pin(Q) { direction: output; function: "IQ"; }
  }
  cell(COMB) {
    pin(A) { direction: input; }
    pin(Y) { direction: output; function: "A"; }
  }
}
"#,
        )
        .expect("parse sample lib")
    }

    #[test]
    fn pin_direction_predicates() {
        let lib = sample_lib();
        let cell = lib[0].get_cell("LCELL").unwrap();
        assert!(is_output_pin(cell.get_pin("Q").unwrap()));
        assert!(!is_output_pin(cell.get_pin("D").unwrap()));
        assert!(is_input_pin(cell.get_pin("D").unwrap()));
        assert!(!is_input_pin(cell.get_pin("Q").unwrap()));
    }

    #[test]
    fn cell_qualifies_needs_a_latch_group_and_the_clock_pin() {
        let lib = sample_lib();
        let lcell = lib[0].get_cell("LCELL").unwrap();
        let comb = lib[0].get_cell("COMB").unwrap();
        assert!(cell_qualifies(lcell, "G"));
        assert!(!cell_qualifies(lcell, "CLK")); // no pin named CLK
        assert!(!cell_qualifies(comb, "G")); // no latch group
    }

    #[test]
    fn convert_latch_to_flipflop_renames_group_and_attributes() {
        let mut lib = sample_lib();
        let cell = lib[0].get_cell_mut("LCELL").unwrap();
        convert_latch_to_flipflop(cell);
        let g = cell
            .iter_subgroups()
            .find(|g| g.type_ == "ff")
            .expect("latch became ff");
        assert!(g.attributes.contains_key("clocked_on")); // enable -> clocked_on
        assert!(g.attributes.contains_key("next_state")); // data_in -> next_state
        assert!(!g.attributes.contains_key("enable"));
        assert!(!g.attributes.contains_key("data_in"));
    }

    /// The documented `pseudosync.txt` debug-writer is dead code (declared but
    /// never written), so merely exercising the engine must not create it.
    #[test]
    fn engine_does_not_leak_pseudosync_txt_in_cwd() {
        let tmp = std::env::temp_dir().join(format!("pseudosync_leak_{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();

        let mut lib = sample_lib();
        process_library(&mut lib[0], "G", &Regex::new("(R|S)N?").unwrap(), false);

        let leaked = tmp.join("pseudosync.txt").exists();
        std::env::set_current_dir(&prev).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
        assert!(!leaked, "pseudosync.txt should not be created in CWD");
    }
}
