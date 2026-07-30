//! Pseudosync library for converting Liberty file latches to flip-flops
//!
//! This library provides functions to process Liberty files and convert latch-based
//! cells to flip-flop-based cells with pseudo-synchronous timing constraints.

use indexmap::IndexMap;
use itertools::Itertools;
use lazy_static::lazy_static;
use liberty_parser::{
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
    fs::File,
    io::{stdin, stdout, BufWriter, Read, Write},
    path::Path,
};

lazy_static! {
    static ref LATCH_REGEX: Regex = Regex::new(r"^latch").unwrap();
}

/// How the clock-to-output reference delay is chosen when a cell drives several
/// outputs.
///
/// The pseudo-flop model splits each original input-to-output arc into a setup
/// constraint plus a clock-to-output delay, so the same reference must be used on
/// both sides for `setup(D) + clk→Q` to reconstruct `delay(D→Q)`. The two modes
/// differ in how wide that reference is drawn, which matters for cells whose
/// outputs are independent rails rather than views of one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceMode {
    /// One reference shared by the whole cell: the mean across every output.
    /// Both the emitted delays and the constraints use it, so every output is
    /// given the same clock-to-output delay. This was the original behaviour.
    Pooled,
    /// The default. Each output keeps its own reference for its emitted delay, and each input
    /// is constrained against the mean reference of only the outputs it actually
    /// drives. For a dual-rail bundle this reduces to running the algorithm
    /// independently per rail, while a control input shared by both rails is
    /// still referenced against both.
    PerOutput,
}

impl std::str::FromStr for ReferenceMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pooled" => Ok(ReferenceMode::Pooled),
            "per-output" => Ok(ReferenceMode::PerOutput),
            other => Err(format!(
                "unknown reference mode {:?}, expected \"pooled\" or \"per-output\"",
                other
            )),
        }
    }
}

/// How the several `when`-conditioned arcs of one pin pair are merged into the
/// single arc the pseudo-flop model can carry.
///
/// A cell characterised over many operating states describes one transition once
/// per state, and those states can differ for real physical reasons -- a device
/// sitting at a different depth in the stack conducts differently -- so the
/// spread between them is data, not noise. The model has room for one arc, and
/// which one it should be depends on what the result is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhenMerge {
    /// Elementwise mean over the conditions. Representative rather than
    /// bounding: uses every measurement, sits inside the spread.
    Mean,
    /// Elementwise minimum. The optimistic envelope.
    Min,
    /// Elementwise maximum. The pessimistic envelope, closest to a signoff bound.
    Max,
}

impl std::str::FromStr for WhenMerge {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "mean" => Ok(WhenMerge::Mean),
            "min" => Ok(WhenMerge::Min),
            "max" => Ok(WhenMerge::Max),
            other => Err(format!(
                "unknown when-merge {:?}, expected \"mean\", \"min\" or \"max\"",
                other
            )),
        }
    }
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

/// One arc exactly as the library characterised it, before any reduction.
///
/// The engine folds a pin pair's `when` conditions into one representative arc
/// to build the model, but the model still has to stand against every condition
/// it claims to cover. Keeping the raw tables lets the report measure it against
/// what was actually measured, so the cost of the `when` reduction shows up
/// instead of being averaged out of its own error term.
#[derive(Debug, Clone, PartialEq)]
pub struct ConditionedArc {
    pub source: String,
    pub output: String,
    /// The `when` expression this arc was characterised under, if any.
    pub when: Option<String>,
    pub timing_type: Option<String>,
    pub timing_sense: Option<String>,
    pub cell_rise: Option<Array2<f64>>,
    pub cell_fall: Option<Array2<f64>>,
}

/// How far the pseudo-flop split lands from the arc it replaced.
///
/// An original arc is a slew x load table. The model stores it as a
/// slew-dependent setup constraint on the input plus a load-dependent
/// clock-to-output delay, and reconstructs it as the outer sum of the two --
/// [`restore_arc`]. The residual is what that separable form cannot express.
#[derive(Debug, Clone, PartialEq)]
pub struct ArcError {
    pub cell: String,
    pub source: String,
    pub output: String,
    /// "rise" or "fall"
    pub edge: &'static str,
    /// The `when` condition of the arc being reconstructed, if it had one.
    pub when: Option<String>,
    pub timing_type: Option<String>,
    pub original: Array2<f64>,
    pub reconstructed: Array2<f64>,
    pub error: Array2<f64>,
}

impl ArcError {
    /// Mean magnitude of the original table, to judge the residual against.
    pub fn scale(&self) -> f64 {
        self.original.iter().map(|v| v.abs()).sum::<f64>() / self.original.len() as f64
    }

    /// Mean signed residual: positive means the model is pessimistic.
    pub fn bias(&self) -> f64 {
        self.error.iter().sum::<f64>() / self.error.len() as f64
    }

    pub fn rms(&self) -> f64 {
        (self.error.iter().map(|v| v * v).sum::<f64>() / self.error.len() as f64).sqrt()
    }

    pub fn max_abs(&self) -> f64 {
        self.error.iter().fold(0.0_f64, |m, v| m.max(v.abs()))
    }

    /// RMS residual as a percentage of the arc's own magnitude.
    pub fn rms_percent(&self) -> f64 {
        let scale = self.scale();
        if scale == 0.0 {
            0.0
        } else {
            100.0 * self.rms() / scale
        }
    }
}

/// Everything the reconstruction report shows for one processed cell.
///
/// The engine returns this rather than writing it out itself, so the library
/// performs no I/O and the numbers stay inspectable from tests.
#[derive(Debug, Clone)]
pub struct CellReport {
    pub library: String,
    pub cell: String,
    /// How this cell's `when` conditions were merged.
    pub when_merge: WhenMerge,
    /// Every arc as characterised, one entry per `when` condition.
    pub raw_arcs: Vec<ConditionedArc>,
    /// The same arcs after the `when` conditions were folded together; this is
    /// what the constraints were actually derived from.
    pub cell_rise_arcs: BTreeMap<(String, String), Array2<f64>>,
    pub cell_fall_arcs: BTreeMap<(String, String), Array2<f64>>,
    /// Reference arc chosen per output.
    pub ref_arcs: BTreeMap<String, RefArc>,
    /// Reference the constraints were taken against.
    pub mean_ref: RefArc,
    pub setup_rise: BTreeMap<String, Array1<f64>>,
    pub setup_fall: BTreeMap<String, Array1<f64>>,
    pub arcs: Vec<ArcError>,
}

/// The references a cell's constraints and delays were drawn against.
struct References<'a> {
    per_output: &'a BTreeMap<String, RefArc>,
    mean: &'a RefArc,
    mode: ReferenceMode,
}

impl References<'_> {
    /// The clock-to-output delay this mode actually emits for `output`.
    fn delay_for(&self, output: &str) -> Option<&RefArc> {
        match self.mode {
            ReferenceMode::Pooled => Some(self.mean),
            ReferenceMode::PerOutput => self.per_output.get(output),
        }
    }
}

/// One half of an arc: which raw table it reads, and which component of a
/// reference arc pairs with it. Bundled so the two can never be mismatched.
struct Edge {
    name: &'static str,
    raw: fn(&ConditionedArc) -> Option<&Array2<f64>>,
    reference: fn(&RefArc) -> &Array1<f64>,
}

const RISE: Edge = Edge {
    name: "rise",
    raw: |a| a.cell_rise.as_ref(),
    reference: |r| &r.cell_rise,
};

const FALL: Edge = Edge {
    name: "fall",
    raw: |a| a.cell_fall.as_ref(),
    reference: |r| &r.cell_fall,
};

/// Measure the reconstruction residual against every condition of every arc.
///
/// The model carries one setup constraint per pin and one delay per output, so
/// each raw condition is reconstructed from that same pair. The residual it
/// leaves therefore contains both error sources at once: what the separable
/// setup-plus-delay form cannot express, and what collapsing the `when`
/// conditions into one representative arc threw away.
fn collect_arc_errors(
    arcs: &[ConditionedArc],
    setup: &BTreeMap<String, Array1<f64>>,
    references: &References,
    cell_name: &str,
    edge: &Edge,
    out: &mut Vec<ArcError>,
) {
    for arc in arcs {
        let (source, output) = (&arc.source, &arc.output);
        let Some(original) = (edge.raw)(arc) else {
            continue;
        };
        let Some(slew_dependent) = setup.get(source) else {
            continue;
        };
        // Reconstruct with the delay this mode actually emits, so the residual
        // describes the library that ships rather than an idealised split. The
        // original report predated pooling and always used the per-output arc.
        let Some(delay_ref) = references.delay_for(output) else {
            continue;
        };

        let reconstructed = restore_arc(slew_dependent, (edge.reference)(delay_ref));
        if reconstructed.raw_dim() != original.raw_dim() {
            continue;
        }

        out.push(ArcError {
            cell: cell_name.to_owned(),
            source: source.clone(),
            output: output.clone(),
            edge: edge.name,
            when: arc.when.clone(),
            timing_type: arc.timing_type.clone(),
            error: reconstructed.clone() - original,
            original: original.clone(),
            reconstructed,
        });
    }
}

/// Running mean of one table family across arcs that share a (related_pin,
/// output) pair.
#[derive(Debug, Clone)]
struct TableAccumulator {
    sum: Option<Array2<f64>>,
    n: f64,
    merge: WhenMerge,
}

impl TableAccumulator {
    fn new(merge: WhenMerge) -> Self {
        Self {
            sum: None,
            n: 0.0,
            merge,
        }
    }

    fn add(&mut self, table: Array2<f64>, family: &str, related_pin: &str, outpin: &str) {
        let merge = self.merge;
        match self.sum.as_mut() {
            // Conditions of one arc are characterised on a common template, so a
            // shape change means these are not the same transition. Averaging
            // them would be meaningless, and adding them would panic.
            Some(sum) if sum.raw_dim() != table.raw_dim() => eprintln!(
                "  Ignoring a {} arc {} -> {}: table shape {:?} differs from {:?}",
                family,
                related_pin,
                outpin,
                table.shape(),
                sum.shape()
            ),
            Some(sum) => {
                match merge {
                    WhenMerge::Mean => *sum += &table,
                    // Elementwise so the envelope is taken per slew/load point,
                    // not by picking whichever condition looks worst overall.
                    WhenMerge::Min => sum.zip_mut_with(&table, |a, b| *a = a.min(*b)),
                    WhenMerge::Max => sum.zip_mut_with(&table, |a, b| *a = a.max(*b)),
                }
                self.n += 1.0;
            }
            None => {
                self.sum = Some(table);
                self.n = 1.0;
            }
        }
    }

    fn result(&self) -> Option<Array2<f64>> {
        self.sum.as_ref().map(|sum| match self.merge {
            WhenMerge::Mean => sum / self.n,
            WhenMerge::Min | WhenMerge::Max => sum.clone(),
        })
    }
}

/// The several `when`-conditioned arcs of one (related_pin, output) pair,
/// reduced to a single representative arc.
///
/// A cell characterised over many `when` conditions describes one transition
/// several times, once per operating state. The pseudo-flop model has room for
/// only one, so each table family is averaged over the conditions that
/// characterise it — using every measurement rather than keeping one arbitrary
/// condition and discarding the rest. The propagation-preserving latch view
/// (`--latch`) is what retains the per-condition detail.
///
/// Families are counted separately because a `combinational_rise` arc carries
/// only the rise tables and a `combinational_fall` arc only the fall ones, so
/// the two are averaged over different numbers of conditions.
#[derive(Debug, Clone)]
struct ArcAccumulator {
    lut_template: Option<String>,
    cell_rise: TableAccumulator,
    cell_fall: TableAccumulator,
    rise_trans: TableAccumulator,
    fall_trans: TableAccumulator,
}

impl ArcAccumulator {
    fn new(merge: WhenMerge) -> Self {
        Self {
            lut_template: None,
            cell_rise: TableAccumulator::new(merge),
            cell_fall: TableAccumulator::new(merge),
            rise_trans: TableAccumulator::new(merge),
            fall_trans: TableAccumulator::new(merge),
        }
    }

    fn accumulate(&mut self, tables: TimingTables, related_pin: &str, outpin: &str) {
        if self.lut_template.is_none() {
            self.lut_template = Some(tables.lut_template);
        }

        for (table, family, acc) in [
            (tables.cell_rise, "cell_rise", &mut self.cell_rise),
            (tables.cell_fall, "cell_fall", &mut self.cell_fall),
            (tables.rise_trans, "rise_transition", &mut self.rise_trans),
            (tables.fall_trans, "fall_transition", &mut self.fall_trans),
        ] {
            if let Some(table) = table {
                acc.add(table, family, related_pin, outpin);
            }
        }
    }

    fn result(&self) -> Option<TimingTables> {
        Some(TimingTables {
            lut_template: self.lut_template.clone()?,
            cell_rise: self.cell_rise.result(),
            cell_fall: self.cell_fall.result(),
            rise_trans: self.rise_trans.result(),
            fall_trans: self.fall_trans.result(),
        })
    }
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
    let lib = liberty_parser::parse_lib(&buf).map_err(|e| simple_error!("{}", e))?;

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

/// Check if a group owns timing arcs directly, rather than delegating them to
/// member `pin` subgroups.
fn owns_timing_arcs(group: &Group) -> bool {
    group.subgroups.iter().any(|g| g.type_ == "timing")
}

/// Collect the leaf pin groups of a cell matching a direction predicate.
///
/// Liberty allows two ways of writing a `bundle`. Either the bundle owns its
/// `timing` groups and the member `pin` subgroups carry only per-member trivia,
/// or the bundle is a plain container and each member `pin` holds its own arcs.
/// A cell-level `pin` is always a leaf; a `bundle` is a leaf only in the first
/// form. In the second, the members are the leaves and inherit the bundle's
/// `direction`, which they do not carry themselves — so the direction predicate
/// is applied to the bundle and never re-tested on a member.
fn timing_leaves(cell: &Group, direction: fn(&Group) -> bool) -> Vec<&Group> {
    cell.subgroups
        .iter()
        .filter(|g| direction(g))
        .flat_map(|g| {
            if g.type_ == "bundle" && !owns_timing_arcs(g) {
                g.subgroups.iter().filter(|s| s.type_ == "pin").collect()
            } else {
                vec![g]
            }
        })
        .collect()
}

/// Mutable counterpart of [`timing_leaves`]
fn timing_leaves_mut(cell: &mut Group, direction: fn(&Group) -> bool) -> Vec<&mut Group> {
    cell.subgroups
        .iter_mut()
        .filter(|g| direction(g))
        .flat_map(|g| {
            if g.type_ == "bundle" && !owns_timing_arcs(g) {
                g.subgroups
                    .iter_mut()
                    .filter(|s| s.type_ == "pin")
                    .collect()
            } else {
                vec![g]
            }
        })
        .collect()
}

/// Collect the input groups that setup/hold constraints should be written to.
///
/// Which name the constraints are keyed by is decided by the library, not by the
/// structure: the keys are the `related_pin` strings harvested from the output
/// arcs. A bundle may be named there directly, in which case it takes a single
/// shared constraint, or its members may be named individually, in which case
/// each member takes its own. `has_constraints` reports whether a name was
/// harvested, so only groups the library actually characterised are returned —
/// which also leaves the clock pin, and any input with no arc to an output,
/// untouched rather than carrying an empty constraint.
fn constraint_targets_mut<'a>(
    cell: &'a mut Group,
    has_constraints: &dyn Fn(&str) -> bool,
) -> Vec<&'a mut Group> {
    cell.subgroups
        .iter_mut()
        .filter(|g| is_input_pin(g))
        .flat_map(|g| {
            // Resolved before borrowing so a bundle and its members are never
            // both handed out.
            if g.type_ == "bundle" && !has_constraints(&g.name) {
                g.subgroups
                    .iter_mut()
                    .filter(|s| s.type_ == "pin" && has_constraints(&s.name))
                    .collect()
            } else if has_constraints(&g.name) {
                vec![g]
            } else {
                vec![]
            }
        })
        .collect()
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

/// Reduce the input-to-output arcs of one source pin to a single constraint.
///
/// The mean arc from the source to every output it drives, sampled at the
/// reference column, minus the clock-to-output delay that the pseudo-flop model
/// will charge for that path. Which delay that is depends on `mode`; see
/// [`ReferenceMode`].
fn constraints_from_arcs(
    arcs: &BTreeMap<(String, String), Array2<f64>>,
    ref_arcs: &BTreeMap<String, RefArc>,
    mean_ref: &RefArc,
    mode: ReferenceMode,
    select: fn(&RefArc) -> &Array1<f64>,
) -> BTreeMap<String, Array1<f64>> {
    let col = mean_ref.col;

    arcs.iter()
        .group_by(|((src, _), _)| src.clone())
        .into_iter()
        .filter_map(|(src, group)| {
            let mut n = 0.0;
            let mut arc_sum: Option<Array2<f64>> = None;
            let mut ref_sum = 0.0;

            for ((_, outpin), table) in group {
                n += 1.0;
                arc_sum = Some(match arc_sum {
                    Some(sum) => sum + table,
                    None => table.clone(),
                });
                // Only the outputs this source actually drives contribute, so a
                // rail-private input is referenced against its own rail alone.
                if mode == ReferenceMode::PerOutput {
                    ref_sum += ref_arcs
                        .get(outpin)
                        .map_or(select(mean_ref)[col], |r| select(r)[col]);
                }
            }

            let reference = match mode {
                // Left as the exact original expression so this mode stays
                // bit-identical to the pooled behaviour it replaces.
                ReferenceMode::Pooled => select(mean_ref)[col],
                ReferenceMode::PerOutput => ref_sum / n,
            };

            arc_sum.map(|sum| (src, (sum / n).slice(s![.., col]).to_owned() - reference))
        })
        .collect()
}

/// Calculate setup constraints for all input pins
fn calculate_setup_constraints(
    cell_rise_arcs: &BTreeMap<(String, String), Array2<f64>>,
    cell_fall_arcs: &BTreeMap<(String, String), Array2<f64>>,
    ref_arcs: &BTreeMap<String, RefArc>,
    mean_ref: &RefArc,
    mode: ReferenceMode,
) -> (BTreeMap<String, Array1<f64>>, BTreeMap<String, Array1<f64>>) {
    let setup_rise =
        constraints_from_arcs(cell_rise_arcs, ref_arcs, mean_ref, mode, |r| &r.cell_rise);
    let setup_fall =
        constraints_from_arcs(cell_fall_arcs, ref_arcs, mean_ref, mode, |r| &r.cell_fall);

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

/// The knobs that decide how one cell is converted. They are chosen once per
/// run and travel together, so they are passed as a unit.
struct CellOptions<'a> {
    clock_name: &'a str,
    reset_name: &'a Regex,
    latch: bool,
    mode: ReferenceMode,
    when_merge: WhenMerge,
}

/// Process a single cell to add pseudo-synchronous timing
fn process_cell(
    cell: &mut Group,
    opts: &CellOptions,
    lib_name: &str,
    reports: &mut Vec<CellReport>,
) -> Option<String> {
    let CellOptions {
        clock_name,
        reset_name,
        latch,
        mode,
        when_merge,
    } = *opts;
    let cell_name = cell.name.clone();
    eprintln!("Processing cell {}", cell_name);

    let mut ref_arcs: BTreeMap<String, RefArc> = BTreeMap::new();
    let mut cell_rise_arcs: BTreeMap<(String, String), Array2<f64>> = BTreeMap::new();
    let mut cell_fall_arcs: BTreeMap<(String, String), Array2<f64>> = BTreeMap::new();

    // Phase 1: Collect every arc, folding each pin pair's `when` conditions into
    // one representative arc
    let mut accumulated: BTreeMap<(String, String), ArcAccumulator> = BTreeMap::new();
    // First-appearance order of each output's source pins, so the reference arc
    // is still chosen by the order the library declares them.
    let mut source_order: BTreeMap<String, Vec<String>> = BTreeMap::new();
    // Kept unreduced so the report can measure the model against every
    // condition, not just against the average it was built from.
    let mut raw_arcs: Vec<ConditionedArc> = Vec::new();

    for outpin in timing_leaves(cell, is_output_pin) {
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
                let sources = source_order.entry(outpin_name.clone()).or_default();
                if !sources.contains(&related_pin) {
                    sources.push(related_pin.clone());
                }

                raw_arcs.push(ConditionedArc {
                    source: related_pin.clone(),
                    output: outpin_name.clone(),
                    when: timing_group.simple_attribute("when").map(|v| v.string()),
                    timing_type: timing_group
                        .simple_attribute("timing_type")
                        .map(|v| v.expr()),
                    timing_sense: timing_group
                        .simple_attribute("timing_sense")
                        .map(|v| v.expr()),
                    cell_rise: timing_tables.cell_rise.clone(),
                    cell_fall: timing_tables.cell_fall.clone(),
                });

                accumulated
                    .entry((related_pin.clone(), outpin_name.clone()))
                    .or_insert_with(|| ArcAccumulator::new(when_merge))
                    .accumulate(timing_tables, &related_pin, outpin_name);
            }
        }
    }

    // Reduce each pin pair to its representative arc, then take each output's
    // reference from the first source whose average has all four tables.
    for ((related_pin, outpin_name), acc) in &accumulated {
        let Some(tables) = acc.result() else { continue };

        if let Some(cell_rise) = tables.cell_rise {
            cell_rise_arcs.insert((related_pin.clone(), outpin_name.clone()), cell_rise);
        }
        if let Some(cell_fall) = tables.cell_fall {
            cell_fall_arcs.insert((related_pin.clone(), outpin_name.clone()), cell_fall);
        }
    }

    for (outpin_name, sources) in &source_order {
        for related_pin in sources {
            let key = (related_pin.clone(), outpin_name.clone());
            let Some(tables) = accumulated.get(&key).and_then(|acc| acc.result()) else {
                continue;
            };

            if let Some(ref_arc) = select_reference_arc(related_pin, &tables) {
                eprintln!(
                    "  Pin {} selected as reference arc for output {}",
                    related_pin, outpin_name
                );
                ref_arcs.insert(outpin_name.clone(), ref_arc);
                break;
            }
        }
    }

    // Phase 2: Calculate mean reference arc for delays and constraints
    let mean_ref_arc = mean_reference_arc(ref_arcs.values().cloned())?;

    // Phase 3: Add pseudo timing to each output pin
    for outpin in timing_leaves_mut(cell, is_output_pin) {
        let outpin_name = &outpin.name;

        if let Some(output_transitions) = ref_arcs.get(outpin_name) {
            // Pooled hands every output the cell-wide mean delay; per-output lets
            // each keep the delay of its own reference arc.
            let delays = match mode {
                ReferenceMode::Pooled => &mean_ref_arc,
                ReferenceMode::PerOutput => output_transitions,
            };

            add_pseudo_timing_to_output_pin(
                outpin,
                clock_name,
                reset_name,
                output_transitions,
                delays,
                latch,
            );
        } else {
            eprintln!(
                "Failed to process outpin {} in cell {} of library {}: no usable reference arc could be found",
                outpin_name, cell_name, lib_name
            );
        }
    }

    // Phase 4: Calculate setup/hold constraints against the reference `mode` selects
    let ref_arc = mean_ref_arc;

    let (setup_rise, setup_fall) =
        calculate_setup_constraints(&cell_rise_arcs, &cell_fall_arcs, &ref_arcs, &ref_arc, mode);

    let (hold_rise, hold_fall) = calculate_hold_constraints(&setup_rise, &setup_fall);

    let references = References {
        per_output: &ref_arcs,
        mean: &ref_arc,
        mode,
    };
    let mut arc_errors: Vec<ArcError> = Vec::new();
    collect_arc_errors(
        &raw_arcs,
        &setup_rise,
        &references,
        &cell_name,
        &RISE,
        &mut arc_errors,
    );
    collect_arc_errors(
        &raw_arcs,
        &setup_fall,
        &references,
        &cell_name,
        &FALL,
        &mut arc_errors,
    );

    reports.push(CellReport {
        library: lib_name.to_owned(),
        cell: cell_name.clone(),
        when_merge,
        raw_arcs,
        cell_rise_arcs: cell_rise_arcs.clone(),
        cell_fall_arcs: cell_fall_arcs.clone(),
        ref_arcs: ref_arcs.clone(),
        mean_ref: ref_arc.clone(),
        setup_rise: setup_rise.clone(),
        setup_fall: setup_fall.clone(),
        arcs: arc_errors,
    });

    // Phase 5: Add constraints to every input the library characterised against
    // an output. A bundle takes them itself when the arcs name the bundle, or
    // delegates to its members when the arcs name the members.
    // The clock is never constrained against itself, and a pin the library never
    // characterised against an output has nothing to be constrained by.
    let has_constraints = |name: &str| {
        name != clock_name
            && !reset_name.is_match(name)
            && (setup_rise.contains_key(name) || setup_fall.contains_key(name))
    };

    for inpin in constraint_targets_mut(cell, &has_constraints) {
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

/// Process a library to convert latches to flip-flops or add pseudo-synchronous
/// timing, using the default [`ReferenceMode::PerOutput`] reference and a mean
/// merge of `when` conditions.
pub fn process_library(lib: &mut Group, clock_name: &str, reset_name: &Regex, latch: bool) {
    process_library_with_reference(
        lib,
        clock_name,
        reset_name,
        latch,
        ReferenceMode::PerOutput,
        WhenMerge::Mean,
    );
}

/// Process a library, choosing how the clock-to-output reference is drawn.
///
/// Returns a [`CellReport`] per processed cell, carrying the original arcs, the
/// reconstruction and its residual, so the cost of the chosen [`ReferenceMode`]
/// can be measured against the library it replaced.
pub fn process_library_with_reference(
    lib: &mut Group,
    clock_name: &str,
    reset_name: &Regex,
    latch: bool,
    mode: ReferenceMode,
    when_merge: WhenMerge,
) -> Vec<CellReport> {
    eprintln!("Processing library {}", lib.name);

    let opts = CellOptions {
        clock_name,
        reset_name,
        latch,
        mode,
        when_merge,
    };
    let mut reports: Vec<CellReport> = Vec::new();

    let mut lut_templates: HashSet<String> = HashSet::new();
    let lib_name = lib.name.clone();

    // Process each qualifying cell
    for cell in lib
        .iter_cells_mut()
        .filter(|x| cell_qualifies(x, clock_name))
    {
        if let Some(template_name) = process_cell(cell, &opts, &lib_name, &mut reports) {
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

    reports
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
                    vec![Attribute::Complex(vec![
                        Value::Float(0.1),
                        Value::Float(0.2),
                    ])],
                ),
                (
                    "index_2".to_owned(),
                    vec![Attribute::Complex(vec![
                        Value::Float(1.0),
                        Value::Float(2.0),
                    ])],
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
        let expected = Array2::from_shape_vec((2, 2), vec![11.0, 21.0, 12.0, 22.0]).unwrap();
        assert_eq!(got, expected);
    }

    // --- select_reference_arc ---------------------------------------------

    fn nine(base: f64) -> Array2<f64> {
        // 3x3 table whose middle row (index 1) is [base+3, base+4, base+5]
        Array2::from_shape_vec((3, 3), (0..9).map(|i| base + i as f64).collect::<Vec<_>>()).unwrap()
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
        let arc =
            Array2::from_shape_vec((3, 3), vec![0.0, 25.0, 0.0, 0.0, 35.0, 0.0, 0.0, 45.0, 0.0])
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

        // With one output there is nothing to pool, so both modes must agree.
        let ref_arcs: BTreeMap<String, RefArc> =
            BTreeMap::from([("Q".to_owned(), ref_arc.clone())]);

        for mode in [ReferenceMode::Pooled, ReferenceMode::PerOutput] {
            let (setup_rise, setup_fall) = calculate_setup_constraints(
                &cell_rise_arcs,
                &cell_fall_arcs,
                &ref_arcs,
                &ref_arc,
                mode,
            );

            // [25,35,45] - 20 = [5,15,25]
            assert_eq!(
                setup_rise["D"],
                Array1::from(vec![5.0, 15.0, 25.0]),
                "{:?}",
                mode
            );
            assert!(setup_fall.is_empty(), "{:?}", mode);

            // hold = -setup
            let (hold_rise, hold_fall) = calculate_hold_constraints(&setup_rise, &setup_fall);
            assert_eq!(
                hold_rise["D"],
                Array1::from(vec![-5.0, -15.0, -25.0]),
                "{:?}",
                mode
            );
            assert!(hold_fall.is_empty(), "{:?}", mode);
        }
    }

    /// A source driving one rail of a dual-rail cell must be referenced against
    /// that rail alone under `PerOutput`, and against both rails under `Pooled`.
    #[test]
    fn per_output_references_a_source_against_only_the_outputs_it_drives() {
        let col = 0usize;

        // Column 0 of each arc is [100]; a 1x1 table keeps the arithmetic visible.
        let arc = |v: f64| Array2::from_shape_vec((1, 1), vec![v]).unwrap();
        let refarc = |delay: f64| RefArc {
            col,
            row: 0,
            related_pin: "CK".to_owned(),
            lut_template: "T".to_owned(),
            rise_trans: Array1::from(vec![0.0]),
            fall_trans: Array1::from(vec![0.0]),
            cell_rise: Array1::from(vec![delay]),
            cell_fall: Array1::from(vec![0.0]),
        };

        // Rail 1 is fast (ref 10), rail 2 slow (ref 30); pooled mean is 20.
        let ref_arcs: BTreeMap<String, RefArc> = BTreeMap::from([
            ("Q1".to_owned(), refarc(10.0)),
            ("Q2".to_owned(), refarc(30.0)),
        ]);
        let mean_ref = mean_reference_arc(ref_arcs.values().cloned()).unwrap();
        assert_eq!(mean_ref.cell_rise[col], 20.0);

        let cell_rise_arcs: BTreeMap<(String, String), Array2<f64>> = BTreeMap::from([
            // D1 is rail-private: it drives Q1 only.
            (("D1".to_owned(), "Q1".to_owned()), arc(100.0)),
            // S is shared: it drives both rails.
            (("S".to_owned(), "Q1".to_owned()), arc(100.0)),
            (("S".to_owned(), "Q2".to_owned()), arc(100.0)),
        ]);
        let empty: BTreeMap<(String, String), Array2<f64>> = BTreeMap::new();

        let (pooled, _) = calculate_setup_constraints(
            &cell_rise_arcs,
            &empty,
            &ref_arcs,
            &mean_ref,
            ReferenceMode::Pooled,
        );
        let (per_output, _) = calculate_setup_constraints(
            &cell_rise_arcs,
            &empty,
            &ref_arcs,
            &mean_ref,
            ReferenceMode::PerOutput,
        );

        // Pooled charges both sources the cell-wide mean: 100 - 20.
        assert_eq!(pooled["D1"], Array1::from(vec![80.0]));
        assert_eq!(pooled["S"], Array1::from(vec![80.0]));

        // PerOutput charges the rail-private source its own rail: 100 - 10.
        assert_eq!(per_output["D1"], Array1::from(vec![90.0]));
        // The shared source drives every output, so its driven mean is the
        // pooled mean and it is left unchanged.
        assert_eq!(per_output["S"], Array1::from(vec![80.0]));
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
        liberty_parser::parse_lib(
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

    // --- reconstruction report ---------------------------------------------

    /// The report is the only evidence of how much the split costs, and it was
    /// once removed unnoticed by a refactor because nothing exercised it. These
    /// tests exist so that cannot happen silently again.
    /// A flat cell whose output is genuinely characterised against its input, so
    /// there is a real arc for the split to be measured against.
    fn reportable_lib() -> Liberty {
        bundle_lib(format!(
            r#"
    pin(D) {{ direction: input; }}
    pin(Q) {{
      direction: output;
      function: "IQ";
      {}
    }}"#,
            arc("D", 1.0)
        ))
    }

    #[test]
    fn every_processed_cell_yields_a_reconstruction_report() {
        let reset = Regex::new("(R|S)N?").unwrap();

        for mode in [ReferenceMode::Pooled, ReferenceMode::PerOutput] {
            let mut lib = reportable_lib();
            let reports = process_library_with_reference(
                &mut lib[0],
                "G",
                &reset,
                false,
                mode,
                WhenMerge::Mean,
            );

            let report = reports
                .iter()
                .find(|r| r.cell == "DUT")
                .unwrap_or_else(|| panic!("no report for the processed cell in {:?}", mode));

            assert_eq!(report.library, "bundle_test");
            assert!(
                !report.cell_rise_arcs.is_empty(),
                "the original arcs must be reported, not just the residual"
            );
            assert!(!report.ref_arcs.is_empty(), "{:?}", mode);
            assert!(!report.setup_rise.is_empty(), "{:?}", mode);
            assert!(
                !report.arcs.is_empty(),
                "at least one arc must be measured in {:?}",
                mode
            );
        }
    }

    /// The reported reconstruction really is setup + clock-to-output delay added
    /// back together, and the residual really is its distance from the original.
    #[test]
    fn reported_reconstruction_is_the_outer_sum_of_setup_and_delay() {
        let mut lib = reportable_lib();
        let reports = process_library_with_reference(
            &mut lib[0],
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
        );
        let report = reports.iter().find(|r| r.cell == "DUT").unwrap();
        assert!(!report.arcs.is_empty(), "fixture must produce arcs");

        for arc in &report.arcs {
            let setup = match arc.edge {
                "rise" => &report.setup_rise[&arc.source],
                _ => &report.setup_fall[&arc.source],
            };
            let refarc = &report.ref_arcs[&arc.output];
            let delay = match arc.edge {
                "rise" => &refarc.cell_rise,
                _ => &refarc.cell_fall,
            };

            assert_eq!(arc.reconstructed, restore_arc(setup, delay), "{:?}", arc);
            assert_eq!(
                arc.error,
                arc.reconstructed.clone() - &arc.original,
                "{:?}",
                arc
            );
            // Exact at the reference column, but only because this fixture
            // characterises each arc once. The setup is derived from the mean
            // over a pin pair's `when` conditions, so it can only reproduce a
            // raw condition exactly when there is nothing to average -- see
            // when_averaging_error_shows_up_against_the_raw_conditions.
            assert!(arc.when.is_none(), "fixture must be unconditioned");
            let col = refarc.col;
            for r in 0..arc.error.nrows() {
                assert!(
                    arc.error[[r, col]].abs() < 1e-9,
                    "residual must vanish on the reference column, got {}",
                    arc.error[[r, col]]
                );
            }
        }
    }

    /// Measuring against the raw conditions is what makes the `when` reduction
    /// visible: the model holds one setup per pin, so it cannot sit exactly on
    /// two conditions that disagree. Measuring against the mean would hide
    /// precisely the error the mean introduced.
    #[test]
    fn when_averaging_error_shows_up_against_the_raw_conditions() {
        // Two conditions for the same pin pair, deliberately far apart.
        let body = format!(
            r#"
    pin(D) {{ direction: input; }}
    pin(Q) {{
      direction: output;
      function: "IQ";
      {}
      {}
    }}"#,
            conditioned_arc("D", 1.0, "(A * B)"),
            conditioned_arc("D", 100.0, "(A * !B)")
        );

        let mut lib = bundle_lib(body);
        let reports = process_library_with_reference(
            &mut lib[0],
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
        );
        let report = reports.iter().find(|r| r.cell == "DUT").unwrap();

        // Both conditions are measured, and each is labelled by its own `when`.
        let rise: Vec<&ArcError> = report
            .arcs
            .iter()
            .filter(|a| a.edge == "rise" && a.source == "D")
            .collect();
        assert_eq!(rise.len(), 2, "each condition must be measured separately");
        let mut whens: Vec<&str> = rise.iter().filter_map(|a| a.when.as_deref()).collect();
        whens.sort();
        assert_eq!(whens, vec!["(A * !B)", "(A * B)"]);

        // The raw conditions differ, so one setup cannot satisfy both: at least
        // one must carry a residual the averaged comparison would have hidden.
        let worst = rise.iter().fold(0.0_f64, |m, a| m.max(a.max_abs()));
        assert!(
            worst > 1.0,
            "averaging two disagreeing conditions must leave a residual, got {}",
            worst
        );

        // And the residual is genuinely against the raw table, not the mean.
        for a in &rise {
            assert_eq!(a.error, a.reconstructed.clone() - &a.original);
        }
    }

    // --- when-condition averaging -----------------------------------------

    fn tables(cell_rise: Option<f64>, cell_fall: Option<f64>, trans: Option<f64>) -> TimingTables {
        let fill = |v: f64| Array2::from_shape_vec((2, 2), vec![v; 4]).unwrap();
        TimingTables {
            lut_template: "T".to_owned(),
            cell_rise: cell_rise.map(fill),
            cell_fall: cell_fall.map(fill),
            rise_trans: trans.map(fill),
            fall_trans: trans.map(fill),
        }
    }

    #[test]
    fn when_conditions_are_averaged_per_family_not_last_wins() {
        let mut acc = ArcAccumulator::new(WhenMerge::Mean);

        // Three conditions characterise cell_rise (10, 20, 60 -> mean 30) but
        // only two of them characterise cell_fall (100, 200 -> mean 150), which
        // is the combinational_rise / combinational_fall split. Each family must
        // divide by its own count, and neither may take the last value (60/200).
        acc.accumulate(tables(Some(10.0), Some(100.0), None), "D", "Q");
        acc.accumulate(tables(Some(20.0), None, None), "D", "Q");
        acc.accumulate(tables(Some(60.0), Some(200.0), None), "D", "Q");

        let mean = acc.result().expect("a template was recorded");
        assert_eq!(mean.cell_rise.unwrap()[[0, 0]], 30.0);
        assert_eq!(mean.cell_fall.unwrap()[[0, 0]], 150.0);
        // A family no condition characterised stays absent.
        assert!(mean.rise_trans.is_none());
        assert!(mean.fall_trans.is_none());
    }

    #[test]
    fn a_condition_on_a_different_table_shape_is_ignored_rather_than_panicking() {
        let mut acc = ArcAccumulator::new(WhenMerge::Mean);
        acc.accumulate(tables(Some(10.0), None, None), "D", "Q");

        let odd = TimingTables {
            lut_template: "T".to_owned(),
            cell_rise: Some(Array2::from_shape_vec((1, 3), vec![99.0; 3]).unwrap()),
            cell_fall: None,
            rise_trans: None,
            fall_trans: None,
        };
        acc.accumulate(odd, "D", "Q");

        // The mismatched condition is dropped, leaving the first untouched.
        assert_eq!(acc.result().unwrap().cell_rise.unwrap()[[0, 0]], 10.0);
    }

    /// The merge strategy is what the library-side spread has to be handled
    /// with, so each mode must do exactly what it claims, elementwise.
    #[test]
    fn when_merge_selects_mean_min_or_max_elementwise() {
        // Two conditions crossing over: the first is larger in cell_rise, the
        // second larger in cell_fall, so a mode that picked whole tables rather
        // than elements would be caught.
        let cases = [
            (WhenMerge::Mean, 30.0, 150.0),
            (WhenMerge::Min, 10.0, 100.0),
            (WhenMerge::Max, 50.0, 200.0),
        ];

        for (merge, want_rise, want_fall) in cases {
            let mut acc = ArcAccumulator::new(merge);
            acc.accumulate(tables(Some(10.0), Some(200.0), None), "D", "Q");
            acc.accumulate(tables(Some(50.0), Some(100.0), None), "D", "Q");

            let got = acc.result().expect("a template was recorded");
            assert_eq!(
                got.cell_rise.unwrap()[[0, 0]],
                want_rise,
                "cell_rise under {:?}",
                merge
            );
            assert_eq!(
                got.cell_fall.unwrap()[[0, 0]],
                want_fall,
                "cell_fall under {:?}",
                merge
            );
        }
    }

    // --- bundle traversal --------------------------------------------------

    /// Four timing tables for one arc, so `select_reference_arc` accepts it.
    fn arc(related_pin: &str, base: f64) -> String {
        format!(
            r#"
        timing() {{
          related_pin: "{}";
          cell_rise(T) {{ values("{}, {}", "{}, {}"); }}
          cell_fall(T) {{ values("{}, {}", "{}, {}"); }}
          rise_transition(T) {{ values("0.1, 0.2", "0.3, 0.4"); }}
          fall_transition(T) {{ values("0.11, 0.21", "0.31, 0.41"); }}
        }}"#,
            related_pin,
            base,
            base + 1.0,
            base + 2.0,
            base + 3.0,
            base + 0.5,
            base + 1.5,
            base + 2.5,
            base + 3.5,
        )
    }

    /// Same as [`arc`] but characterised under a `when` condition.
    fn conditioned_arc(related_pin: &str, base: f64, when: &str) -> String {
        arc(related_pin, base).replace(
            &format!(r#"related_pin: "{}";"#, related_pin),
            &format!(
                "related_pin: \"{}\";\n          when: \"{}\";",
                related_pin, when
            ),
        )
    }

    fn bundle_lib(cell_body: String) -> Liberty {
        liberty_parser::parse_lib(&format!(
            r#"
library(bundle_test) {{
  lu_table_template(T) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}
  cell(DUT) {{
    latch_bank(IQ,IQN,2) {{ enable: "G"; data_in: "D"; }}
    pin(G) {{ direction: input; clock: true; }}
{}
  }}
}}"#,
            cell_body
        ))
        .expect("parse bundle fixture")
    }

    fn member<'a>(cell: &'a Group, bundle: &str, pin: &str) -> &'a Group {
        cell.iter_subgroups_of_type("bundle")
            .find(|b| b.name == bundle)
            .unwrap_or_else(|| panic!("bundle {} not found", bundle))
            .iter_subgroups_of_type("pin")
            .find(|p| p.name == pin)
            .unwrap_or_else(|| panic!("member {} not found in bundle {}", pin, bundle))
    }

    fn has_arc(group: &Group, timing_type: &str, related_pin: &str) -> bool {
        group.iter_subgroups_of_type("timing").any(|t| {
            t.simple_attribute("timing_type")
                .map(|tt| tt.expr() == timing_type)
                .unwrap_or(false)
                && t.simple_attribute("related_pin")
                    .map(|rp| rp.string() == related_pin)
                    .unwrap_or(false)
        })
    }

    /// Constraint groups must carry a table; an empty one is not usable timing.
    fn constraint_is_populated(group: &Group, timing_type: &str) -> bool {
        group
            .iter_subgroups_of_type("timing")
            .filter(|t| {
                t.simple_attribute("timing_type")
                    .map(|tt| tt.expr() == timing_type)
                    .unwrap_or(false)
            })
            .any(|t| {
                t.iter_subgroups_of_type("rise_constraint").next().is_some()
                    || t.iter_subgroups_of_type("fall_constraint").next().is_some()
            })
    }

    /// A bundle that only contains member pins delegates to them: the arcs live
    /// in the members, name the members as `related_pin`, and each member is
    /// processed as its own output.
    #[test]
    fn bundle_members_take_their_own_constraints() {
        let body = format!(
            r#"
    bundle(D) {{
      members(D1, D2);
      direction: input;
      pin(D1) {{ capacitance: 0.001; }}
      pin(D2) {{ capacitance: 0.001; }}
    }}
    bundle(Q) {{
      members(Q1, Q2);
      direction: output;
      function: "IQ";
      pin(Q1) {{ {} }}
      pin(Q2) {{ {} }}
    }}"#,
            arc("D1", 1.0),
            arc("D2", 2.0)
        );

        let mut lib = bundle_lib(body);
        process_library(&mut lib[0], "G", &Regex::new("(R|S)N?").unwrap(), false);
        let cell = lib[0].get_cell("DUT").expect("DUT");

        // Each member output carries its own pseudo arc against the clock.
        for q in ["Q1", "Q2"] {
            assert!(
                has_arc(member(cell, "Q", q), "rising_edge", "G"),
                "{} should gain a clock arc",
                q
            );
        }

        // Each member input carries its own populated constraints.
        for d in ["D1", "D2"] {
            let pin = member(cell, "D", d);
            assert_eq!(
                pin.simple_attribute("nextstate_type").unwrap().expr(),
                "data",
                "{} should be marked as data",
                d
            );
            for tt in ["setup_rising", "hold_rising"] {
                assert!(
                    constraint_is_populated(pin, tt),
                    "{} should carry a populated {}",
                    d,
                    tt
                );
            }
        }

        // The container itself is not a pin and takes nothing.
        let d_bundle = cell
            .iter_subgroups_of_type("bundle")
            .find(|b| b.name == "D")
            .unwrap();
        assert!(d_bundle.simple_attribute("nextstate_type").is_none());
        assert_eq!(d_bundle.iter_subgroups_of_type("timing").count(), 0);

        // The clock is never constrained against itself.
        let g = cell.get_pin("G").expect("G");
        assert!(g.simple_attribute("nextstate_type").is_none());
        assert_eq!(g.iter_subgroups_of_type("timing").count(), 0);
    }

    /// A bundle that owns its timing arcs is the leaf itself, and the arcs name
    /// the bundle rather than its members. This is the form the ASCEND libraries
    /// use, and it must keep being handled at bundle level.
    #[test]
    fn bundle_owning_its_arcs_is_processed_as_a_single_pin() {
        let body = format!(
            r#"
    bundle(D) {{
      members(D0, D1);
      direction: input;
      pin(D0) {{ capacitance: 0.001; }}
      pin(D1) {{ capacitance: 0.001; }}
    }}
    bundle(Q) {{
      members(Q0, Q1);
      direction: output;
      function: "IQ";
      {}
      pin(Q0) {{ max_capacitance: 0.05; }}
      pin(Q1) {{ max_capacitance: 0.05; }}
    }}"#,
            arc("D", 1.0)
        );

        let mut lib = bundle_lib(body);
        process_library(&mut lib[0], "G", &Regex::new("(R|S)N?").unwrap(), false);
        let cell = lib[0].get_cell("DUT").expect("DUT");

        let q_bundle = cell
            .iter_subgroups_of_type("bundle")
            .find(|b| b.name == "Q")
            .unwrap();
        assert!(
            has_arc(q_bundle, "rising_edge", "G"),
            "the bundle itself should gain the clock arc"
        );
        // Members stay untouched -- they never held arcs to begin with.
        for q in ["Q0", "Q1"] {
            assert_eq!(
                member(cell, "Q", q)
                    .iter_subgroups_of_type("timing")
                    .count(),
                0
            );
        }

        let d_bundle = cell
            .iter_subgroups_of_type("bundle")
            .find(|b| b.name == "D")
            .unwrap();
        assert_eq!(
            d_bundle.simple_attribute("nextstate_type").unwrap().expr(),
            "data"
        );
        for tt in ["setup_rising", "hold_rising"] {
            assert!(
                constraint_is_populated(d_bundle, tt),
                "bundle D needs {}",
                tt
            );
        }
        for d in ["D0", "D1"] {
            assert!(member(cell, "D", d)
                .simple_attribute("nextstate_type")
                .is_none());
        }
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

    /// The engine performs no I/O of its own: the reconstruction report is
    /// returned as data and written by the caller to a path it chooses.
    ///
    /// This once asserted that the `pseudosync.txt` writer was "dead code", which
    /// was false -- it was live until the report facility was dropped by accident
    /// in a refactor. The assertion is kept because writing to the process CWD
    /// from library code is the behaviour that made that loss invisible, but it
    /// now guards a deliberate design rather than ratifying an accident.
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
