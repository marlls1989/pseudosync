//! Timing arcs: how they are averaged, merged across `when` conditions,
//! reduced to a reference, and restored from the pseudo-flop split.

use crate::conditions::ClassId;
use crate::templates::{axis_values, Axes};
use liberty_parser::{ast::Value, liberty::Group};
use ndarray::prelude::*;
use std::collections::BTreeMap;

/// How the several `when`-conditioned arcs of one pin pair are merged into the
/// single arc the pseudo-flop model can carry.
///
/// A cell characterised over many operating states describes one transition once
/// per state, and those states can differ for real physical reasons -- a device
/// sitting at a different depth in the stack conducts differently -- so the
/// spread between them is data, not noise. The model has room for one arc, and
/// which one it should be depends on what the result is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WhenMerge {
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

/// How the clock-to-output reference delay is chosen when a cell drives several
/// outputs.
///
/// The pseudo-flop model splits each original input-to-output arc into a setup
/// constraint plus a clock-to-output delay, so the same reference must be used on
/// both sides for `setup(D) + clk→Q` to reconstruct `delay(D→Q)`. The two modes
/// differ in how wide that reference is drawn, which matters for cells whose
/// outputs are independent rails rather than views of one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReferenceMode {
    /// One reference shared by the whole cell: the mean across every output.
    /// Both the emitted delays and the constraints use it, so every output is
    /// given the same clock-to-output delay. This was the original behaviour.
    Pooled,
    /// Each output keeps its own reference for its emitted delay, and each input
    /// is constrained against the mean reference of only the outputs it actually
    /// drives. For a dual-rail bundle this reduces to running the algorithm
    /// independently per rail, while a control input shared by both rails is
    /// still referenced against both.
    PerOutput,
    /// The default. Each post-settled state of each output keeps its own reference, and the
    /// emitted delays and checks are conditioned on it.
    ///
    /// The three modes are one ladder, differing only in how finely the reference is
    /// keyed: pooled keys it on the cell, per-output on (output, edge), per-state on
    /// (output, edge, post-settled end state). Arcs that collide on one key share a
    /// reference, which is why the finest of the three is per-*state* and not
    /// per-arc: two arcs describing the same end state describe one clock-to-output
    /// delay, however many `when` conditions the library spelled it under.
    PerState,
}

impl std::str::FromStr for ReferenceMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pooled" => Ok(ReferenceMode::Pooled),
            "per-output" => Ok(ReferenceMode::PerOutput),
            "per-state" => Ok(ReferenceMode::PerState),
            other => Err(format!(
                "unknown reference mode {:?}, expected \"pooled\", \"per-output\" or \"per-state\"",
                other
            )),
        }
    }
}

/// How widely one reference is drawn, and so which arcs share it.
///
/// This is the key [`ReferenceMode`] selects: pooled and per-output draw one
/// reference for a whole output, so every arc of it is [`Scope::Whole`]; per-state
/// draws one per post-settled state, so an arc is filed under the state it leaves
/// the cell in and a `when`-less arc under the catch-all it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Scope {
    /// One reference for the whole output, covering every state of it.
    Whole,
    /// One post-settled state, named by the class its condition falls in.
    State(ClassId),
    /// Whatever the conditioned states do not cover: the `when`-less arcs.
    ///
    /// Ordered last so that a catch-all group is emitted after the conditioned ones,
    /// which is the order Liberty's `default_timing` is read in.
    CatchAll,
}

impl Scope {
    /// The scope one edge of one arc is filed under, or `None` where per-state can
    /// name no state for that edge.
    ///
    /// Only per-state distinguishes: under the other two every arc of an output
    /// shares one reference, which is what keeps those modes exactly what they were.
    pub(crate) fn of(mode: ReferenceMode, class: Option<ClassId>, whenless: bool) -> Option<Self> {
        match mode {
            ReferenceMode::Pooled | ReferenceMode::PerOutput => Some(Scope::Whole),
            // A `when`-less arc covers whatever the conditioned ones do not, so
            // putting it through the state construction would claim it holds only
            // while its own source pin is at one value.
            ReferenceMode::PerState if whenless => Some(Scope::CatchAll),
            ReferenceMode::PerState => class.map(Scope::State),
        }
    }

    /// Whether the two edge halves drawn from a merged arc are a complete reference
    /// at this scope.
    ///
    /// A whole-output reference must describe both directions, because it stands for
    /// every state the output is ever in. A per-state one need not: a
    /// `combinational_rise` group carries the rise families alone, and that is the
    /// ordinary shape a conditioned arc is characterised in -- requiring both edges
    /// there would refuse the majority of them.
    fn accepts(&self, rise: &Option<EdgeRef>, fall: &Option<EdgeRef>) -> bool {
        match self {
            Scope::Whole => rise.is_some() && fall.is_some(),
            Scope::State(_) | Scope::CatchAll => rise.is_some() || fall.is_some(),
        }
    }
}

/// The way an input pin logically affects an output pin, as Liberty's
/// `timing_sense` states it (RM p.328).
///
/// Liberty spells these `positive_unate`, `negative_unate` and `non_unate`; the
/// shared "unate" is dropped from the first two because a set of variants that all
/// carry one word says the word in every use of the type and none of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TimingSense {
    Positive,
    Negative,
    NonUnate,
}

impl TimingSense {
    /// The three spellings Liberty gives the attribute, and nothing else. An
    /// unrecognised value is `None` rather than a guess: it is exactly as
    /// uninformative about the input's direction as a missing attribute.
    pub(crate) fn from_expr(text: &str) -> Option<Self> {
        match text {
            "positive_unate" => Some(TimingSense::Positive),
            "negative_unate" => Some(TimingSense::Negative),
            "non_unate" => Some(TimingSense::NonUnate),
            _ => None,
        }
    }
}

/// Which way a pin is moving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Transition {
    Rise,
    Fall,
}

impl Transition {
    /// The word a report captions this direction with.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Transition::Rise => "rise",
            Transition::Fall => "fall",
        }
    }
}

/// The direction the INPUT pin was moving in, for an arc whose OUTPUT moved in
/// `output`. `None` where the sense does not determine one.
///
/// This is the one place the derivation is decided; classification, the constraint
/// routing and the report all call it rather than re-deriving it.
///
/// The derivation. `timing_sense` describes the way an input pin logically affects
/// an output pin (RM p.328): `positive_unate` combines an *incoming* rise with a
/// *local* rise, `negative_unate` combines an incoming rise with a local fall
/// (RM p.329), where "incoming" is the input side and "local" is this cell's own
/// output. The output's direction is the table family the values were read from --
/// `cell_rise` is paired with `rise_transition`, and a transition is the output's
/// own slew, so both name the output. Inverting that pairing gives the input's
/// direction: under `positive_unate` it matches the output's, under
/// `negative_unate` it is the opposite, and under `non_unate` the same input edge
/// can drive the output either way, so nothing determines it.
///
/// Nothing here rests on the arrow orientation of RM Table 21.
pub(crate) fn input_transition(sense: TimingSense, output: Transition) -> Option<Transition> {
    match sense {
        TimingSense::Positive => Some(output),
        TimingSense::Negative => Some(match output {
            Transition::Rise => Transition::Fall,
            Transition::Fall => Transition::Rise,
        }),
        TimingSense::NonUnate => None,
    }
}

/// Where in a characterised table the one value standing for a whole axis is read.
///
/// A 2-D arc is slew x load, and the split reduces it to a load-indexed profile plus
/// a slew-indexed one. Something has to stand in for the axis being collapsed, and
/// the choice is not forced by the model: the middle sample is one measurement the
/// library actually made, the mean uses every measurement but corresponds to none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Anchor {
    /// The middle row, column or element of the table. The original behaviour: every
    /// number emitted is one the library characterised.
    Middle,
    /// The mean over the axis being collapsed. Uses every measurement, at the cost of
    /// standing for no single characterised point.
    Average,
}

impl std::str::FromStr for Anchor {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "middle" => Ok(Anchor::Middle),
            "average" => Ok(Anchor::Average),
            other => Err(format!(
                "unknown anchor {:?}, expected \"middle\" or \"average\"",
                other
            )),
        }
    }
}

/// Which half of the split carries the constant the two are separated around.
///
/// `delay(A→Z) = propagation(G→Z) + setup(A→G)` fixes the sum, not how the constant
/// at the anchor point is divided between the halves. The residual is a different
/// matter: each arc's own crossing is folded in where its reference is drawn
/// (`select_reference_arc`), but a source pin's constraint is then averaged over
/// every output that pin drives (`constraints_from_arcs`, grouped on `(src, scope)`),
/// and the two operations do not commute. `Setup` is exact at the anchor point for
/// every arc; `Prop` adds a constant bias — that arc's crossing minus the mean
/// crossing across the group — on any pin driving two or more outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OffsetPlacement {
    /// The constant stays in the setup constraint, which is therefore referred to the
    /// anchor point of the propagation delay. The original behaviour.
    Setup,
    /// The constant is folded into the propagation delay instead, leaving the setup
    /// constraint the arc's own slew profile.
    Prop,
}

impl std::str::FromStr for OffsetPlacement {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "setup" => Ok(OffsetPlacement::Setup),
            "prop" => Ok(OffsetPlacement::Prop),
            other => Err(format!(
                "unknown offset placement {:?}, expected \"setup\" or \"prop\"",
                other
            )),
        }
    }
}

/// The load-indexed profile a table is collapsed to: the row the [`Anchor`] selects.
///
/// `Middle` takes the row at `len_of(Axis(0)) / 2`, the same integer division the
/// reference's own `row` is taken with; `Average` takes the mean down the rows.
fn prop_profile(t: &Array2<f64>, anchor: Anchor) -> Array1<f64> {
    match anchor {
        Anchor::Middle => t.slice(s![t.len_of(Axis(0)) / 2, ..]).to_owned(),
        Anchor::Average => t.mean_axis(Axis(0)).unwrap_or_else(|| {
            panic!("internal: a characterisation table reached the split with no rows")
        }),
    }
}

/// The slew-indexed profile a table is collapsed to: the column the [`Anchor`] selects.
///
/// `Middle` takes the column at `len_of(Axis(1)) / 2`, the same integer division the
/// reference's own `col` is taken with; `Average` takes the mean across the columns.
pub(crate) fn slew_profile(t: &Array2<f64>, anchor: Anchor) -> Array1<f64> {
    match anchor {
        Anchor::Middle => t.slice(s![.., t.len_of(Axis(1)) / 2]).to_owned(),
        Anchor::Average => t.mean_axis(Axis(1)).unwrap_or_else(|| {
            panic!("internal: a characterisation table reached the split with no columns")
        }),
    }
}

/// The single value where the two profiles meet: the constant the split is taken
/// around, counted once so it is not charged to both halves.
fn crossing(t: &Array2<f64>, anchor: Anchor) -> f64 {
    match anchor {
        Anchor::Middle => t[[t.len_of(Axis(0)) / 2, t.len_of(Axis(1)) / 2]],
        Anchor::Average => t.mean().unwrap_or_else(|| {
            panic!("internal: a characterisation table reached the split with no elements")
        }),
    }
}

/// One output edge's half of a reference arc.
///
/// The clock-to-output delay the model emits for this edge, the output slew that
/// pairs with it, and the constant the constraint half is offset by. Bundled because
/// the three are read off one table family: a profile paired with another family's
/// crossing would subtract a fall delay from a rise arc.
///
/// A transition table is a 1-D profile and never carries an offset of its own, so
/// `crossing` belongs to `delay` alone.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EdgeRef {
    pub(crate) delay: Array1<f64>,
    pub(crate) transition: Array1<f64>,
    pub(crate) crossing: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RefArc {
    pub(crate) col: usize,
    pub(crate) row: usize,
    pub(crate) related_pin: String,
    pub(crate) lut_template: String,
    /// How the profiles below were read off the 2-D tables, so a report never
    /// captions an averaged reference with a row index it did not use.
    pub(crate) anchor: Anchor,
    /// The two output edges, each present only where the arcs this was drawn from
    /// carried both of that edge's tables. A whole-output reference always holds
    /// both; a per-state one holds whichever edges its state was characterised on.
    pub(crate) rise: Option<EdgeRef>,
    pub(crate) fall: Option<EdgeRef>,
}

/// The references a cell's constraints and delays were drawn against, keyed by the
/// output and the scope each was drawn at.
pub(crate) struct References<'a> {
    pub(crate) per_output: &'a BTreeMap<(String, Scope), RefArc>,
    pub(crate) mean: &'a RefArc,
    pub(crate) mode: ReferenceMode,
}

impl References<'_> {
    /// The clock-to-output delay this mode actually emits for `output` at `scope`, or
    /// `None` if nothing there was converted.
    ///
    /// Skipped-ness is mode-independent, and `per_output` holds exactly the converted
    /// (output, scope) pairs, so presence there is the one place convertedness is
    /// decided. Pooled then yields the cell-wide mean for a *converted* output --
    /// which is the mode's whole point -- and nothing for a skipped one. Handing the
    /// mean to a skipped output would charge it a delay drawn from outputs it has
    /// nothing to do with, describing a path nothing characterised.
    pub(crate) fn delay_for(&self, output: &str, scope: &Scope) -> Option<&RefArc> {
        let own = self.per_output.get(&(output.to_owned(), *scope))?;

        match self.mode {
            ReferenceMode::Pooled => Some(self.mean),
            ReferenceMode::PerOutput | ReferenceMode::PerState => Some(own),
        }
    }
}

/// Timing tables extracted from a timing group
#[derive(Debug, Clone)]
pub(crate) struct TimingTables {
    pub(crate) lut_template: String,
    /// The `timing_sense` the group declares, where it declares one this tool
    /// recognises. `None` covers both an absent attribute and an unrecognised
    /// spelling, which say equally little about the input's direction.
    pub(crate) sense: Option<TimingSense>,
    /// The axes the table group declares for itself, which override the named
    /// template's. `None` where it declares none and the template's stand.
    pub(crate) slews: Option<Vec<f64>>,
    pub(crate) loads: Option<Vec<f64>>,
    pub(crate) cell_rise: Option<Array2<f64>>,
    pub(crate) cell_fall: Option<Array2<f64>>,
    pub(crate) rise_trans: Option<Array2<f64>>,
    pub(crate) fall_trans: Option<Array2<f64>>,
}

/// Running mean of one table family across arcs that share a (related_pin,
/// output) pair.
#[derive(Debug, Clone)]
pub(crate) struct TableAccumulator {
    sum: Option<Array2<f64>>,
    n: f64,
    merge: WhenMerge,
}

impl TableAccumulator {
    pub(crate) fn new(merge: WhenMerge) -> Self {
        Self {
            sum: None,
            n: 0.0,
            merge,
        }
    }

    pub(crate) fn add(
        &mut self,
        table: Array2<f64>,
        family: &str,
        related_pin: &str,
        outpin: &str,
    ) {
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

    pub(crate) fn result(&self) -> Option<Array2<f64>> {
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
pub(crate) struct ArcAccumulator {
    lut_template: Option<String>,
    /// The axes of the arcs merged here, and whether they all agreed. Labelling a
    /// merged table with axes only some of its conditions were indexed on would
    /// caption it with numbers it was not measured at, so disagreement drops the
    /// labels rather than picks a winner.
    axes: Option<Axes>,
    axes_agree: bool,
    cell_rise: TableAccumulator,
    cell_fall: TableAccumulator,
    rise_trans: TableAccumulator,
    fall_trans: TableAccumulator,
}

impl ArcAccumulator {
    pub(crate) fn new(merge: WhenMerge) -> Self {
        Self {
            lut_template: None,
            axes: None,
            axes_agree: true,
            cell_rise: TableAccumulator::new(merge),
            cell_fall: TableAccumulator::new(merge),
            rise_trans: TableAccumulator::new(merge),
            fall_trans: TableAccumulator::new(merge),
        }
    }

    pub(crate) fn accumulate(&mut self, tables: TimingTables, related_pin: &str, outpin: &str) {
        if self.lut_template.is_none() {
            self.lut_template = Some(tables.lut_template);
        }

        let arc_axes = Axes {
            slew: tables.slews,
            load: tables.loads,
        };
        match &self.axes {
            None => self.axes = Some(arc_axes),
            Some(seen) if *seen != arc_axes => self.axes_agree = false,
            Some(_) => {}
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

    pub(crate) fn result(&self) -> Option<TimingTables> {
        let (slews, loads) = match (&self.axes, self.axes_agree) {
            (Some(axes), true) => (axes.slew.clone(), axes.load.clone()),
            _ => (None, None),
        };

        Some(TimingTables {
            lut_template: self.lut_template.clone()?,
            // The merged arc is what a reference is drawn from, and a reference is
            // a clock-to-output quantity that no input's direction bears on. The
            // constraints are routed per arc, before this merge, so nothing
            // downstream of it asks.
            sense: None,
            slews,
            loads,
            cell_rise: self.cell_rise.result(),
            cell_fall: self.cell_fall.result(),
            rise_trans: self.rise_trans.result(),
            fall_trans: self.fall_trans.result(),
        })
    }
}

/// Calculate the mean of timing tables from multiple groups
fn mean_timingtable<'a, I>(groups: I) -> Option<Array2<f64>>
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

/// Add one edge into a running sum, counting the arcs that carried it.
///
/// The crossing is summed alongside the arrays it was read from, so the mean
/// reference's offset is the mean of the offsets rather than a value re-read off a
/// table that no longer exists.
fn accumulate_edge(sum: &mut Option<EdgeRef>, n: &mut f64, edge: Option<EdgeRef>) {
    let Some(edge) = edge else { return };
    *n += 1.0;
    match sum {
        None => *sum = Some(edge),
        Some(sum) => {
            sum.delay += &edge.delay;
            sum.transition += &edge.transition;
            sum.crossing += edge.crossing;
        }
    }
}

/// Calculate the mean reference arc from multiple RefArc instances
///
/// Each edge is counted separately, because a per-state reference carries only the
/// edges its state was characterised on: dividing an edge by the number of *arcs*
/// would scale it down by the ones that never described it. Where every arc holds
/// both edges -- which is what a whole-output reference requires -- both counts are
/// the arc count, and this is the mean it has always been.
pub(crate) fn mean_reference_arc<I>(ref_arcs: I) -> Option<RefArc>
where
    I: IntoIterator<Item = RefArc>,
{
    // Guaranteed by the caller: a cell whose outputs draw references from different
    // lookup templates is refused at cell scope before the mean is taken. These
    // therefore fail as a defect in pseudosync -- a guarantee relaxed somewhere
    // upstream -- and never as a complaint about the input.
    const CHECKED: &str = "internal: every arc this cell transforms was checked \
                           for a common domain before the mean was taken";

    let mut arcs = ref_arcs.into_iter();
    let mut first = arcs.next()?;

    let (mut rise, mut rises) = (None, 0.0);
    let (mut fall, mut falls) = (None, 0.0);
    accumulate_edge(&mut rise, &mut rises, first.rise.take());
    accumulate_edge(&mut fall, &mut falls, first.fall.take());

    for arc in arcs {
        assert_eq!(first.col, arc.col, "{}", CHECKED);
        assert_eq!(first.row, arc.row, "{}", CHECKED);
        assert_eq!(first.anchor, arc.anchor, "{}", CHECKED);
        assert_eq!(first.lut_template, arc.lut_template, "{}", CHECKED);
        accumulate_edge(&mut rise, &mut rises, arc.rise);
        accumulate_edge(&mut fall, &mut falls, arc.fall);
    }

    let scaled = |edge: EdgeRef, n: f64| EdgeRef {
        delay: edge.delay / n,
        transition: edge.transition / n,
        crossing: edge.crossing / n,
    };

    Some(RefArc {
        col: first.col,
        row: first.row,
        related_pin: first.related_pin,
        lut_template: first.lut_template,
        anchor: first.anchor,
        rise: rise.map(|e| scaled(e, rises)),
        fall: fall.map(|e| scaled(e, falls)),
    })
}

/// Restore a 2D timing arc from 1D slew and capacitance dependent arrays
pub(crate) fn restore_arc(
    slew_dependent: &Array1<f64>,
    capacitance_dependent: &Array1<f64>,
) -> Array2<f64> {
    let cap: Array2<f64> =
        Array::ones((slew_dependent.len(), capacitance_dependent.len())) * capacitance_dependent;
    let slw: Array2<f64> =
        Array::ones((capacitance_dependent.len(), slew_dependent.len())) * slew_dependent;

    cap + slw.t()
}

/// The four table families a complete reference is drawn from, and so the only ones
/// whose lookup template this conversion ever reads.
pub(crate) const REFERENCE_FAMILIES: [&str; 4] = [
    "cell_rise",
    "cell_fall",
    "rise_transition",
    "fall_transition",
];

/// The domain each present reference family in one timing group is characterised on: the
/// lookup template it names, and the number of rows and columns its table carries — which
/// is the length of that template's two index lists.
///
/// Differing dimensions **are** a differing template, whatever the name says. Two tables
/// cannot be transformed together unless they are indexed the same way, so the dimensions
/// are part of the domain's identity rather than a separate test alongside the name. That
/// is also why this reads them through `mean_timingtable`, the same reader the conversion
/// itself uses: the dimensions checked are then necessarily the ones used.
pub(crate) fn arc_domains(timing_group: &Group) -> Vec<(String, (usize, usize))> {
    timing_group
        .iter_subgroups()
        .filter(|g| REFERENCE_FAMILIES.contains(&g.type_.as_str()))
        .filter_map(|g| mean_timingtable(vec![g]).map(|t| (g.name.clone(), t.dim())))
        .collect()
}

/// Extract timing tables from a timing group
pub(crate) fn extract_timing_tables_from_arc(timing_group: &Group) -> Option<TimingTables> {
    let mut lut_template = None;
    // Whichever family claims the template also supplies the axes, so the two can
    // never come from different tables.
    let mut axes: Option<Axes> = None;
    let mut claim = |group: Option<&&Group>, lut_template: &mut Option<String>| {
        if let (Some(group), None) = (group, &lut_template) {
            *lut_template = Some(group.name.clone());
            axes = Some(Axes {
                slew: axis_values(group, "index_1"),
                load: axis_values(group, "index_2"),
            });
        }
    };

    let (cell_rise_groups, others): (Vec<&Group>, Vec<&Group>) = timing_group
        .iter_subgroups()
        .partition(|g| g.type_ == "cell_rise");
    claim(cell_rise_groups.first(), &mut lut_template);
    let cell_rise = mean_timingtable(cell_rise_groups);

    let (cell_fall_groups, others): (Vec<&Group>, Vec<&Group>) =
        others.into_iter().partition(|g| g.type_ == "cell_fall");
    claim(cell_fall_groups.first(), &mut lut_template);
    let cell_fall = mean_timingtable(cell_fall_groups);

    let (rise_trans_groups, others): (Vec<&Group>, Vec<&Group>) = others
        .into_iter()
        .partition(|g| g.type_ == "rise_transition");
    claim(rise_trans_groups.first(), &mut lut_template);
    let rise_trans = mean_timingtable(rise_trans_groups);

    let fall_trans_groups: Vec<&Group> = others
        .into_iter()
        .filter(|g| g.type_ == "fall_transition")
        .collect();
    claim(fall_trans_groups.first(), &mut lut_template);
    let fall_trans = mean_timingtable(fall_trans_groups);

    // Require at least one timing table to be present
    if cell_rise.is_none() && cell_fall.is_none() && rise_trans.is_none() && fall_trans.is_none() {
        return None;
    }

    let (slews, loads) = axes.map_or((None, None), |a| (a.slew, a.load));

    // Read structurally rather than through `Value::expr`, which panics on a value
    // spelled as a quoted string. An unrecognised spelling is `None`, and the caller
    // treats that exactly as it treats an absent attribute.
    let sense = timing_group
        .simple_attribute("timing_sense")
        .and_then(|v| match v {
            Value::Expression(text) | Value::String(text) => TimingSense::from_expr(text),
            _ => None,
        });

    Some(TimingTables {
        lut_template: lut_template?,
        sense,
        slews,
        loads,
        cell_rise,
        cell_fall,
        rise_trans,
        fall_trans,
    })
}

/// Select a reference arc from timing tables, collapsing each family at `anchor`.
///
/// An edge is drawn only where both of its tables are present -- a delay with no
/// transition beside it describes half a path -- and the `scope` then decides
/// whether the edges drawn are enough of a reference. Returns None where they are
/// not.
pub(crate) fn select_reference_arc(
    related_pin: &str,
    timing_tables: &TimingTables,
    anchor: Anchor,
    placement: OffsetPlacement,
    scope: Scope,
) -> Option<RefArc> {
    // Placement is applied once, here, so that everything downstream reads one
    // already-decided pair: the delay this edge emits and the constant the
    // constraint half still owes. `Prop` folds the constant into the delay and
    // leaves nothing to subtract; the sum of the two halves is the same either way.
    // The residual is not: this folds in the arc's own crossing, but
    // `constraints_from_arcs` averages a source pin's reference over every output it
    // drives, so the two do not commute. `Setup` lands exactly on the anchor point
    // for every arc; `Prop` shifts the residual by that arc's crossing minus the
    // group's mean crossing wherever a pin drives more than one output.
    let edge = |delays: Option<&Array2<f64>>, transitions: Option<&Array2<f64>>| {
        let (delays, transitions) = (delays?, transitions?);
        let delay = prop_profile(delays, anchor);
        let crossing = crossing(delays, anchor);
        let transition = prop_profile(transitions, anchor);
        Some(match placement {
            OffsetPlacement::Setup => EdgeRef {
                delay,
                transition,
                crossing,
            },
            OffsetPlacement::Prop => EdgeRef {
                delay: delay - crossing,
                transition,
                crossing: 0.0,
            },
        })
    };

    let rise = edge(
        timing_tables.cell_rise.as_ref(),
        timing_tables.rise_trans.as_ref(),
    );
    let fall = edge(
        timing_tables.cell_fall.as_ref(),
        timing_tables.fall_trans.as_ref(),
    );
    if !scope.accepts(&rise, &fall) {
        return None;
    }

    // The reference element is located on whichever delay family this scope
    // accepted. Both name the same domain -- a cell whose arcs sit on more than one
    // is refused before any reference is drawn -- so which of the two is read from
    // decides nothing but which one is present.
    let sized = timing_tables
        .cell_rise
        .as_ref()
        .or(timing_tables.cell_fall.as_ref())?;

    Some(RefArc {
        col: sized.len_of(Axis(1)) / 2,
        row: sized.len_of(Axis(0)) / 2,
        anchor,
        lut_template: timing_tables.lut_template.clone(),
        related_pin: related_pin.to_owned(),
        rise,
        fall,
    })
}

#[cfg(test)]
mod tests {
    //! Behaviour of the `arcs` module: arc averaging, restoration, reference
    //! selection and `when` merging.

    use super::*;
    use crate::conditions::{collision_classes, Condition}; // Test-only; a scope's class ids are minted by the real classifier rather than written down.
    use indexmap::IndexMap;
    use liberty_parser::{
        ast::Value,
        liberty::{Attribute, Group},
    };

    // --- mean_timingtable --------------------------------------------------

    /// Killed by: `mean_timingtable` divided the summed tables by `n + 1.0` instead of `n`.
    #[test]
    fn mean_timingtable_averages_the_groups_elementwise() {
        // Two 2x2 tables whose elementwise mean is [[3, 4], [5, 6]]: every element
        // of the second is four larger than its counterpart in the first, so a
        // wrong divisor or a dropped group moves every result element.
        let table = |values: [[f64; 2]; 2]| {
            let mut g = Group::new("cell_rise", "test_template");
            g.attributes.insert(
                "values".to_owned(),
                vec![Attribute::Complex(
                    values
                        .into_iter()
                        .map(|row| Value::FloatGroup(row.to_vec()))
                        .collect(),
                )],
            );
            g
        };
        let first = table([[1.0, 2.0], [3.0, 4.0]]);
        let second = table([[5.0, 6.0], [7.0, 8.0]]);

        let mean = mean_timingtable(vec![&first, &second]).expect("two tables to average");

        assert_eq!(mean.shape(), &[2, 2]);
        assert_eq!(
            mean,
            Array2::from_shape_vec((2, 2), vec![3.0, 4.0, 5.0, 6.0]).unwrap()
        );
    }

    // --- mean_reference_arc ------------------------------------------------

    /// Killed by: `mean_reference_arc` left one family unnormalised -- `x.rise_trans /= 1.0` in place of `/= n`.
    #[test]
    fn mean_reference_arc_averages_all_four_table_families() {
        // The second arc is twice the first everywhere, so each family's mean is
        // 1.5x the first -- and every family carries its own values, so a family
        // that was left unnormalised, or filled from a neighbour, is visible.
        let arc = |scale: f64| RefArc {
            col: 1,
            row: 1,
            related_pin: "A".to_owned(),
            lut_template: "template".to_owned(),
            anchor: Anchor::Middle,
            rise: Some(EdgeRef {
                delay: Array1::from(vec![1.0, 2.0, 3.0]) * scale,
                transition: Array1::from(vec![0.1, 0.2, 0.3]) * scale,
                crossing: 2.0 * scale,
            }),
            fall: Some(EdgeRef {
                delay: Array1::from(vec![1.5, 2.5, 3.5]) * scale,
                transition: Array1::from(vec![0.15, 0.25, 0.35]) * scale,
                crossing: 2.5 * scale,
            }),
        };

        let mean = mean_reference_arc(vec![arc(1.0), arc(2.0)]).expect("two arcs to average");

        // The index of the reference element and its provenance are carried over
        // unchanged; only the tables are averaged.
        assert_eq!(mean.col, 1);
        assert_eq!(mean.row, 1);
        assert_eq!(mean.related_pin, "A");
        assert_eq!(mean.lut_template, "template");

        let close = |got: &Array1<f64>, want: [f64; 3], family: &str| {
            for (i, want) in want.into_iter().enumerate() {
                assert!(
                    (got[i] - want).abs() < 1e-10,
                    "{}[{}] is {}, expected {}",
                    family,
                    i,
                    got[i],
                    want
                );
            }
        };
        let (rise, fall) = (
            mean.rise.as_ref().expect("both arcs carry a rise edge"),
            mean.fall.as_ref().expect("both arcs carry a fall edge"),
        );
        close(&rise.transition, [0.15, 0.3, 0.45], "rise transition");
        close(&fall.transition, [0.225, 0.375, 0.525], "fall transition");
        close(&rise.delay, [1.5, 3.0, 4.5], "rise delay");
        close(&fall.delay, [2.25, 3.75, 5.25], "fall delay");
    }

    /// The crossing is a scalar, so it is not covered by the array assertions
    /// above -- and it is the value the constraint half is offset by, so a mean
    /// reference carrying one arc's crossing would charge every input that arc's
    /// offset.
    ///
    /// Killed by: `mean_reference_arc` summed the crossings but left them
    /// undivided -- `x.rise.crossing /= 1.0` in place of `/= n`. That also reddens
    /// two downstream users of the pooled reference, but the sibling above,
    /// `mean_reference_arc_averages_all_four_table_families`, stays green under it
    /// -- which is what shows the crossing is pinned here and nowhere else.
    #[test]
    fn mean_reference_arc_averages_the_crossings_alongside_the_arrays() {
        // Crossings 2 and 4 for rise, 2.5 and 5 for fall: means 3 and 3.75, both
        // distinct from either input and from each other.
        let arc = |scale: f64| RefArc {
            col: 1,
            row: 1,
            related_pin: "A".to_owned(),
            lut_template: "template".to_owned(),
            anchor: Anchor::Middle,
            rise: Some(EdgeRef {
                delay: Array1::from(vec![1.0, 2.0, 3.0]),
                transition: Array1::from(vec![0.1, 0.2, 0.3]),
                crossing: 2.0 * scale,
            }),
            fall: Some(EdgeRef {
                delay: Array1::from(vec![1.5, 2.5, 3.5]),
                transition: Array1::from(vec![0.15, 0.25, 0.35]),
                crossing: 2.5 * scale,
            }),
        };

        let mean = mean_reference_arc(vec![arc(1.0), arc(2.0)]).expect("two arcs to average");

        let (rise, fall) = (
            mean.rise.as_ref().expect("both arcs carry a rise edge"),
            mean.fall.as_ref().expect("both arcs carry a fall edge"),
        );
        assert!((rise.crossing - 3.0).abs() < 1e-10, "{:?}", rise);
        assert!((fall.crossing - 3.75).abs() < 1e-10, "{:?}", fall);
    }

    // --- References::delay_for ---------------------------------------------

    /// A reference arc whose rise delay is the one number under test.
    fn scoped_refarc(delay: f64) -> RefArc {
        RefArc {
            col: 0,
            row: 0,
            related_pin: "A".to_owned(),
            lut_template: "T".to_owned(),
            anchor: Anchor::Middle,
            rise: Some(EdgeRef {
                delay: Array1::from(vec![delay]),
                transition: Array1::from(vec![0.0]),
                crossing: delay,
            }),
            fall: Some(EdgeRef {
                delay: Array1::from(vec![0.0]),
                transition: Array1::from(vec![0.0]),
                crossing: 0.0,
            }),
        }
    }

    /// The rise delay `delay_for` answered with, for the assertions below.
    fn answered(references: &References, output: &str, scope: Scope) -> Option<f64> {
        references
            .delay_for(output, &scope)
            .map(|r| r.rise.as_ref().expect("the fixture draws both edges").delay[0])
    }

    /// A skipped output has no clock-to-output delay, under either mode.
    ///
    /// `per_output` holds exactly the converted (output, scope) pairs, so presence
    /// there is the one place convertedness is decided -- and that decision does not
    /// depend on the mode. Pooling settles which reference a *converted* output is
    /// given; it is not a licence to invent one for an output that supplies none.
    /// Handing a skipped output the cell-wide mean would charge it a delay drawn from
    /// outputs it has nothing to do with, describing a path nothing characterised.
    ///
    /// Killed by: `delay_for` restored to answering `Pooled` with `Some(self.mean)`
    /// unconditionally, so the skipped output was handed the cell-wide mean.
    #[test]
    fn a_skipped_output_has_no_delay_under_either_mode() {
        // One converted output, and a cell-wide mean distinguishable from it so that
        // handing the mean out where it should not be is visible.
        let per_output: BTreeMap<(String, Scope), RefArc> =
            BTreeMap::from([(("Q".to_owned(), Scope::Whole), scoped_refarc(3.0))]);
        let mean = scoped_refarc(13.0);

        for mode in [ReferenceMode::Pooled, ReferenceMode::PerOutput] {
            let references = References {
                per_output: &per_output,
                mean: &mean,
                mode,
            };

            // The converted output is answered, with the reference this mode emits:
            // its own under per-output, the cell-wide mean under pooled.
            let expected = match mode {
                ReferenceMode::Pooled => 13.0,
                _ => 3.0,
            };
            assert_eq!(
                answered(&references, "Q", Scope::Whole),
                Some(expected),
                "{:?}",
                mode
            );

            // The skipped output is answered with nothing, in both modes.
            assert_eq!(
                answered(&references, "QN", Scope::Whole),
                None,
                "a skipped output has no delay under {:?}",
                mode
            );
        }
    }

    /// Per-state answers each state with the reference drawn for that state, and a
    /// state nothing was drawn for with nothing.
    ///
    /// The scope is part of what was converted, not a filter over it: two states of
    /// one output are two references, and an output converted in one state has
    /// nothing to say about a state it was never characterised in.
    ///
    /// Killed by: `delay_for` looked its key up as `(output, Scope::Whole)` instead of
    /// at the scope asked for, so both states were answered with the first one's
    /// reference and the uncharacterised state was answered at all.
    /// `a_skipped_output_has_no_delay_under_either_mode` stays green under it,
    /// because every key there is `Scope::Whole` already.
    #[test]
    fn per_state_answers_each_state_with_its_own_reference() {
        // Real class ids, minted by the classifier over three distinct conditions:
        // `ClassId` is opaque, and widening it so a test could write one down would
        // be widening visibility to suit a test.
        let conditions: Vec<Condition> = ["A", "B", "C"]
            .iter()
            .map(|t| Condition::parse(t).expect("a pin name is a condition"))
            .collect();
        let classes = collision_classes(&conditions);

        // Two states of one output, an order of magnitude apart, and a third that
        // was never drawn.
        let per_output: BTreeMap<(String, Scope), RefArc> = BTreeMap::from([
            (
                ("Q".to_owned(), Scope::State(classes[0])),
                scoped_refarc(3.0),
            ),
            (
                ("Q".to_owned(), Scope::State(classes[1])),
                scoped_refarc(30.0),
            ),
        ]);
        let mean = scoped_refarc(13.0);
        let references = References {
            per_output: &per_output,
            mean: &mean,
            mode: ReferenceMode::PerState,
        };

        assert_eq!(
            answered(&references, "Q", Scope::State(classes[0])),
            Some(3.0)
        );
        assert_eq!(
            answered(&references, "Q", Scope::State(classes[1])),
            Some(30.0)
        );
        assert_eq!(answered(&references, "Q", Scope::State(classes[2])), None);
        assert_eq!(answered(&references, "Q", Scope::CatchAll), None);
    }

    // --- restore_arc -------------------------------------------------------

    /// Killed by: `restore_arc` built its `cap` term from `Array::zeros` rather than `Array::ones`, dropping the load-dependent half of the outer sum.
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

    fn all_nine() -> TimingTables {
        TimingTables {
            slews: None,
            loads: None,
            lut_template: "T".to_owned(),
            sense: Some(TimingSense::Positive),
            cell_rise: Some(nine(0.0)),
            cell_fall: Some(nine(100.0)),
            rise_trans: Some(nine(200.0)),
            fall_trans: Some(nine(300.0)),
        }
    }

    /// The rise and fall halves of a reference the fixture draws both of.
    fn both_edges(arc: &RefArc) -> (&EdgeRef, &EdgeRef) {
        (
            arc.rise.as_ref().expect("a rise edge"),
            arc.fall.as_ref().expect("a fall edge"),
        )
    }

    /// Killed by: `select_reference_arc` took `col` as `sized.len_of(Axis(1)) * 0` instead of `/ 2`.
    #[test]
    fn select_reference_arc_picks_the_middle_row_and_column() {
        let arc = select_reference_arc(
            "CK",
            &all_nine(),
            Anchor::Middle,
            OffsetPlacement::Setup,
            Scope::Whole,
        )
        .expect("all four tables present");
        assert_eq!(arc.row, 1);
        assert_eq!(arc.col, 1);
        assert_eq!(arc.related_pin, "CK");
        assert_eq!(arc.lut_template, "T");
        assert_eq!(arc.anchor, Anchor::Middle);
        let (rise, fall) = both_edges(&arc);
        // middle row of cell_rise == [3,4,5]
        assert_eq!(rise.delay, Array1::from(vec![3.0, 4.0, 5.0]));
        assert_eq!(fall.delay, Array1::from(vec![103.0, 104.0, 105.0]));
    }

    /// `Prop` moves the constant out of the constraint and into the delay, and does
    /// nothing else: the two halves still sum to the same arc.
    ///
    /// Derivation from the model. `nine(0)` is `[[0,1,2],[3,4,5],[6,7,8]]`, so under
    /// `Middle` the rise profile is row 1, `[3,4,5]`, and the crossing is the middle
    /// element, `4`. `Setup` emits that profile whole and leaves `4` for the
    /// constraint to subtract; `Prop` emits `[3,4,5] - 4 = [-1,0,1]` and leaves
    /// nothing. `setup + delay` is `x - 4 + [3,4,5]` either way.
    ///
    /// A transition is the output's own slew, not a delay referred to anything, so it
    /// is the same profile under both placements.
    ///
    /// Killed by: `select_reference_arc`'s `Prop` arm kept `crossing` rather than
    /// zeroing it, so the constant was charged to both halves at once. Observed to
    /// redden this test alone -- no other test asks for `Prop`.
    #[test]
    fn prop_placement_moves_the_crossing_into_the_delay_and_leaves_none_to_subtract() {
        let at = |placement| {
            select_reference_arc("CK", &all_nine(), Anchor::Middle, placement, Scope::Whole)
                .expect("all four tables present")
        };
        let setup = at(OffsetPlacement::Setup);
        let prop = at(OffsetPlacement::Prop);
        let (setup_rise, setup_fall) = both_edges(&setup);
        let (prop_rise, prop_fall) = both_edges(&prop);

        assert_eq!(setup_rise.delay, Array1::from(vec![3.0, 4.0, 5.0]));
        assert_eq!(setup_rise.crossing, 4.0);

        assert_eq!(prop_rise.delay, Array1::from(vec![-1.0, 0.0, 1.0]));
        assert_eq!(prop_rise.crossing, 0.0);
        // cell_fall's middle row is [103,104,105] and its crossing 104.
        assert_eq!(prop_fall.delay, Array1::from(vec![-1.0, 0.0, 1.0]));
        assert_eq!(prop_fall.crossing, 0.0);

        // The output's slew is not a delay and moves with neither placement.
        assert_eq!(prop_rise.transition, setup_rise.transition);
        assert_eq!(prop_fall.transition, setup_fall.transition);
    }

    /// One edge's tables, for the completeness assertions below: a delay family and
    /// the transition that pairs with it, and nothing of the other edge.
    fn one_edge(edge: Transition) -> TimingTables {
        let mut tables = all_nine();
        match edge {
            Transition::Rise => {
                tables.cell_fall = None;
                tables.fall_trans = None;
            }
            Transition::Fall => {
                tables.cell_rise = None;
                tables.rise_trans = None;
            }
        }
        tables
    }

    /// A whole-output reference requires all four families; a per-state one requires
    /// one complete edge pair.
    ///
    /// Derivation from the model. A whole-output reference stands for every state the
    /// output is ever in, so it has to describe both directions of it. A per-state
    /// reference describes one state, and a state characterised as a
    /// `combinational_rise` group carries the rise families alone -- so requiring both
    /// edges there would refuse the ordinary shape a conditioned arc comes in. What
    /// neither accepts is half an edge: a delay with no transition beside it, or a
    /// transition with no delay, describes half a path under any scope.
    ///
    /// Killed by: `Scope::accepts` answered `rise.is_some() || fall.is_some()` for `Scope::Whole` too, so a whole-output reference was drawn from one edge alone. That also reddens the two engine tests about a skipped output, which is the same rule seen through the whole conversion; `per_state_emits_one_conditioned_clock_arc_per_state_and_a_catch_all_last` stays green under it, which is what shows this test pins the whole-output arm rather than the per-state one.
    #[test]
    fn a_scope_decides_how_complete_a_reference_has_to_be() {
        let drawn = |tables: &TimingTables, scope| {
            select_reference_arc("CK", tables, Anchor::Middle, OffsetPlacement::Setup, scope)
        };
        let class = collision_classes(&[Condition::parse("A").expect("parse")])[0];

        for scope in [Scope::Whole, Scope::State(class), Scope::CatchAll] {
            assert!(
                drawn(&all_nine(), scope).is_some(),
                "all four families are a reference at {:?}",
                scope
            );
        }

        for edge in [Transition::Rise, Transition::Fall] {
            let tables = one_edge(edge);
            assert!(
                drawn(&tables, Scope::Whole).is_none(),
                "a whole-output reference needs both edges, {:?} alone is not one",
                edge
            );
            for scope in [Scope::State(class), Scope::CatchAll] {
                let arc = drawn(&tables, scope)
                    .unwrap_or_else(|| panic!("{:?} alone is a reference at {:?}", edge, scope));
                assert_eq!(arc.rise.is_some(), edge == Transition::Rise, "{:?}", scope);
                assert_eq!(arc.fall.is_some(), edge == Transition::Fall, "{:?}", scope);
            }
        }

        // Half an edge is no edge under any scope: a delay with no transition beside
        // it describes half a path.
        let mut half = all_nine();
        half.rise_trans = None;
        half.cell_fall = None;
        half.fall_trans = None;
        for scope in [Scope::Whole, Scope::State(class), Scope::CatchAll] {
            assert!(
                drawn(&half, scope).is_none(),
                "a delay with no transition is not an edge at {:?}",
                scope
            );
        }
    }

    // --- anchor helpers ----------------------------------------------------

    /// A 2x3 table whose rows and columns are all distinct, so a helper that
    /// collapsed the wrong axis, or picked the wrong index along the right one,
    /// cannot land on the expected values by coincidence.
    ///
    ///     [[1, 2, 3],
    ///      [7, 11, 15]]
    fn oblong() -> Array2<f64> {
        Array2::from_shape_vec((2, 3), vec![1.0, 2.0, 3.0, 7.0, 11.0, 15.0]).unwrap()
    }

    /// Killed by: `prop_profile`'s `Average` arm averaged `Axis(1)` rather than `Axis(0)`, collapsing the load axis where the slew axis was asked for. Observed to redden this test alone; mutating the `Middle` arm reddens six neighbours with it, because every default-anchor run reads that arm.
    #[test]
    fn prop_profile_is_the_middle_row_or_the_mean_down_the_rows() {
        // 2 rows, so the middle row is index 2/2 = 1: [7, 11, 15].
        assert_eq!(
            prop_profile(&oblong(), Anchor::Middle),
            Array1::from(vec![7.0, 11.0, 15.0])
        );
        // Column means: (1+7)/2, (2+11)/2, (3+15)/2.
        assert_eq!(
            prop_profile(&oblong(), Anchor::Average),
            Array1::from(vec![4.0, 6.5, 9.0])
        );
    }

    /// Killed by: `slew_profile`'s `Average` arm averaged `Axis(0)` rather than `Axis(1)`, collapsing the slew axis where the load axis was asked for. Observed to redden this test alone.
    #[test]
    fn slew_profile_is_the_middle_column_or_the_mean_across_the_columns() {
        // 3 columns, so the middle column is index 3/2 = 1: [2, 11].
        assert_eq!(
            slew_profile(&oblong(), Anchor::Middle),
            Array1::from(vec![2.0, 11.0])
        );
        // Row means: (1+2+3)/3 = 2, (7+11+15)/3 = 11. Deliberately equal to the
        // middle column here, so this pair alone would not discriminate -- the
        // Average assertion below on a table where they differ is what does.
        assert_eq!(
            slew_profile(&oblong(), Anchor::Average),
            Array1::from(vec![2.0, 11.0])
        );
        // [[0, 0, 3], [0, 0, 3]]: middle column 0, row mean 1. The two disagree.
        let skewed = Array2::from_shape_vec((2, 3), vec![0.0, 0.0, 3.0, 0.0, 0.0, 3.0]).unwrap();
        assert_eq!(
            slew_profile(&skewed, Anchor::Middle),
            Array1::from(vec![0.0, 0.0])
        );
        assert_eq!(
            slew_profile(&skewed, Anchor::Average),
            Array1::from(vec![1.0, 1.0])
        );
    }

    /// The crossing is where the two profiles meet, so it must be the element the
    /// row profile and the column profile share -- not a value read off either
    /// axis independently.
    ///
    /// Killed by: `crossing`'s `Average` arm returned `t.sum()` instead of `t.mean()`. Observed to redden this test alone; mutating the `Middle` arm to `[[0, t.len_of(Axis(1)) / 2]]` reddens nine neighbours with it, because every default-anchor run subtracts that value.
    #[test]
    fn crossing_is_the_middle_element_or_the_grand_mean() {
        // prop_profile(Middle) is row 1 = [7, 11, 15] and slew_profile(Middle) is
        // column 1 = [2, 11]; they meet at 11.
        assert_eq!(crossing(&oblong(), Anchor::Middle), 11.0);
        // (1+2+3+7+11+15)/6 = 39/6 = 6.5.
        assert_eq!(crossing(&oblong(), Anchor::Average), 6.5);
    }

    // --- timing_sense --------------------------------------------------------

    /// Killed by: `TimingSense::from_expr` answered `None` for `"non_unate"`. Observed to redden this test alone -- the engine treats an unrecognised spelling exactly as `non_unate`, so its skip still fires with the same message; mutating the `"positive_unate"` arm instead reddens three routing tests with it.
    #[test]
    fn timing_sense_from_expr_maps_the_three_liberty_spellings_and_nothing_else() {
        assert_eq!(
            TimingSense::from_expr("positive_unate"),
            Some(TimingSense::Positive)
        );
        assert_eq!(
            TimingSense::from_expr("negative_unate"),
            Some(TimingSense::Negative)
        );
        assert_eq!(
            TimingSense::from_expr("non_unate"),
            Some(TimingSense::NonUnate)
        );

        // Anything else says nothing about the input's direction, and is not
        // guessed at.
        assert_eq!(TimingSense::from_expr("positive"), None);
        assert_eq!(TimingSense::from_expr(""), None);
    }

    /// The direction the input was moving in, given the direction the output was.
    ///
    /// Derivation, from the model rather than from the code. `timing_sense`
    /// describes how an input pin logically affects an output pin (RM p.328):
    /// `positive_unate` combines an incoming rise with a local rise, and
    /// `negative_unate` combines an incoming rise with a local fall (RM p.329),
    /// where "incoming" is the input side and "local" this cell's output. A delay
    /// family names the OUTPUT -- `cell_rise` pairs with `rise_transition`, and a
    /// transition is the output's own slew -- so reading that pairing backwards
    /// gives the input: the same direction under `positive_unate`, the opposite
    /// under `negative_unate`. `non_unate` says the same input edge can drive the
    /// output either way, so it determines nothing.
    ///
    /// Killed by: `input_transition`'s `NonUnate` arm returned `Some(output)`. Observed to redden this test alone: the engine skips a non-unate arc before it would ever ask, so only this test reads that arm. Mutating the `Negative` arm reddens the engine's routing tests with it.
    #[test]
    fn input_transition_is_the_output_direction_read_back_through_the_sense() {
        use TimingSense::*;

        assert_eq!(
            input_transition(Positive, Transition::Rise),
            Some(Transition::Rise)
        );
        assert_eq!(
            input_transition(Positive, Transition::Fall),
            Some(Transition::Fall)
        );

        assert_eq!(
            input_transition(Negative, Transition::Rise),
            Some(Transition::Fall)
        );
        assert_eq!(
            input_transition(Negative, Transition::Fall),
            Some(Transition::Rise)
        );

        assert_eq!(input_transition(NonUnate, Transition::Rise), None);
        assert_eq!(input_transition(NonUnate, Transition::Fall), None);
    }

    // --- Anchor::from_str / OffsetPlacement::from_str -----------------------

    /// Killed by: `Anchor::from_str` mapped `"middle"` to `Anchor::Average`.
    #[test]
    fn anchor_from_str_maps_each_spelling() {
        assert_eq!("middle".parse::<Anchor>(), Ok(Anchor::Middle));
        assert_eq!("average".parse::<Anchor>(), Ok(Anchor::Average));

        let err = "bogus".parse::<Anchor>().unwrap_err();
        assert!(
            err.contains("unknown anchor"),
            "error message was {:?}",
            err
        );
    }

    /// Killed by: `OffsetPlacement::from_str` mapped `"setup"` to `OffsetPlacement::Prop`.
    #[test]
    fn offset_placement_from_str_maps_each_spelling() {
        assert_eq!(
            "setup".parse::<OffsetPlacement>(),
            Ok(OffsetPlacement::Setup)
        );
        assert_eq!("prop".parse::<OffsetPlacement>(), Ok(OffsetPlacement::Prop));

        let err = "bogus".parse::<OffsetPlacement>().unwrap_err();
        assert!(
            err.contains("unknown offset placement"),
            "error message was {:?}",
            err
        );
    }

    // --- extract_timing_tables_from_arc: lut_template precedence ----------

    /// A timing table subgroup carrying a distinct template name, with just
    /// enough of a "values" attribute for `mean_timingtable` to accept it.
    fn table_group(type_: &str, template_name: &str) -> Group {
        Group {
            type_: type_.to_owned(),
            name: template_name.to_owned(),
            attributes: IndexMap::from([(
                "values".to_owned(),
                vec![Attribute::Complex(vec![Value::FloatGroup(vec![1.0])])],
            )]),
            subgroups: vec![],
        }
    }

    fn timing_group(subgroups: Vec<Group>) -> Group {
        Group {
            type_: "timing".to_owned(),
            name: "".to_owned(),
            attributes: IndexMap::new(),
            subgroups,
        }
    }

    /// Killed by: `extract_timing_tables_from_arc`'s cell_rise arm matched `(Some(group), Some(_))`, so cell_rise never claimed the template.
    #[test]
    fn lut_template_prefers_cell_rise_over_the_others() {
        let g = timing_group(vec![
            table_group("cell_rise", "CR_TPL"),
            table_group("cell_fall", "CF_TPL"),
            table_group("rise_transition", "RT_TPL"),
            table_group("fall_transition", "FT_TPL"),
        ]);
        let tt = extract_timing_tables_from_arc(&g).expect("tables present");
        assert_eq!(tt.lut_template, "CR_TPL");
    }

    /// Killed by: `extract_timing_tables_from_arc` joined its `cell_rise.is_none() && ...` guard with `||`, so any missing family refused the arc.
    #[test]
    fn lut_template_falls_back_to_cell_fall_when_cell_rise_is_absent() {
        let g = timing_group(vec![
            table_group("cell_fall", "CF_TPL"),
            table_group("rise_transition", "RT_TPL"),
            table_group("fall_transition", "FT_TPL"),
        ]);
        let tt = extract_timing_tables_from_arc(&g).expect("tables present");
        assert_eq!(tt.lut_template, "CF_TPL");
    }

    /// Killed by: `extract_timing_tables_from_arc` joined its `cell_rise.is_none() && ...` guard with `||`, so any missing family refused the arc.
    #[test]
    fn lut_template_falls_back_to_rise_transition_when_only_transitions_present() {
        let g = timing_group(vec![
            table_group("rise_transition", "RT_TPL"),
            table_group("fall_transition", "FT_TPL"),
        ]);
        let tt = extract_timing_tables_from_arc(&g).expect("tables present");
        assert_eq!(tt.lut_template, "RT_TPL");
        assert!(tt.cell_rise.is_none());
        assert!(tt.cell_fall.is_none());
    }

    /// Killed by: `extract_timing_tables_from_arc` joined its `cell_rise.is_none() && ...` guard with `||`, so any missing family refused the arc.
    #[test]
    fn lut_template_falls_back_to_fall_transition_when_only_that_is_present() {
        let g = timing_group(vec![table_group("fall_transition", "FT_TPL")]);
        let tt = extract_timing_tables_from_arc(&g).expect("tables present");
        assert_eq!(tt.lut_template, "FT_TPL");
    }

    /// Killed by: `extract_timing_tables_from_arc`'s fall_transition filter widened to `g.type_ != "nothing_at_all"`, making the last family take whatever remained.
    #[test]
    fn extract_timing_tables_is_none_with_no_table_subgroups() {
        assert!(extract_timing_tables_from_arc(&timing_group(vec![])).is_none());

        // A constraint arc carries subgroups, but none of them is one of the four
        // delay or transition tables. Nothing left over may be taken for a table:
        // the last family is the *fall_transition* one, not "whatever remains".
        let constraint_arc = timing_group(vec![
            table_group("rise_constraint", "RC_TPL"),
            table_group("fall_constraint", "FC_TPL"),
        ]);
        assert!(extract_timing_tables_from_arc(&constraint_arc).is_none());
    }

    /// Killed by: `extract_timing_tables_from_arc`'s cell_rise arm matched `(Some(group), Some(_))`, so cell_rise never claimed the template.
    #[test]
    fn extract_timing_tables_with_only_cell_rise_leaves_the_others_none() {
        let g = timing_group(vec![table_group("cell_rise", "CR_TPL")]);
        let tt = extract_timing_tables_from_arc(&g).expect("tables present");
        assert_eq!(tt.lut_template, "CR_TPL");
        assert!(tt.cell_rise.is_some());
        assert!(tt.cell_fall.is_none());
        assert!(tt.rise_trans.is_none());
        assert!(tt.fall_trans.is_none());
    }

    // --- WhenMerge::from_str ------------------------------------------------

    /// Killed by: `WhenMerge::from_str` mapped `"mean"` to `WhenMerge::Min`.
    #[test]
    fn when_merge_from_str_maps_each_spelling() {
        assert_eq!("mean".parse::<WhenMerge>(), Ok(WhenMerge::Mean));
        assert_eq!("min".parse::<WhenMerge>(), Ok(WhenMerge::Min));
        assert_eq!("max".parse::<WhenMerge>(), Ok(WhenMerge::Max));

        let err = "bogus".parse::<WhenMerge>().unwrap_err();
        assert!(
            err.contains("unknown when-merge"),
            "error message was {:?}",
            err
        );
    }

    // --- ReferenceMode::from_str --------------------------------------------

    /// Killed by: `ReferenceMode::from_str` mapped `"pooled"` to `ReferenceMode::PerOutput`.
    #[test]
    fn reference_mode_from_str_maps_each_spelling() {
        assert_eq!("pooled".parse::<ReferenceMode>(), Ok(ReferenceMode::Pooled));
        assert_eq!(
            "per-output".parse::<ReferenceMode>(),
            Ok(ReferenceMode::PerOutput)
        );
        assert_eq!(
            "per-state".parse::<ReferenceMode>(),
            Ok(ReferenceMode::PerState)
        );

        let err = "bogus".parse::<ReferenceMode>().unwrap_err();
        assert!(
            err.contains("unknown reference mode"),
            "error message was {:?}",
            err
        );
    }

    // --- when-condition averaging -----------------------------------------

    fn tables(cell_rise: Option<f64>, cell_fall: Option<f64>, trans: Option<f64>) -> TimingTables {
        let fill = |v: f64| Array2::from_shape_vec((2, 2), vec![v; 4]).unwrap();
        TimingTables {
            slews: None,
            loads: None,
            lut_template: "T".to_owned(),
            sense: Some(TimingSense::Positive),
            cell_rise: cell_rise.map(fill),
            cell_fall: cell_fall.map(fill),
            rise_trans: trans.map(fill),
            fall_trans: trans.map(fill),
        }
    }

    /// Killed by: `TableAccumulator::result` returned `sum / 1.0` for `WhenMerge::Mean`, leaving the sum undivided.
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

    /// Killed by: `TableAccumulator::add`'s mismatch guard changed to `sum.raw_dim() != sum.raw_dim()`, so a differently shaped condition reached the addition.
    #[test]
    fn a_condition_on_a_different_table_shape_is_ignored_rather_than_panicking() {
        let mut acc = ArcAccumulator::new(WhenMerge::Mean);
        acc.accumulate(tables(Some(10.0), None, None), "D", "Q");

        let odd = TimingTables {
            slews: None,
            loads: None,
            lut_template: "T".to_owned(),
            sense: Some(TimingSense::Positive),
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
    ///
    /// Killed by: `TableAccumulator::add` folded `WhenMerge::Min` with `a.max(*b)`.
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
}
