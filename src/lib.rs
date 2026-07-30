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
mod arcs_tests;
#[cfg(test)]
mod emit_tests;
#[cfg(test)]
mod engine_tests;
#[cfg(test)]
mod liberty_io_tests;
#[cfg(test)]
mod pins_tests;
#[cfg(test)]
mod report_tests;
