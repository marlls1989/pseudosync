//! The conversion itself: what each cell's constraints and delays are drawn
//! against, and how a library is walked.

use crate::arcs::{
    arc_domains, extract_timing_tables_from_arc, input_transition, mean_reference_arc,
    select_reference_arc, slew_profile, Anchor, ArcAccumulator, EdgeRef, RefArc, ReferenceMode,
    References, Scope, TableAccumulator, TimingSense, TimingTables, Transition, WhenMerge,
};
use crate::conditions::{
    collision_classes, collision_classes_within, merge_conditions, ClassId, Condition,
};
use crate::emit::{
    convert_latch_to_flipflop, create_hold_timing_group, create_pseudo_output_timing_arc,
    create_setup_timing_group, generate_pseudo_lut_templates, Guard,
};
use crate::pins::{
    cell_qualifies, constraint_targets_mut, is_output_pin, timing_leaves, timing_leaves_mut,
};
use crate::report::{
    collect_arc_errors, ArcError, CellReport, CheckClass, ConditionedArc, ConstraintArcs,
    ConstraintKey, Constraints, LibraryReport, Refusal, StateClass, FALL, RISE,
};
use crate::templates::{Axes, Templates};
use itertools::Itertools;
use liberty_parser::{
    ast::Value,
    liberty::{Attribute, Group},
};
use ndarray::prelude::*;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// Reduce the input-to-output arcs of one source pin to a single constraint.
///
/// The mean arc from the source to every output it drives, sampled at the
/// reference column, minus the clock-to-output delay that the pseudo-flop model
/// will charge for that path. Which delay that is depends on `mode`; see
/// [`ReferenceMode`].
///
/// `arcs` is already restricted to converted outputs by the caller, which is what
/// makes the mixed-source rule hold: a source driving both a converted and a skipped
/// output is averaged over the converted ones alone.
/// The state each present delay family of one arc describes once its input has
/// settled, in collection order beside the raw arcs.
///
/// Engine-internal: the report keeps only the class each of these fell into.
enum ArcPost {
    /// A whenless arc, which is the catch-all for its output and edge rather than
    /// a state of its own.
    CatchAll,
    /// The `when` could not be read, so the arc describes no state this tool can
    /// name. It still converts -- nothing in the split consults a condition.
    Unreadable,
    /// The post-settled condition per output edge, present for the delay families
    /// the arc actually carries, beside the `when` they were built from.
    Settled {
        /// The source condition, which remembers the library's own spelling. The
        /// report names a class's members by that spelling rather than by a second
        /// copy of the attribute, so a member is always listed with the very text
        /// that was parsed into the class it is listed under.
        source: Condition,
        rise: Option<Condition>,
        fall: Option<Condition>,
    },
}

/// Group one cell's arcs by the post-settled state they describe, writing each
/// class back onto the arc's own edge field and returning the report's view of them
/// beside the merged condition each class is emitted under.
///
/// A row is headed by its class's MERGED condition -- the very condition the class's
/// arc carries in the emitted library -- and never by the condition of the member
/// that opened the row. A class's members can hold at once, which is what put them in
/// one class, so a member's own condition can be strictly narrower than the state its
/// class is emitted under; heading the row with it would state the class under a
/// condition its arc was never confined to. That is why the labels are drawn here,
/// before the rows: [`merged_class_conditions`] reads the classes this function has
/// just written back, so the rows cannot be headed until it has run.
///
/// Classes are drawn within one OUTPUT and never across two. After the split every
/// propagation arc of an output is `G -> Q`, so on one output nothing but the `when`
/// tells two arcs apart -- two sources' conditions that can hold at once are one
/// state there, `related_pin` no longer separating them, and that collision is
/// exactly what a per-state model must resolve. `G -> Q1` and `G -> Q2` are different
/// pin pairs, and Liberty UG p.7-49–50's mutual-exclusivity requirement is about one
/// pin's state-dependent timing arcs: two outputs' conditions were never required to
/// exclude one another, so an overlap between them is not a collision at all.
///
/// A whenless arc is the catch-all of its output and edge -- it covers whatever the
/// conditioned arcs do not -- and takes a row of its own rather than a class. An arc
/// whose `when` could not be read takes neither: it was warned about where it was
/// read, and nothing can be said about the state it describes.
fn classify_states(
    raw_arcs: &mut [ConditionedArc],
    post: &[ArcPost],
) -> (Vec<StateClass>, BTreeMap<ClassId, Condition>) {
    // Flattened per output, in the collection order of each, with the way back to the
    // arc and edge every condition came from -- because the classes are numbered by
    // first appearance within the group they are drawn in. The outputs themselves are
    // held in first-appearance order for the same reason.
    let mut outputs: Vec<String> = Vec::new();
    let mut conditions: Vec<Vec<Condition>> = Vec::new();
    let mut origins: Vec<Vec<(usize, Transition)>> = Vec::new();
    for (index, entry) in post.iter().enumerate() {
        let ArcPost::Settled { rise, fall, .. } = entry else {
            continue;
        };
        let output = &raw_arcs[index].output;
        let group = match outputs.iter().position(|held| held == output) {
            Some(group) => group,
            None => {
                outputs.push(output.clone());
                conditions.push(Vec::new());
                origins.push(Vec::new());
                outputs.len() - 1
            }
        };
        for (edge, condition) in [(Transition::Rise, rise), (Transition::Fall, fall)] {
            if let Some(condition) = condition {
                conditions[group].push(condition.clone());
                origins[group].push((index, edge));
            }
        }
    }

    for (group, ids) in origins.iter().zip(collision_classes_within(&conditions)) {
        for ((index, edge), id) in group.iter().zip(ids) {
            match edge {
                Transition::Rise => raw_arcs[*index].class_rise = Some(id),
                Transition::Fall => raw_arcs[*index].class_fall = Some(id),
            }
        }
    }

    // The condition each class is stated under, drawn from the write-back above so
    // that a row can be headed by it rather than by whichever member opened the row.
    let class_conditions = merged_class_conditions(raw_arcs, post);

    // One row per (output, edge, class), plus one catch-all row per (output, edge).
    // Keyed on `Option<ClassId>` so the catch-all sorts before every class, and
    // built through an index so the rows keep first-appearance order.
    let mut rows: Vec<StateClass> = Vec::new();
    let mut index_of: BTreeMap<(String, Transition, Option<ClassId>), usize> = BTreeMap::new();
    let mut place = |output: &str,
                     edge: Transition,
                     class: Option<ClassId>,
                     member: (String, Option<String>)| {
        let slot = index_of
            .entry((output.to_owned(), edge, class))
            .or_insert_with(|| {
                rows.push(StateClass {
                    output: output.to_owned(),
                    edge,
                    condition: class.map(|class| {
                        class_conditions
                            .get(&class)
                            .unwrap_or_else(|| {
                                panic!(
                                    "internal: state {:?} of output {} was numbered without a \
                                     merged condition",
                                    class, output
                                )
                            })
                            .liberty()
                    }),
                    members: Vec::new(),
                });
                rows.len() - 1
            });
        rows[*slot].members.push(member);
    };

    for (arc, entry) in raw_arcs.iter().zip(post.iter()) {
        match entry {
            ArcPost::Unreadable => {}
            ArcPost::CatchAll => {
                let member = (arc.source.clone(), None);
                for (edge, present) in [
                    (Transition::Rise, arc.cell_rise.is_some()),
                    (Transition::Fall, arc.cell_fall.is_some()),
                ] {
                    if present {
                        place(&arc.output, edge, None, member.clone());
                    }
                }
            }
            ArcPost::Settled { source, .. } => {
                let member = (arc.source.clone(), Some(source.as_written()));
                // The class written back above is what says an edge was classified:
                // it was set for exactly the post-settled conditions this arc
                // carried, so an edge the arc never described holds none and takes
                // no row.
                for (edge, class) in [
                    (Transition::Rise, arc.class_rise),
                    (Transition::Fall, arc.class_fall),
                ] {
                    if class.is_some() {
                        place(&arc.output, edge, class, member.clone());
                    }
                }
            }
        }
    }

    (rows, class_conditions)
}

/// The merged condition each post-settled class is emitted under: the least
/// restrictive condition covering the whole class.
///
/// A class's members can hold at once — that is what put them in one class — so no
/// one member's condition describes the state the class's merged tables were
/// computed over. [`merge_conditions`] takes the union of the members' own
/// conditions, which is what the merged arc actually holds under.
///
/// The members are collected in appearance order, so a class whose union equals one
/// of them is stated in the first such member's spelling — which is every class that
/// was an equality class before conditions began colliding on overlap.
///
/// Keyed on the class alone, which is enough because a class is drawn within one
/// output: its members are all conditions that output was characterised under, so the
/// union cannot state a condition that output holds no tables over.
fn merged_class_conditions(
    raw_arcs: &[ConditionedArc],
    post: &[ArcPost],
) -> BTreeMap<ClassId, Condition> {
    let mut members: BTreeMap<ClassId, Vec<&Condition>> = BTreeMap::new();
    for (arc, entry) in raw_arcs.iter().zip(post.iter()) {
        if let ArcPost::Settled { rise, fall, .. } = entry {
            for (condition, class) in [(rise, arc.class_rise), (fall, arc.class_fall)] {
                if let (Some(condition), Some(class)) = (condition, class) {
                    members.entry(class).or_default().push(condition);
                }
            }
        }
    }
    members
        .into_iter()
        .map(|(class, members)| (class, merge_conditions(&members)))
        .collect()
}

/// One emitted pair of checks on one input pin: the condition they are stated under,
/// and the scope their values are filed at.
///
/// One scope, because a group's values are keyed by the very class that made it a
/// group: both input directions of one condition are looked up there, and the pin
/// carries at most one value per direction under it. What separates the two
/// directions is the map they are read from, not the key they are read at.
struct CheckGroup {
    /// `None` for the catch-all, which states no condition.
    condition: Option<Condition>,
    scope: Scope,
}

/// Group each input pin's arcs by the condition the library characterised them
/// under, writing each class back onto the arc it came from and returning the groups
/// the pin's checks are emitted as.
///
/// A second classification, independent of the post-settled one: this is per pin and
/// numbers its classes per pin, so nothing here can be confused for a state of the
/// cell. Grouping on the BDD rather than on the text is what makes two spellings of
/// one condition impossible to emit as two overlapping check groups.
///
/// The class written back is what the constraints are then summed by, so a group has
/// exactly one value per direction by construction: every arc the pin drives under
/// this condition lands in this group's average, whichever output it drives and
/// whatever state that output settles in.
///
/// A group states its whole class's merged condition and not the condition of the
/// arc that opened it. Liberty UG p.7-49–50's mutual-exclusivity requirement is
/// about a pin's state-dependent timing arcs, which a check group is as much as a
/// delay group is: two of a pin's check `when`s that can both hold are as
/// inadmissible as two of an output's. Where the class's union equals one of its
/// members — every class that was an equality class before conditions began
/// colliding on overlap — the merge returns that member, so the emitted text is
/// still the library's own.
///
/// The conditioned groups come out in first-appearance order and the catch-all last,
/// which is the order Liberty reads a `default_timing` group in.
fn check_groups(
    raw_arcs: &mut [ConditionedArc],
    post: &[ArcPost],
) -> (BTreeMap<String, Vec<CheckGroup>>, Vec<CheckClass>) {
    // Pins in the order the library declares their arcs, so the report's rows follow
    // the library rather than an alphabetical order the library never chose. Owned,
    // so the classes below can be written back onto the arcs they were drawn from.
    let mut pins: Vec<String> = Vec::new();
    for arc in raw_arcs.iter() {
        if !pins.contains(&arc.source) {
            pins.push(arc.source.clone());
        }
    }

    let mut groups: BTreeMap<String, Vec<CheckGroup>> = BTreeMap::new();
    let mut rows: Vec<CheckClass> = Vec::new();

    for pin in &pins {
        let members: Vec<usize> = (0..raw_arcs.len())
            .filter(|i| &raw_arcs[*i].source == pin)
            .collect();
        let conditions: Vec<Condition> = members
            .iter()
            .filter_map(|i| match &post[*i] {
                ArcPost::Settled { source, .. } => Some(source.clone()),
                _ => None,
            })
            .collect();
        let ids = collision_classes(&conditions);

        // The condition each class is stated under, taken over its own members in
        // appearance order -- the same derivation an output's states take, on the
        // raw source conditions rather than the post-settled ones.
        let mut class_members: BTreeMap<ClassId, Vec<&Condition>> = BTreeMap::new();
        for (condition, id) in conditions.iter().zip(ids.iter()) {
            class_members.entry(*id).or_default().push(condition);
        }
        let labels: BTreeMap<ClassId, Condition> = class_members
            .into_iter()
            .map(|(id, members)| (id, merge_conditions(&members)))
            .collect();

        // Indexed by class where the arc states a condition, and by `None` for the
        // catch-all, so the catch-all can be moved to the end whatever order it
        // first appeared in.
        let mut slot_of: BTreeMap<Option<ClassId>, usize> = BTreeMap::new();
        let mut order: Vec<Option<ClassId>> = Vec::new();
        let mut built: Vec<(CheckGroup, CheckClass)> = Vec::new();
        let mut settled = 0usize;

        for &index in &members {
            let (class, condition) = match &post[index] {
                ArcPost::Settled { .. } => {
                    let class = ids[settled];
                    settled += 1;
                    let label = labels.get(&class).unwrap_or_else(|| {
                        panic!(
                            "internal: check class {:?} of pin {} was numbered without a \
                             merged condition",
                            class, pin
                        )
                    });
                    (Some(class), Some(label.clone()))
                }
                ArcPost::CatchAll => (None, None),
                // Unreachable: this function is called from the per-state arm alone,
                // and per-state drops an arc whose `when` it could not read before that
                // arc ever reaches `post`. The arm is here to keep the match total, and
                // drops the arc, which is what the skip upstream already did -- it is
                // not the catch-all, whose members cover a state this one does not name.
                ArcPost::Unreadable => continue,
            };

            // Back onto the arc, because it is the key the arc's values are summed
            // by: the constraints of a pin are grouped by the condition its own
            // checks are stated under, so an arc contributes to the group its `when`
            // put it in whichever output it drives.
            raw_arcs[index].check_class = class;

            let slot = *slot_of.entry(class).or_insert_with(|| {
                order.push(class);
                // The scope this group's values are filed at. Total for both arms --
                // a class names a state and a catch-all names the catch-all -- and
                // taken from the same construction the arcs' own scopes are, so the
                // group and its values cannot be keyed apart.
                let scope = Scope::of(ReferenceMode::PerState, class, class.is_none())
                    .unwrap_or_else(|| {
                        panic!(
                            "internal: check class {:?} of pin {} names no scope",
                            class, pin
                        )
                    });
                built.push((
                    CheckGroup {
                        condition: condition.clone(),
                        scope,
                    },
                    CheckClass {
                        pin: pin.to_owned(),
                        condition: condition.as_ref().map(|c| c.as_written()),
                        members: Vec::new(),
                    },
                ));
                built.len() - 1
            });
            built[slot].1.members.push(raw_arcs[index].output.clone());
        }

        // The catch-all last, because it covers whatever the conditioned groups do
        // not and is read after them.
        let catch_all_last = |class: &Option<ClassId>| class.is_none();
        let mut ordered: Vec<usize> = (0..built.len())
            .filter(|i| !catch_all_last(&order[*i]))
            .collect();
        ordered.extend((0..built.len()).filter(|i| catch_all_last(&order[*i])));

        let (pin_groups, pin_rows): (Vec<CheckGroup>, Vec<CheckClass>) = ordered
            .into_iter()
            .map(|i| {
                let (group, row) = &built[i];
                (
                    CheckGroup {
                        condition: group.condition.clone(),
                        scope: group.scope,
                    },
                    row.clone(),
                )
            })
            .unzip();
        groups.insert(pin.to_owned(), pin_groups);
        rows.extend(pin_rows);
    }

    (groups, rows)
}

/// One edge's half of an arc's tables: the delay family naming that edge and the
/// transition that pairs with it, carrying the group's own template and axes so a
/// half is accumulated exactly as the whole group was.
///
/// The two halves are filed separately because under a per-state reference they
/// describe two different states: an arc conditioned on `B` leaves the cell in
/// `B * A` when its input settled high and `B * !A` when it settled low.
fn edge_half(tables: &TimingTables, edge: Transition) -> TimingTables {
    let mut half = tables.clone();
    match edge {
        Transition::Rise => {
            half.cell_fall = None;
            half.fall_trans = None;
        }
        Transition::Fall => {
            half.cell_rise = None;
            half.rise_trans = None;
        }
    }
    half
}

/// The direction the input was moving in, for an arc that survived the sense skip.
///
/// Failing loudly rather than defaulting, because the skip above is what makes this
/// total: if it is ever relaxed, the result is an unambiguous defect report about
/// pseudosync and not a constraint quietly filed under the wrong edge.
fn derived_input_edge(
    sense: TimingSense,
    output_edge: Transition,
    related_pin: &str,
    outpin_name: &str,
) -> Transition {
    input_transition(sense, output_edge).unwrap_or_else(|| {
        panic!(
            "internal: arc {} -> {} survived the sense skip with no derivable input direction",
            related_pin, outpin_name
        )
    })
}

/// The reference half a table of `family` is charged against, where the reference
/// carries that edge at all.
///
/// Keyed on the table family and not on the input's direction, because the
/// clock-to-output delay of an output rise is what a `cell_rise` table has to be
/// referred to whatever made the input move that way. The sense decides which
/// constraint the result is written to, never which delay it is measured from.
fn family_reference<'a>(r: &'a RefArc, family: &str) -> Option<&'a EdgeRef> {
    match family {
        "cell_rise" => r.rise.as_ref(),
        _ => r.fall.as_ref(),
    }
}

/// The same, for an entry the caller has already established has a reference.
///
/// Every constraint entry whose scope supplies no reference for its own family is
/// dropped before the arithmetic runs, which is what makes this total. It fails
/// loudly rather than defaulting, so relaxing that would be reported as a defect in
/// pseudosync rather than as a plausible number in the emitted library.
fn charged_reference<'a>(r: &'a RefArc, family: &str, what: &str) -> &'a EdgeRef {
    family_reference(r, family).unwrap_or_else(|| {
        panic!(
            "internal: {} reached the constraint arithmetic with no {} reference",
            what, family
        )
    })
}

/// Reduce the input-to-output arcs of one source pin to a single constraint.
///
/// The mean arc from the source to every output it drives, sampled at the
/// reference column, minus the clock-to-output delay that the pseudo-flop model
/// will charge for that path. Which delay that is depends on `mode`; see
/// [`ReferenceMode`].
///
/// `arcs` is already restricted to converted outputs by the caller, which is what
/// makes the mixed-source rule hold: a source driving both a converted and a skipped
/// output is averaged over the converted ones alone. It is grouped by the direction
/// the INPUT was moving in, so `input_edge` selects the entries this constraint is
/// built from, and by the condition the SOURCE's own checks are grouped under, so
/// every check group carries exactly one value.
///
/// That is one averaging rule and not two. Per-output averages a pin over every
/// output it drives, because it emits one unconditioned check per pin; per-state
/// specialises the same average to the outputs the pin drives UNDER THIS CONDITION,
/// because that is what its check group states. Per-output is the degenerate case
/// where the condition is "always", and both are keyed by the check the values are
/// emitted in. Each entry is still charged the reference of ITS OWN output and state,
/// which is the delay the model emits for that path, so the average is of the several
/// crossings the one check has to stand in for.
fn constraints_from_arcs(
    arcs: &ConstraintArcs,
    input_edge: Transition,
    ref_arcs: &BTreeMap<(String, Scope), RefArc>,
    mean_ref: &RefArc,
    mode: ReferenceMode,
    anchor: Anchor,
) -> BTreeMap<(String, Scope), Array1<f64>> {
    /// One group's running totals, summed in the order the entries are keyed so the
    /// arithmetic does not depend on how the group was assembled.
    #[derive(Default)]
    struct Running {
        n: f64,
        arc_sum: Option<Array2<f64>>,
        ref_sum: f64,
        families: BTreeSet<&'static str>,
    }

    let mut groups: BTreeMap<(String, Scope), Running> = BTreeMap::new();

    for (key, table) in arcs {
        if key.input_edge != input_edge {
            continue;
        }
        let (outpin, family) = (&key.outpin, key.family);
        let group = groups.entry((key.src.clone(), key.check)).or_default();
        group.n += 1.0;
        group.families.insert(family);
        group.arc_sum = Some(match group.arc_sum.take() {
            Some(sum) => sum + table,
            None => table.clone(),
        });
        group.ref_sum += match mode {
            // Only the outputs this source actually drives contribute, so a
            // rail-private input is referenced against its own rail alone -- and
            // under per-state only the state this entry describes, so each term of
            // the average is a delay the model really does emit for that path.
            //
            // Substituting the cell-wide mean here would charge this input a delay
            // measured on a *different* output, for a path nothing characterised.
            // The caller's restriction to converted outputs makes that unreachable,
            // so this fails loudly instead: if the restriction is ever relaxed, the
            // failure is unambiguous rather than a plausible number in the emitted
            // library.
            ReferenceMode::PerOutput | ReferenceMode::PerState => ref_arcs
                .get(&(outpin.clone(), key.delay))
                .map(|r| charged_reference(r, family, outpin).crossing)
                .unwrap_or_else(|| {
                    panic!(
                        "arc to output {} reached the constraint arithmetic with no reference of its own",
                        outpin
                    )
                }),
            ReferenceMode::Pooled => {
                charged_reference(mean_ref, family, "the cell-wide mean").crossing
            }
        };
    }

    groups
        .into_iter()
        .filter_map(|(key, group)| {
            let reference = match (mode, group.families.iter().next()) {
                // Every entry of a single-family group charges the same constant, and
                // (c + c + ... + c) / n is not bit-identical to c, so the original
                // single-subtraction expression is kept rather than reconstructed. A
                // group of mixed families -- which takes two senses on one pin pair to
                // produce -- has no single constant, and takes the summed path below.
                (ReferenceMode::Pooled, Some(family)) if group.families.len() == 1 => {
                    charged_reference(mean_ref, family, "the cell-wide mean").crossing
                }
                _ => group.ref_sum / group.n,
            };

            group
                .arc_sum
                .map(|sum| (key, slew_profile(&(sum / group.n), anchor) - reference))
        })
        .collect()
}

/// Calculate setup constraints for all input pins, one map per input direction.
fn calculate_setup_constraints(
    constraint_arcs: &ConstraintArcs,
    ref_arcs: &BTreeMap<(String, Scope), RefArc>,
    mean_ref: &RefArc,
    mode: ReferenceMode,
    anchor: Anchor,
) -> (Constraints, Constraints) {
    let setup_input_rise = constraints_from_arcs(
        constraint_arcs,
        Transition::Rise,
        ref_arcs,
        mean_ref,
        mode,
        anchor,
    );
    let setup_input_fall = constraints_from_arcs(
        constraint_arcs,
        Transition::Fall,
        ref_arcs,
        mean_ref,
        mode,
        anchor,
    );

    (setup_input_rise, setup_input_fall)
}

/// Calculate hold constraints from setup constraints (negated)
fn calculate_hold_constraints(
    setup_input_rise: &Constraints,
    setup_input_fall: &Constraints,
) -> (Constraints, Constraints) {
    let hold_input_rise = setup_input_rise
        .iter()
        .map(|(k, v)| (k.clone(), v.clone() * -1.0))
        .collect();

    let hold_input_fall = setup_input_fall
        .iter()
        .map(|(k, v)| (k.clone(), v.clone() * -1.0))
        .collect();

    (hold_input_rise, hold_input_fall)
}

/// One clock-to-output arc the model emits: the output's own transitions, the delays
/// it is charged, and the state it is stated under.
struct PseudoArc<'a> {
    transitions: &'a RefArc,
    delays: &'a RefArc,
    guard: Guard<'a>,
}

/// Add pseudo-synchronous timing to an output pin
///
/// One arc per state the output was converted in. Under a mode that draws one
/// reference per output there is exactly one, unguarded, which is the arc this
/// emitted before there were states.
fn add_pseudo_timing_to_output_pin(
    outpin: &mut Group,
    clock_name: &str,
    reset_name: &Regex,
    arcs: &[PseudoArc],
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

    // Add the new pseudo-synchronous timing arcs:
    // - Use this output's own transitions (decoupled from input)
    // - Use mean cell_rise/cell_fall delays (averaged across outputs)
    for arc in arcs {
        outpin.subgroups.push(create_pseudo_output_timing_arc(
            clock_name,
            arc.transitions,
            arc.delays,
            &arc.guard,
        ));
    }
}

/// One emitted setup/hold pair on an input pin: the values each direction carries,
/// and the condition the pair is stated under.
struct CheckEntry<'a> {
    guard: Guard<'a>,
    setup_rise: Option<&'a Array1<f64>>,
    setup_fall: Option<&'a Array1<f64>>,
    hold_rise: Option<&'a Array1<f64>>,
    hold_fall: Option<&'a Array1<f64>>,
}

/// Add setup and hold constraints to an input pin
///
/// One setup group and one hold group per entry, setups first. Both directions of an
/// entry sit in the ONE group, because UG p.7-56 asks a constraint group for at least
/// one lookup table and not for a particular one: a condition characterised in both
/// input directions is one check carrying two constraint families, not two checks.
fn add_constraints_to_input_pin(
    inpin: &mut Group,
    clock_name: &str,
    ref_arc: &RefArc,
    entries: &[CheckEntry],
) {
    // Mark pin as data input
    inpin.attributes.insert(
        "nextstate_type".to_owned(),
        vec![Attribute::Simple(Value::Expression("data".to_owned()))],
    );

    // Setups first, then holds, so a pin's groups read as the two families they are
    // rather than interleaved pair by pair.
    for entry in entries {
        inpin.subgroups.push(create_setup_timing_group(
            clock_name,
            ref_arc,
            entry.setup_rise,
            entry.setup_fall,
            &entry.guard,
        ));
    }

    for entry in entries {
        inpin.subgroups.push(create_hold_timing_group(
            clock_name,
            ref_arc,
            entry.hold_rise,
            entry.hold_fall,
            &entry.guard,
        ));
    }
}

/// The knobs that decide how one cell is converted. They are chosen once per
/// run and travel together, so they are passed as a unit -- which is also what
/// keeps the entry point below to a signature a reader can hold in their head.
pub(crate) struct CellOptions<'a> {
    pub(crate) clock_name: &'a str,
    pub(crate) reset_name: &'a Regex,
    pub(crate) latch: bool,
    pub(crate) mode: ReferenceMode,
    pub(crate) when_merge: WhenMerge,
    pub(crate) anchor: Anchor,
}

/// Process a single cell to add pseudo-synchronous timing.
///
/// On success the `lut_template` name the cell's pseudo delays were built on, so
/// the library can generate the template. On failure the reason the cell could
/// not be converted, for the caller to report against the cell it belongs to.
fn process_cell(
    cell: &mut Group,
    opts: &CellOptions,
    templates: &Templates,
    lib_name: &str,
    reports: &mut LibraryReport,
) -> Result<String, String> {
    let CellOptions {
        clock_name,
        reset_name,
        latch,
        mode,
        when_merge,
        anchor,
    } = *opts;
    let cell_name = cell.name.clone();
    eprintln!("Processing cell {}", cell_name);

    let mut ref_arcs: BTreeMap<(String, Scope), RefArc> = BTreeMap::new();
    // The delay tables the constraints are derived from; see [`ConstraintKey`] for
    // what each part of the key decides.
    //
    // The input's edge is part of the key because it is what a constraint is keyed on,
    // and it is not the output's: a negative-unate arc's `cell_rise` values describe an
    // input that fell. Merging by output family instead would blend two opposite input
    // directions inside one accumulator wherever a pin pair carries arcs of both senses
    // -- the shape an XOR is characterised in. Routing per arc, before the `when`
    // merge, is what keeps them apart. The check scope is in the key for the same
    // reason: two arcs of one pin pair whose `when`s put them in different check
    // groups belong to two different emitted checks, and merging them here would
    // charge each group the other's arc.
    let mut constraint_arcs: BTreeMap<ConstraintKey, TableAccumulator> = BTreeMap::new();

    // Phase 1: Collect every arc, folding the arcs that share a scope into one
    // representative arc
    let mut accumulated: BTreeMap<(String, String, Scope), ArcAccumulator> = BTreeMap::new();
    // First-appearance order of each (output, scope)'s source pins, so the reference
    // arc is still chosen by the order the library declares them.
    let mut source_order: BTreeMap<(String, Scope), Vec<String>> = BTreeMap::new();
    // Kept unreduced so the report can measure the model against every
    // condition, not just against the average it was built from.
    let mut raw_arcs: Vec<ConditionedArc> = Vec::new();
    // Indexed to `raw_arcs`: the state each arc leaves the cell in once its input
    // has settled. Classified after the walk, within each output, so the ids run in
    // the order the library declares the arcs.
    let mut post_conditions: Vec<ArcPost> = Vec::new();
    // Also indexed to `raw_arcs`: the tables each arc carries, held until the
    // classification has run. A per-state scope is a class, a class is drawn over
    // every arc of its output at once, and so nothing can be filed under one until
    // every arc has been read -- which is why the accumulation is a second pass rather
    // than part of this walk. It runs in the same order, so what each accumulator
    // sums, and in what order, is unchanged.
    let mut arc_tables: Vec<TimingTables> = Vec::new();
    // Outputs an arc of which was skipped for a `when` this tool could not read, one
    // entry per skipped arc. Recorded rather than reported here, because a cell that
    // turns out not to convert at all is emitted verbatim and has no output-scope
    // refusal to make.
    let mut unreadable_arcs: Vec<String> = Vec::new();

    // Every output leaf, whether or not it turns out to carry a usable arc. The skip
    // decision below is taken over this list rather than over the outputs that
    // reached the model: an output whose only non-reset arc carries no
    // characterisation table never enters `source_order`, and keying the decision on
    // that map is what let such an output pass unnoticed.
    let mut output_leaves: Vec<String> = Vec::new();
    // The domain of every table the conversion will transform: the lookup template named,
    // and the dimensions its table carries. Judged once, after this loop.
    let mut domains: BTreeSet<(String, (usize, usize))> = BTreeSet::new();
    // The slew and load points the transformed tables are indexed at, so the report can
    // caption a residual with the regime it occurs in rather than a row number. A table
    // may declare its own axes, which override the template's. One pair serves the whole
    // cell because a cell whose arcs sit on more than one domain is refused below; if the
    // arcs disagree anyway the labels are dropped rather than taken from one of them.
    let mut cell_axes: Option<Axes> = None;
    let mut axes_agree = true;
    // Outputs an arc of which was skipped at arc scope, and the output-scope reason that
    // skip gives, so the warning can state the one that is true rather than the one that
    // merely fits. The first skip's reason is kept: an output whose arcs fell short in
    // more than one way is described by the first way that was found, rather than by
    // whichever happened to be looked at last.
    let mut arc_skipped: BTreeMap<String, &'static str> = BTreeMap::new();

    for outpin in timing_leaves(cell, is_output_pin) {
        let outpin_name = &outpin.name;
        output_leaves.push(outpin_name.clone());

        // Process each timing group in the output pin
        for timing_group in outpin.iter_subgroups_of_type("timing") {
            // Read rather than unwrapped. A timing group with no `related_pin` describes no
            // path between two pins, so there would be nothing here to split. Constraints of
            // that kind are input-pin constraints -- a clock's or a reset's -- and this walks
            // output pins only, so it does not arise; reading it is merely cheaper than a
            // panic that would take the whole run down if it ever did.
            let Some(related_pin) = timing_group
                .simple_attribute("related_pin")
                .map(|v| v.string())
            else {
                continue;
            };

            // Skip reset pins
            if reset_name.is_match(&related_pin) {
                continue;
            }

            // Extract timing tables from this arc
            if let Some(timing_tables) = extract_timing_tables_from_arc(timing_group) {
                // Arc scope. A template declaring only one axis cannot carry the
                // split, so this arc is skipped and enters nothing: not the raw
                // arcs, not the accumulator, not the source order. A one-axis
                // template is ordinary Liberty -- it is the shape this tool's own
                // derived templates take -- so the warning says what is missing
                // rather than calling the input malformed.
                if let Some(missing) = templates.missing_axis(&timing_tables.lut_template) {
                    eprintln!(
                        "Skipping arc {} -> {} of cell {} in library {}: lookup template {} {}",
                        related_pin,
                        outpin_name,
                        cell_name,
                        lib_name,
                        timing_tables.lut_template,
                        missing
                    );
                    arc_skipped.entry(outpin_name.clone()).or_insert(
                        "every non-reset arc was skipped: no lookup template with both axes",
                    );
                    continue;
                }

                // Arc scope, and mode-independent. A constraint is keyed on the
                // direction the INPUT was moving in, and `timing_sense` is the only
                // thing that determines it, so an arc that does not state one cannot
                // be placed in either constraint family under any mode.
                //
                // An absent or unrecognised `timing_sense` is treated exactly as
                // `non_unate`: it says as little, and no Liberty default for the
                // attribute is stated in RM pp.319-372 or UG pp.7-1..7-65, so there
                // is nothing to fall back on. Like the axis skip, this arc enters
                // nothing at all.
                let sense = match timing_tables.sense {
                    Some(sense @ (TimingSense::Positive | TimingSense::Negative)) => sense,
                    unusable => {
                        let reason = match unusable {
                            Some(_) => {
                                "timing_sense non_unate: the input's direction cannot be determined"
                            }
                            None => "no timing_sense: the input's direction cannot be determined",
                        };
                        eprintln!(
                            "Skipping arc {} -> {} of cell {} in library {}: {}",
                            related_pin, outpin_name, cell_name, lib_name, reason
                        );
                        arc_skipped.entry(outpin_name.clone()).or_insert(
                            "every non-reset arc was skipped: no derivable input direction",
                        );
                        continue;
                    }
                };

                // This arc will be transformed, so its tables' domains are among those
                // that must agree. Recorded per family rather than per group, because the
                // four families of one group can name different templates.
                domains.extend(arc_domains(timing_group));

                let declared = templates.axes(&timing_tables.lut_template);
                let arc_axes = Axes {
                    slew: timing_tables
                        .slews
                        .clone()
                        .or_else(|| declared.and_then(|a| a.slew.clone())),
                    load: timing_tables
                        .loads
                        .clone()
                        .or_else(|| declared.and_then(|a| a.load.clone())),
                };
                match &cell_axes {
                    None => cell_axes = Some(arc_axes),
                    Some(seen) if *seen != arc_axes => axes_agree = false,
                    Some(_) => {}
                }

                let when = timing_group.simple_attribute("when").map(|v| v.string());

                // The state this arc leaves the cell in once its input has settled:
                // the arc's own `when`, conjoined with the literal saying which way
                // the source pin went. The literal follows the INPUT's transition --
                // settled high gives `P`, settled low `!P` -- so a negative-unate
                // `cell_rise` arc is conditioned on `!P`, because what made this
                // output rise was its input falling.
                //
                // A whenless arc is the catch-all for its output and edge: it covers
                // whatever state the conditioned arcs do not, so putting it through
                // the same construction would spuriously claim it holds only while
                // its own source pin is high.
                let post = match &when {
                    None => ArcPost::CatchAll,
                    Some(text) => match Condition::parse(text) {
                        Err(reason) => {
                            eprintln!(
                                "Cannot classify arc {} -> {} of cell {} in library {}: {}",
                                related_pin, outpin_name, cell_name, lib_name, reason
                            );
                            ArcPost::Unreadable
                        }
                        Ok(source_when) => {
                            let settled = |output_edge: Transition, present: bool| {
                                if !present {
                                    return None;
                                }
                                let input_edge = derived_input_edge(
                                    sense,
                                    output_edge,
                                    &related_pin,
                                    outpin_name,
                                );
                                Some(source_when.and(&Condition::literal(
                                    &related_pin,
                                    input_edge == Transition::Rise,
                                )))
                            };
                            let rise = settled(Transition::Rise, timing_tables.cell_rise.is_some());
                            let fall = settled(Transition::Fall, timing_tables.cell_fall.is_some());
                            ArcPost::Settled {
                                source: source_when,
                                rise,
                                fall,
                            }
                        }
                    },
                };
                // Arc scope, and per-state only. A condition this tool cannot read
                // names no state, and under a per-state reference there is no scope to
                // file the arc under -- so it is skipped and enters nothing. The other
                // modes draw one reference for the whole output, which no condition
                // bears on, so there the arc converts exactly as any other does.
                if mode == ReferenceMode::PerState && matches!(post, ArcPost::Unreadable) {
                    unreadable_arcs.push(outpin_name.clone());
                    arc_skipped
                        .entry(outpin_name.clone())
                        .or_insert("every non-reset arc was skipped: its `when` could not be read");
                    continue;
                }

                post_conditions.push(post);

                raw_arcs.push(ConditionedArc {
                    source: related_pin.clone(),
                    output: outpin_name.clone(),
                    when,
                    sense,
                    class_rise: None,
                    class_fall: None,
                    check_class: None,
                    cell_rise: timing_tables.cell_rise.clone(),
                    cell_fall: timing_tables.cell_fall.clone(),
                });
                arc_tables.push(timing_tables);
            }
        }
    }

    // Cell scope, judged at the earliest point it can be: every arc the conversion
    // transforms must be on the same domain. The pseudo template pair is derived per cell
    // from one lookup template, and values indexed on different dimensions are not
    // comparable — so a cell carrying arcs on more than one domain is one this conversion
    // cannot describe, and it is emitted verbatim.
    //
    // Differing dimensions count as a differing template even under one name, which is
    // what makes this one rule rather than two. It also catches the case where the four
    // families of a single timing group disagree: reading the axes of only the first
    // present family would otherwise publish one family's table under a template derived
    // from another's — silently wrong numbers on legal input.
    //
    // This is long before phase 3 mutates anything, so such a cell comes through exactly
    // as it arrived.
    if domains.len() > 1 {
        return Err(format!(
            "arcs on more than one domain: {}",
            domains
                .iter()
                .map(|(template, (rows, cols))| format!("{} at {}x{}", template, rows, cols))
                .join(", ")
        ));
    }

    // One classification per output, over its own post-settled conditions in
    // collection order, so two arcs describing the same state of one output share an
    // id however they were spelled -- and the ids run in the order the library
    // declares the arcs. The merged condition of each class comes back with the
    // classification because the report's rows are headed by it too, so what an arc
    // is emitted under and what it is reported under are one label read twice.
    let (classes, class_conditions) = classify_states(&mut raw_arcs, &post_conditions);

    // The conditions the checks are grouped under: a second classification, per pin
    // and over the source `when`s alone, numbered independently of the post-settled
    // classes above. Only per-state emits more than one check per pin, so the other
    // modes group under nothing and report nothing -- and leave every arc's check
    // class unset, which is the whole-output scope those modes file everything at.
    //
    // Drawn before the arcs are filed, because the class it puts on each arc is what
    // the constraints below are grouped by.
    let (check_groups, check_classes) = match mode {
        ReferenceMode::PerState => check_groups(&mut raw_arcs, &post_conditions),
        _ => (BTreeMap::new(), Vec::new()),
    };

    // Phase 1b: file each arc's two edge halves under the scope this mode draws its
    // references at. The walk order is preserved, so each accumulator sums the same
    // tables in the same order it always has; under a whole-output scope both halves
    // of an arc land in the one accumulator, which is the arc it always was.
    for (index, tables) in arc_tables.iter().enumerate() {
        let arc = &raw_arcs[index];
        let whenless = arc.when.is_none();
        // The check group this arc's values are summed into, which is its own pin's
        // condition and not its output's state. Drawn through the same construction
        // as the scope below, so a group and the values it reads cannot be keyed
        // apart.
        let Some(check) = Scope::of(mode, arc.check_class, whenless) else {
            continue;
        };

        for (output_edge, class, family, delays) in [
            (
                Transition::Rise,
                arc.class_rise,
                "cell_rise",
                &tables.cell_rise,
            ),
            (
                Transition::Fall,
                arc.class_fall,
                "cell_fall",
                &tables.cell_fall,
            ),
        ] {
            let Some(scope) = Scope::of(mode, class, whenless) else {
                continue;
            };

            let half = edge_half(tables, output_edge);
            if half.cell_rise.is_some()
                || half.cell_fall.is_some()
                || half.rise_trans.is_some()
                || half.fall_trans.is_some()
            {
                let sources = source_order.entry((arc.output.clone(), scope)).or_default();
                if !sources.contains(&arc.source) {
                    sources.push(arc.source.clone());
                }
                accumulated
                    .entry((arc.source.clone(), arc.output.clone(), scope))
                    .or_insert_with(|| ArcAccumulator::new(when_merge))
                    .accumulate(half, &arc.source, &arc.output);
            }

            // Only the delay families feed a constraint: a transition is the
            // output's own slew, which the model emits rather than constrains.
            let Some(table) = delays else { continue };
            let input_edge = derived_input_edge(arc.sense, output_edge, &arc.source, &arc.output);
            constraint_arcs
                .entry(ConstraintKey {
                    src: arc.source.clone(),
                    outpin: arc.output.clone(),
                    delay: scope,
                    check,
                    input_edge,
                    family,
                })
                .or_insert_with(|| TableAccumulator::new(when_merge))
                .add(table.clone(), family, &arc.source, &arc.output);
        }
    }

    // Reduce each routed group to its representative arc, then take each scope's
    // reference from the first source whose average is complete enough for it.
    let mut constraint_arcs: ConstraintArcs = constraint_arcs
        .iter()
        .filter_map(|(key, acc)| acc.result().map(|table| (key.clone(), table)))
        .collect();

    for ((outpin_name, scope), sources) in &source_order {
        for related_pin in sources {
            let key = (related_pin.clone(), outpin_name.clone(), *scope);
            let Some(tables) = accumulated.get(&key).and_then(|acc| acc.result()) else {
                continue;
            };

            if let Some(ref_arc) = select_reference_arc(related_pin, &tables, anchor, *scope) {
                eprintln!(
                    "  Pin {} selected as reference arc for output {}",
                    related_pin, outpin_name
                );
                ref_arcs.insert((outpin_name.clone(), *scope), ref_arc);
                break;
            }
        }
    }

    // Phase 2: the reference each converted output's delays are drawn against.
    //
    // No output at all supplying a complete reference is a cell-scope refusal:
    // there is nothing to build a flip-flop model from, so the cell is emitted
    // verbatim and named. This is the last read-only step -- phase 3 onwards mutates
    // the cell, so a cell that will not be converted is never touched.
    let mean_ref_arc = mean_reference_arc(ref_arcs.values().cloned())
        .ok_or_else(|| "no output supplies a complete reference".to_owned())?;

    // Output scope. An output the conversion cannot re-express is skipped, not
    // converted: it keeps its timing groups exactly as the input wrote them and
    // gains no clock-to-output arc, because an output with no arc being split has no
    // clock-to-output delay to state. Convertibility is decided per output, so one
    // skipped output does not cost the cell the outputs that can be converted.
    let converted = |name: &str| ref_arcs.keys().any(|(output, _)| output == name);
    let skipped: Vec<String> = output_leaves
        .iter()
        .filter(|name| !converted(name))
        .cloned()
        .collect();
    for outpin_name in &skipped {
        // All three are the same output-scope refusal; the wording says which of the three
        // ways the output fell short, so the warning can be acted on. The arc-scope case
        // has to be asked about separately: an arc skipped there -- for a template missing
        // an axis, for a direction the tool cannot derive, or, under a per-state reference,
        // for a `when` it cannot read -- never reaches `source_order`, so judging by that
        // map alone would report an output skipped for any of those reasons as having
        // carried no table at all.
        let reason = if source_order.keys().any(|(output, _)| output == outpin_name) {
            "no non-reset source supplies a complete reference"
        } else if let Some(reason) = arc_skipped.get(outpin_name) {
            reason
        } else {
            "no non-reset timing arc carrying a characterisation table"
        };
        eprintln!(
            "Skipping output {} of cell {} in library {}: {}",
            outpin_name, cell_name, lib_name, reason
        );
        reports.refusals.push(Refusal {
            library: lib_name.to_owned(),
            cell: cell_name.clone(),
            output: Some(outpin_name.clone()),
            reason: reason.to_owned(),
        });
    }

    // Output scope, and per-state only: an arc whose `when` could not be read was
    // skipped. Restricted to outputs that still converted despite the skip: one
    // that did not convert already carries the output-scope refusal above, drawn
    // from the same skip via `arc_skipped`, and must not carry this one as well.
    // Named here rather than at the skip either way, because a cell that turns out
    // not to convert at all is emitted verbatim and makes no output-scope refusal
    // about anything.
    //
    // `unique()`d: `unreadable_arcs` carries one entry per skipped arc, and an
    // output whose `when` could not be read on two separate arcs would otherwise
    // read this list twice under the same name, and so be given the same refusal
    // twice over -- an identical reason repeated tells the reader nothing a single
    // one would not.
    for outpin_name in unreadable_arcs.iter().unique() {
        if !converted(outpin_name) {
            continue;
        }
        reports.refusals.push(Refusal {
            library: lib_name.to_owned(),
            cell: cell_name.clone(),
            output: Some(outpin_name.clone()),
            reason: "an arc was skipped: its `when` could not be read, and a per-state \
                     reference is drawn per condition"
                .to_owned(),
        });
    }

    // Phase 3: Add pseudo timing to each output pin
    for outpin in timing_leaves_mut(cell, is_output_pin) {
        let outpin_name = outpin.name.clone();

        // One arc per state this output was converted in, conditioned states first
        // and the catch-all last -- which is `Scope`'s own order, so the map supplies
        // it. A skipped output has none, is left exactly as the input wrote it, and
        // phase 2 has already named it. Note that the retain which strips a converted
        // output's original non-reset arcs lives inside
        // `add_pseudo_timing_to_output_pin`, so an empty list here is what keeps a
        // skipped output's originals in the default mode -- under `--latch` they
        // survive by construction either way.
        let arcs: Vec<PseudoArc> = ref_arcs
            .iter()
            .filter(|((output, _), _)| *output == outpin_name)
            .map(|((_, scope), transitions)| PseudoArc {
                transitions,
                // Pooled hands every output the cell-wide mean delay; the other two
                // let each reference keep the delay it was drawn from.
                delays: match mode {
                    ReferenceMode::Pooled => &mean_ref_arc,
                    ReferenceMode::PerOutput | ReferenceMode::PerState => transitions,
                },
                guard: match scope {
                    Scope::Whole => Guard::Unguarded,
                    Scope::State(class) => {
                        Guard::Conditioned(class_conditions.get(class).unwrap_or_else(|| {
                            panic!(
                                "internal: state {:?} of output {} was drawn a reference \
                                 without ever being classified",
                                class, outpin_name
                            )
                        }))
                    }
                    Scope::CatchAll => Guard::CatchAll,
                },
            })
            .collect();
        if arcs.is_empty() {
            continue;
        }

        add_pseudo_timing_to_output_pin(outpin, clock_name, reset_name, &arcs, latch);
    }

    // An input driving both a converted and a skipped output is constrained over the
    // converted outputs only, and an input driving no converted output gets no
    // constraint at all -- the degenerate case of the same rule, which phase 5's
    // `has_constraints` then leaves untouched of its own accord.
    //
    // Restricting the arc maps here *is* that rule, and it is also what keeps a
    // skipped output out of every reference computation in both modes: its arcs are
    // no longer present to be averaged. The two halves of the split must sum back to
    // the arc they came from, and a skipped output supplies no propagation half, so
    // there is nothing for an input driving it to be charged against.
    //
    // The reference has to carry the entry's own FAMILY, not merely its scope: a
    // state characterised in one direction alone emits a delay for that direction
    // alone, and a check referred to a delay the model never states would describe a
    // path with no propagation half. A whole-output reference always carries both, so
    // this is exactly the output test it has always been there.
    constraint_arcs.retain(|key, _| {
        ref_arcs
            .get(&(key.outpin.clone(), key.delay))
            .and_then(|r| family_reference(r, key.family))
            .is_some()
    });

    // Phase 4: Calculate setup/hold constraints against the reference `mode` selects
    let ref_arc = mean_ref_arc;

    let (setup_input_rise, setup_input_fall) =
        calculate_setup_constraints(&constraint_arcs, &ref_arcs, &ref_arc, mode, anchor);

    let (hold_input_rise, hold_input_fall) =
        calculate_hold_constraints(&setup_input_rise, &setup_input_fall);

    let references = References {
        per_output: &ref_arcs,
        mean: &ref_arc,
        mode,
    };
    let (slews, loads) = match (cell_axes, axes_agree) {
        (Some(axes), true) => (axes.slew, axes.load),
        _ => (None, None),
    };

    // Both constraint maps go in, because which of them an arc was folded into is
    // the arc's own property -- its input direction -- not the caller's to choose.
    let mut arc_errors: Vec<ArcError> = Vec::new();
    for edge in [&RISE, &FALL] {
        collect_arc_errors(
            &raw_arcs,
            &setup_input_rise,
            &setup_input_fall,
            &references,
            &cell_name,
            edge,
            &mut arc_errors,
        );
    }

    // The condition each check class denotes, so a constraint filed under one can be
    // captioned with the condition its checks are stated under. Read off the groups
    // themselves rather than classified a second time, and keyed by the pin as well
    // as the class because the classes are numbered per pin.
    //
    // In the source library's own spelling, which is the spelling the emitted `when`
    // carries: a caption is read to correlate a constraint with the group it ships
    // in, so it names that group as the library will see it and not as this tool
    // would have rendered it.
    let check_conditions: BTreeMap<(String, ClassId), String> = check_groups
        .iter()
        .flat_map(|(pin, pin_groups)| {
            pin_groups.iter().filter_map(move |group| {
                match (group.scope, group.condition.as_ref()) {
                    (Scope::State(class), Some(condition)) => {
                        Some(((pin.clone(), class), condition.as_written()))
                    }
                    _ => None,
                }
            })
        })
        .collect();

    reports.cells.push(CellReport {
        library: lib_name.to_owned(),
        cell: cell_name.clone(),
        when_merge,
        raw_arcs,
        constraint_arcs: constraint_arcs.clone(),
        ref_arcs: ref_arcs.clone(),
        mean_ref: ref_arc.clone(),
        setup_input_rise: setup_input_rise.clone(),
        setup_input_fall: setup_input_fall.clone(),
        hold_input_rise: hold_input_rise.clone(),
        hold_input_fall: hold_input_fall.clone(),
        slews,
        loads,
        arcs: arc_errors,
        classes,
        class_conditions: match mode {
            ReferenceMode::PerState => class_conditions
                .iter()
                .map(|(class, condition)| (*class, condition.liberty()))
                .collect(),
            _ => BTreeMap::new(),
        },
        check_classes,
        check_conditions,
    });

    // Phase 5: Add constraints to every input the library characterised against
    // an output. A bundle takes them itself when the arcs name the bundle, or
    // delegates to its members when the arcs name the members.
    // The clock is never constrained against itself, and a pin the library never
    // characterised against an output has nothing to be constrained by.
    //
    // Reset pins need no test of their own: phase 1 skips every arc whose
    // `related_pin` matches `reset_name`, so a reset name never becomes a key of
    // `constraint_arcs`, hence never of the setup maps those are grouped into.
    // Membership below therefore already implies the name is not a reset. The clock
    // has no such skip -- a latch characterises its output against the enable -- so
    // that test is live.
    let has_constraints = |name: &str| {
        name != clock_name
            && setup_input_rise
                .keys()
                .chain(setup_input_fall.keys())
                .any(|(source, _)| source == name)
    };

    for inpin in constraint_targets_mut(cell, &has_constraints) {
        let inpin_name = inpin.name.clone();
        // One entry per condition the library characterised this pin under, or one
        // unconditioned entry where the mode draws a single reference per output.
        // Both of an entry's directions are looked up at the group's own scope,
        // because that is what the values were summed by: a check on the pin pair
        // `D -> G` is stated under `D`'s condition, so one condition is one value per
        // direction however many outputs `D` drives under it.
        let entries: Vec<CheckEntry> = match check_groups.get(&inpin_name) {
            None => vec![CheckEntry {
                guard: Guard::Unguarded,
                setup_rise: setup_input_rise.get(&(inpin_name.clone(), Scope::Whole)),
                setup_fall: setup_input_fall.get(&(inpin_name.clone(), Scope::Whole)),
                hold_rise: hold_input_rise.get(&(inpin_name.clone(), Scope::Whole)),
                hold_fall: hold_input_fall.get(&(inpin_name.clone(), Scope::Whole)),
            }],
            Some(groups) => groups
                .iter()
                .map(|group| CheckEntry {
                    guard: match &group.condition {
                        // The source condition verbatim: nothing conjoined, no
                        // transition literal, no edge. What the check constrains is
                        // said by `timing_type`, not by the condition it holds under.
                        Some(condition) => Guard::Conditioned(condition),
                        None => Guard::CatchAll,
                    },
                    setup_rise: setup_input_rise.get(&(inpin_name.clone(), group.scope)),
                    setup_fall: setup_input_fall.get(&(inpin_name.clone(), group.scope)),
                    hold_rise: hold_input_rise.get(&(inpin_name.clone(), group.scope)),
                    hold_fall: hold_input_fall.get(&(inpin_name.clone(), group.scope)),
                })
                // A group whose every arc was dropped for want of a reference has no
                // constraint table to carry, and UG p.7-56 asks each one for at least
                // one. Emitting the empty group would state a check with no value.
                .filter(|entry| entry.setup_rise.is_some() || entry.setup_fall.is_some())
                .collect(),
        };

        add_constraints_to_input_pin(inpin, clock_name, &ref_arc, &entries);
    }

    // Phase 6: Convert latch to flip-flop if needed
    if !latch {
        convert_latch_to_flipflop(cell);
    }

    // Return the lut_template name for library-level template generation
    Ok(ref_arc.lut_template)
}

/// Process a library to convert latches to flip-flops or add pseudo-synchronous
/// timing, choosing how the clock-to-output reference is drawn.
///
/// Returns a [`CellReport`] per converted cell -- carrying the original arcs, the
/// reconstruction and its residual, so the cost of the chosen [`ReferenceMode`] can be
/// measured against the library it replaced -- together with a [`Refusal`] for every
/// candidate cell, or output of one, the conversion could not honour.
pub(crate) fn process_library(lib: &mut Group, opts: &CellOptions) -> LibraryReport {
    eprintln!("Processing library {}", lib.name);

    let clock_name = opts.clock_name;
    let mut reports = LibraryReport::default();

    let mut lut_templates: HashSet<String> = HashSet::new();
    let lib_name = lib.name.clone();
    // Owned, so the immutable borrow of the library ends here and the cells below
    // can be walked mutably.
    let templates = Templates::of_library(lib);

    // Process each qualifying cell
    for cell in lib
        .iter_cells_mut()
        .filter(|x| cell_qualifies(x, clock_name))
    {
        match process_cell(cell, opts, &templates, &lib_name, &mut reports) {
            Ok(template_name) => {
                lut_templates.insert(template_name);
            }
            // A candidate the conversion could not honour: left verbatim, named on
            // standard error, and recorded in the report. With the exit status pinned
            // at 0 those two are the only signals a caller has, so a refusal that
            // reached only one of them would be half invisible.
            Err(reason) => {
                eprintln!(
                    "Failed to process cell {} of library {}: {}",
                    cell.name, lib_name, reason
                );
                reports.refusals.push(Refusal {
                    library: lib_name.clone(),
                    cell: cell.name.clone(),
                    output: None,
                    reason,
                });
            }
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
    //! Behaviour of the `engine` module: constraint calculation and bundle traversal.

    use super::*;
    use crate::arcs::{mean_reference_arc, Anchor, EdgeRef, RefArc, ReferenceMode, WhenMerge};
    use crate::liberty_io::parse_liberty_file;
    use crate::pins::{cell_qualifies, is_output_pin};
    use liberty_parser::{
        ast::Value,
        liberty::{Group, Liberty},
    };
    use regex::Regex;
    use std::collections::BTreeMap;
    use std::path::Path;

    /// The conversion knobs, with the anchor at the default the command line
    /// supplies. The anchor is exercised where it is decided, in `arcs`; here it
    /// only has to stay out of the way.
    fn opts<'a>(
        clock_name: &'a str,
        reset_name: &'a Regex,
        latch: bool,
        mode: ReferenceMode,
        when_merge: WhenMerge,
    ) -> CellOptions<'a> {
        CellOptions {
            clock_name,
            reset_name,
            latch,
            mode,
            when_merge,
            anchor: Anchor::Middle,
        }
    }

    // --- calculate_setup / hold constraints --------------------------------

    /// Killed by: `constraints_from_arcs` added the reference delay to the sampled column instead of subtracting it.
    #[test]
    fn setup_constraint_is_input_arc_minus_reference_delay() {
        // One input->output rise arc for source pin "D"; column `col` is what
        // the reference samples. ref.cell_rise[col] is subtracted off.
        let col = 1usize;
        // 3x3 arc whose column 1 is [25, 35, 45]
        let arc =
            Array2::from_shape_vec((3, 3), vec![0.0, 25.0, 0.0, 0.0, 35.0, 0.0, 0.0, 45.0, 0.0])
                .unwrap();
        // Positive unate, so a `cell_rise` table is what an input rise contributed.
        let mut constraint_arcs: ConstraintArcs = BTreeMap::new();
        constraint_arcs.insert(
            ConstraintKey {
                src: "D".to_owned(),
                outpin: "Q".to_owned(),
                delay: Scope::Whole,
                check: Scope::Whole,
                input_edge: Transition::Rise,
                family: "cell_rise",
            },
            arc,
        );

        let ref_arc = RefArc {
            col,
            row: 1,
            related_pin: "CK".to_owned(),
            lut_template: "T".to_owned(),
            anchor: Anchor::Middle,
            rise: Some(EdgeRef {
                delay: Array1::from(vec![10.0, 20.0, 30.0]),
                transition: Array1::from(vec![0.0, 0.0, 0.0]),
                // The reference delay at the anchor point: profile[col] = 20.
                crossing: 20.0,
            }),
            fall: Some(EdgeRef {
                delay: Array1::from(vec![0.0, 0.0, 0.0]),
                transition: Array1::from(vec![0.0, 0.0, 0.0]),
                crossing: 0.0,
            }),
        };

        // With one output there is nothing to pool, so both modes must agree.
        let ref_arcs: BTreeMap<(String, Scope), RefArc> =
            BTreeMap::from([(("Q".to_owned(), Scope::Whole), ref_arc.clone())]);
        let whole = ("D".to_owned(), Scope::Whole);

        for mode in [ReferenceMode::Pooled, ReferenceMode::PerOutput] {
            let (setup_rise, setup_fall) = calculate_setup_constraints(
                &constraint_arcs,
                &ref_arcs,
                &ref_arc,
                mode,
                Anchor::Middle,
            );

            // [25,35,45] - 20 = [5,15,25]
            assert_eq!(
                setup_rise[&whole],
                Array1::from(vec![5.0, 15.0, 25.0]),
                "{:?}",
                mode
            );
            assert!(setup_fall.is_empty(), "{:?}", mode);

            // hold = -setup
            let (hold_rise, hold_fall) = calculate_hold_constraints(&setup_rise, &setup_fall);
            assert_eq!(
                hold_rise[&whole],
                Array1::from(vec![-5.0, -15.0, -25.0]),
                "{:?}",
                mode
            );
            assert!(hold_fall.is_empty(), "{:?}", mode);
        }
    }

    /// A source driving one rail of a dual-rail cell must be referenced against
    /// that rail alone under `PerOutput`, and against both rails under `Pooled`.
    ///
    /// Killed by: `constraints_from_arcs` gave `PerOutput` the pooled `select(mean_ref)[col]` instead of `ref_sum / n`.
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
        };

        // Rail 1 is fast (ref 10), rail 2 slow (ref 30); pooled mean is 20.
        let ref_arcs: BTreeMap<(String, Scope), RefArc> = BTreeMap::from([
            (("Q1".to_owned(), Scope::Whole), refarc(10.0)),
            (("Q2".to_owned(), Scope::Whole), refarc(30.0)),
        ]);
        let mean_ref = mean_reference_arc(ref_arcs.values().cloned()).unwrap();
        assert_eq!(mean_ref.rise.as_ref().expect("a rise edge").crossing, 20.0);

        // Both scopes whole, which is every entry either of these modes files.
        let rise = |src: &str, out: &str| ConstraintKey {
            src: src.to_owned(),
            outpin: out.to_owned(),
            delay: Scope::Whole,
            check: Scope::Whole,
            input_edge: Transition::Rise,
            family: "cell_rise",
        };
        let constraint_arcs: ConstraintArcs = BTreeMap::from([
            // D1 is rail-private: it drives Q1 only.
            (rise("D1", "Q1"), arc(100.0)),
            // S is shared: it drives both rails.
            (rise("S", "Q1"), arc(100.0)),
            (rise("S", "Q2"), arc(100.0)),
        ]);

        let (pooled, _) = calculate_setup_constraints(
            &constraint_arcs,
            &ref_arcs,
            &mean_ref,
            ReferenceMode::Pooled,
            Anchor::Middle,
        );
        let (per_output, _) = calculate_setup_constraints(
            &constraint_arcs,
            &ref_arcs,
            &mean_ref,
            ReferenceMode::PerOutput,
            Anchor::Middle,
        );

        let of = |source: &str| (source.to_owned(), Scope::Whole);
        // Pooled charges both sources the cell-wide mean: 100 - 20.
        assert_eq!(pooled[&of("D1")], Array1::from(vec![80.0]));
        assert_eq!(pooled[&of("S")], Array1::from(vec![80.0]));

        // PerOutput charges the rail-private source its own rail: 100 - 10.
        assert_eq!(per_output[&of("D1")], Array1::from(vec![90.0]));
        // The shared source drives every output, so its driven mean is the
        // pooled mean and it is left unchanged.
        assert_eq!(per_output[&of("S")], Array1::from(vec![80.0]));
    }

    // --- bundle traversal --------------------------------------------------

    /// Four timing tables for one arc, so `select_reference_arc` accepts it.
    fn arc(related_pin: &str, timing_type: &str, base: f64) -> String {
        format!(
            r#"
        timing() {{
          related_pin: "{}";
          timing_sense : positive_unate;
          timing_type: {};
          cell_rise(T) {{ values("{}, {}", "{}, {}"); }}
          cell_fall(T) {{ values("{}, {}", "{}, {}"); }}
          rise_transition(T) {{ values("0.1, 0.2", "0.3, 0.4"); }}
          fall_transition(T) {{ values("0.11, 0.21", "0.31, 0.41"); }}
        }}"#,
            related_pin,
            timing_type,
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

    /// The timing groups of one `timing_type`, so both how many there are and what
    /// they contain can be asserted.
    fn arcs_of_type<'a>(group: &'a Group, timing_type: &str) -> Vec<&'a Group> {
        group
            .iter_subgroups_of_type("timing")
            .filter(|t| {
                t.simple_attribute("timing_type")
                    .map(|tt| tt.expr() == timing_type)
                    .unwrap_or(false)
            })
            .collect()
    }

    /// How many arcs a group carries for each `(related_pin, timing_type)` pair.
    type ArcCensus = BTreeMap<(String, String), usize>;

    /// The timing arcs of a group, counted by `(related_pin, timing_type)`.
    ///
    /// Liberty attaches no meaning to the order of a pin's arcs, so a whole pin's
    /// arc population is pinned as a census rather than by position.
    fn arc_census(group: &Group) -> ArcCensus {
        let mut census: ArcCensus = ArcCensus::new();
        for timing in group.iter_subgroups_of_type("timing") {
            let related = timing
                .simple_attribute("related_pin")
                .map_or_else(String::new, |v| v.string());
            let kind = timing
                .simple_attribute("timing_type")
                .map_or_else(String::new, |v| v.expr());
            *census.entry((related, kind)).or_default() += 1;
        }
        census
    }

    /// The expected counterpart of [`arc_census`], written out as it reads.
    fn census(entries: &[(&str, &str, usize)]) -> ArcCensus {
        entries
            .iter()
            .map(|(related, kind, n)| (((*related).to_owned(), (*kind).to_owned()), *n))
            .collect()
    }

    /// What flip-flop mode must leave on an output pin, as a function of what that
    /// pin started with.
    ///
    /// Both halves follow from the model rather than from any run.
    /// [`add_pseudo_timing_to_output_pin`] retains a `timing` group if and only if
    /// its `related_pin` matches the reset pattern, so every reset arc survives
    /// with exactly the count the library declared -- that count is a property of
    /// the FIXTURE, not of the tool -- and every other arc goes. In their place a
    /// pseudo-flop declares one `rising_edge` arc against the clock per output it
    /// could characterise. So the expected census is the original one filtered to
    /// the reset, plus that single arc.
    fn ff_census(original: &ArcCensus, clock: &str, reset_name: &Regex) -> ArcCensus {
        let mut expected: ArcCensus = original
            .iter()
            .filter(|((related, _), _)| reset_name.is_match(related))
            .map(|(key, n)| (key.clone(), *n))
            .collect();
        *expected
            .entry((clock.to_owned(), "rising_edge".to_owned()))
            .or_default() += 1;
        expected
    }

    /// The one pseudo-synchronous arc an output pin carries against the clock.
    fn pseudo_output_arc<'a>(pin: &'a Group, clock: &str) -> &'a Group {
        let arcs: Vec<&Group> = arcs_of_type(pin, "rising_edge")
            .into_iter()
            .filter(|t| {
                t.simple_attribute("related_pin")
                    .map(|rp| rp.string() == clock)
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(
            arcs.len(),
            1,
            "pin {} should carry exactly one {} arc",
            pin.name,
            clock
        );
        arcs[0]
    }

    /// The numbers of a `values(...)` table, flattened row-major.
    fn table_values(group: &Group) -> Vec<f64> {
        group
            .complex_attribute("values")
            .map(|values| {
                values
                    .iter()
                    .flat_map(|v| match v {
                        Value::FloatGroup(row) => row.clone(),
                        Value::Float(x) => vec![*x],
                        other => panic!("table value {:?} is not numeric", other),
                    })
                    .collect()
            })
            .unwrap_or_default()
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
    ///
    /// Killed by: `constraint_targets_mut`'s `else if has_constraints(&g.name)` widened to `else if !has_constraints(&g.name) || true`, so the bundle container took constraints too.
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
            arc("D1", "combinational", 1.0),
            arc("D2", "combinational", 2.0)
        );

        let mut lib = bundle_lib(body);
        process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
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

    /// Delegating to the members is not the same as constraining all of them: a
    /// member takes constraints only if the library characterised it.
    ///
    /// `D1` drives both outputs, so its name is harvested from the arcs and it has a
    /// constraint to take. `D2` is declared, is a real input and is a member of the
    /// same bundle, but no arc ever names it -- there is no delay to subtract a
    /// reference from, so it must come through with nothing at all. An empty setup
    /// pair on it would be timing no tool can use, and the marker alone would claim
    /// a data input the library never characterised.
    ///
    /// Killed by: `constraint_targets_mut` dropped `&& has_constraints(&s.name)` from its member filter.
    #[test]
    fn only_characterised_members_of_a_delegating_bundle_take_constraints() {
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
            arc("D1", "combinational", 1.0),
            arc("D1", "combinational", 2.0)
        );

        let mut lib = bundle_lib(body);
        process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        let cell = lib[0].get_cell("DUT").expect("DUT");

        // The characterised member takes the marker and a populated pair.
        let d1 = member(cell, "D", "D1");
        assert_eq!(
            d1.simple_attribute("nextstate_type").unwrap().expr(),
            "data"
        );
        for tt in ["setup_rising", "hold_rising"] {
            assert!(
                constraint_is_populated(d1, tt),
                "D1 drove both outputs and needs a populated {}",
                tt
            );
        }

        // The uncharacterised member of the same bundle takes nothing.
        let d2 = member(cell, "D", "D2");
        assert!(
            d2.simple_attribute("nextstate_type").is_none(),
            "D2 was never characterised and must not be marked as data"
        );
        assert_eq!(
            d2.iter_subgroups_of_type("timing").count(),
            0,
            "D2 was never characterised and must not carry a constraint group"
        );

        // Nor does the container the constraints were delegated from.
        let d_bundle = cell
            .iter_subgroups_of_type("bundle")
            .find(|b| b.name == "D")
            .unwrap();
        assert!(d_bundle.simple_attribute("nextstate_type").is_none());
        assert_eq!(d_bundle.iter_subgroups_of_type("timing").count(), 0);
    }

    /// A bundle that owns its timing arcs is the leaf itself, and the arcs name
    /// the bundle rather than its members. This is the form the ASCEND libraries
    /// use, and it must keep being handled at bundle level.
    ///
    /// Killed by: `timing_leaves_mut` dropped `&& !owns_timing_arcs(g)`, so a bundle owning its arcs delegated to its members anyway.
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
            arc("D", "combinational", 1.0)
        );

        let mut lib = bundle_lib(body);
        process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
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

    /// The engine performs no I/O of its own: the reconstruction report is
    /// returned as data and written by the caller to a path it chooses.
    ///
    /// This once asserted that the `pseudosync.txt` writer was "dead code", which
    /// was false -- it was live until the report facility was dropped by accident
    /// in a refactor. The assertion is kept because writing to the process CWD
    /// from library code is the behaviour that made that loss invisible, but it
    /// now guards a deliberate design rather than ratifying an accident.
    ///
    /// It checks without relocating the process. The working directory is
    /// process-global and the harness runs these tests on parallel threads, so moving it
    /// would move the ground under every other test in the binary. Nothing else in the
    /// tree writes this name, so asserting on the directory the run starts in
    /// discriminates just as well -- and the precondition makes a file left over from an
    /// earlier run a loud failure instead of a silent false pass.
    ///
    /// Killed by: `process_library` wrote a `pseudosync.txt` into the process working
    /// directory, reddening the assertion after the run. Reverted, and the file the
    /// mutation left behind deleted.
    #[test]
    fn engine_does_not_leak_pseudosync_txt_in_cwd() {
        let leaked = std::env::current_dir()
            .expect("read working directory")
            .join("pseudosync.txt");

        // A precondition rather than an assumption: a file already sitting there would
        // fail this test for a reason that is not this run's.
        assert!(
            !leaked.exists(),
            "{} exists before the run -- remove it; this test can say nothing while it is there",
            leaked.display()
        );

        let mut lib = sample_lib();
        process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );

        assert!(
            !leaked.exists(),
            "the engine wrote {} into the working directory",
            leaked.display()
        );
    }

    // --- whole-cell conversion, on a synthetic latch ------------------------

    /// A latch shaped like the parts this tool was written for: an enable, an
    /// asynchronous reset that owns an arc to the output, a data input the library
    /// characterised against that output, and an input it never characterised at
    /// all.
    fn latch_cell_lib() -> Liberty {
        liberty_parser::parse_lib(&format!(
            r#"
library(latch_cell_test) {{
  lu_table_template(T) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}
  cell(LATCH_CELL) {{
    latch(IQ, IQN) {{ clear: "!RN"; data_in: "A"; enable: "G"; }}
    pin(G) {{ direction: input; clock: true; }}
    pin(RN) {{ direction: input; }}
    pin(A) {{ direction: input; }}
    pin(B) {{ direction: input; }}
    pin(Q) {{
      direction: output;
      function: "IQ";
      {}
      {}
    }}
  }}
}}"#,
            arc("A", "combinational", 1.0),
            arc("RN", "clear", 2.0)
        ))
        .expect("parse latch cell fixture")
    }

    fn converted_latch_cell(latch: bool) -> Liberty {
        let mut lib = latch_cell_lib();
        process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                latch,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        lib
    }

    /// The four tables the pseudo output arc must carry, all named after the
    /// generated delay template.
    fn assert_pseudo_delay_tables(pseudo: &Group) {
        assert_eq!(
            pseudo.iter_subgroups().count(),
            4,
            "the pseudo arc carries exactly the four delay tables"
        );
        for table in [
            "cell_rise",
            "cell_fall",
            "rise_transition",
            "fall_transition",
        ] {
            let groups: Vec<&Group> = pseudo.iter_subgroups_of_type(table).collect();
            assert_eq!(groups.len(), 1, "one {} table", table);
            assert!(
                groups[0].name.ends_with("_pseudo_delay"),
                "{} table is named {:?}, not a pseudo delay template",
                table,
                groups[0].name
            );
            assert!(
                !table_values(groups[0]).is_empty(),
                "{} table has no values",
                table
            );
        }
    }

    /// Latch mode is additive: every characterised arc survives and the pseudo
    /// clock-to-output arc is appended alongside them.
    ///
    /// Killed by: `add_pseudo_timing_to_output_pin` guarded its retain with `if latch` instead of `if !latch`, stripping the original arcs in latch mode.
    #[test]
    fn latch_mode_appends_the_pseudo_arc_to_the_original_ones() {
        let lib = converted_latch_cell(true);
        let cell = lib[0].get_cell("LATCH_CELL").expect("LATCH_CELL");
        let q = cell.get_pin("Q").expect("Q");

        // [`latch_cell_lib`] writes Q exactly two arcs -- A/combinational and
        // RN/clear -- and latch mode retains every one of them, so the census is
        // those two plus the single clock arc a pseudo-flop declares per output.
        assert_eq!(
            arc_census(q),
            census(&[
                ("A", "combinational", 1),
                ("RN", "clear", 1),
                ("G", "rising_edge", 1),
            ])
        );

        let pseudo = pseudo_output_arc(q, "G");
        assert_eq!(
            pseudo.simple_attribute("timing_sense").unwrap().expr(),
            "non_unate"
        );
        assert_pseudo_delay_tables(pseudo);
    }

    /// Flip-flop mode replaces the combinational model rather than extending it:
    /// every arc the reset does not own is stripped off the output.
    ///
    /// Killed by: `add_pseudo_timing_to_output_pin`'s retain negated its reset test -- `|| !reset_name.is_match(..)` -- keeping exactly the arcs it should drop.
    #[test]
    fn ff_mode_strips_every_output_arc_the_reset_does_not_own() {
        let lib = converted_latch_cell(false);
        let cell = lib[0].get_cell("LATCH_CELL").expect("LATCH_CELL");
        let q = cell.get_pin("Q").expect("Q");

        // Of the two arcs [`latch_cell_lib`] writes Q, only RN's `related_pin`
        // matches the reset pattern, so the retain step keeps that one and drops
        // A -> Q -- a flip-flop still needs its asynchronous clear characterised.
        // One clock arc replaces what was dropped.
        assert_eq!(
            arc_census(q),
            census(&[("RN", "clear", 1), ("G", "rising_edge", 1)])
        );
    }

    /// Flip-flop mode renames the latch's control attributes to their flip-flop
    /// spellings, keeping the expressions themselves.
    ///
    /// Killed by: `convert_latch_to_flipflop` removed the key `"enable_not_a_key"`, leaving `enable` unrenamed.
    #[test]
    fn ff_mode_renames_the_latch_control_attributes() {
        let lib = converted_latch_cell(false);
        let cell = lib[0].get_cell("LATCH_CELL").expect("LATCH_CELL");

        assert_eq!(cell.iter_subgroups_of_type("latch").count(), 0);
        let ffs: Vec<&Group> = cell.iter_subgroups_of_type("ff").collect();
        assert_eq!(ffs.len(), 1, "exactly one ff group");
        assert_eq!(ffs[0].name, "IQ, IQN");
        assert_eq!(ffs[0].simple_attribute("clocked_on").unwrap().string(), "G");
        assert_eq!(ffs[0].simple_attribute("next_state").unwrap().string(), "A");
        assert_eq!(ffs[0].simple_attribute("clear").unwrap().string(), "!RN");
        assert!(ffs[0].simple_attribute("enable").is_none());
        assert!(ffs[0].simple_attribute("data_in").is_none());
    }

    /// Latch mode leaves the state element exactly as the library declared it.
    ///
    /// Killed by: `process_cell`'s phase 6 guarded `convert_latch_to_flipflop` with `if latch` instead of `if !latch`.
    #[test]
    fn latch_mode_leaves_the_latch_group_intact() {
        let lib = converted_latch_cell(true);
        let cell = lib[0].get_cell("LATCH_CELL").expect("LATCH_CELL");

        let latches: Vec<&Group> = cell.iter_subgroups_of_type("latch").collect();
        assert_eq!(latches.len(), 1, "the latch group survives latch mode");
        assert_eq!(latches[0].name, "IQ, IQN");
        assert_eq!(
            latches[0].simple_attribute("clear").unwrap().string(),
            "!RN"
        );
        assert_eq!(latches[0].simple_attribute("enable").unwrap().string(), "G");
        assert_eq!(
            latches[0].simple_attribute("data_in").unwrap().string(),
            "A"
        );
        assert_eq!(
            cell.iter_subgroups_of_type("ff").count(),
            0,
            "no ff group is created in latch mode"
        );
    }

    /// Constraints reach the inputs the library characterised against an output,
    /// and only those.
    ///
    /// An empty `setup_rising` group -- one with no rise/fall constraint table in
    /// it -- is not usable timing, so a pin with nothing to be constrained by must
    /// be left alone rather than given one.
    ///
    /// Killed by: `constraint_targets_mut`'s `else if has_constraints(&g.name)` widened to `else if !has_constraints(&g.name) || true`, so an uncharacterised input took an empty pair.
    #[test]
    fn constraints_reach_characterised_data_inputs_only() {
        let lib = converted_latch_cell(false);
        let cell = lib[0].get_cell("LATCH_CELL").expect("LATCH_CELL");

        let a = cell.get_pin("A").expect("A");
        assert_eq!(a.simple_attribute("nextstate_type").unwrap().expr(), "data");
        for timing_type in ["setup_rising", "hold_rising"] {
            assert_eq!(
                arcs_of_type(a, timing_type).len(),
                1,
                "A carries one {}",
                timing_type
            );
            assert!(
                constraint_is_populated(a, timing_type),
                "A's {} carries no constraint table",
                timing_type
            );
        }

        // RN is excluded by name, and B was never characterised against Q -- so
        // there is nothing to constrain it by, and an empty group is the only thing
        // it could be given.
        for name in ["RN", "B"] {
            let pin = cell
                .get_pin(name)
                .unwrap_or_else(|| panic!("{} pin not found", name));
            assert!(
                pin.simple_attribute("nextstate_type").is_none(),
                "{} has no characterised constraint and must not be marked as data",
                name
            );
            for timing_type in ["setup_rising", "hold_rising"] {
                assert!(
                    arcs_of_type(pin, timing_type).is_empty(),
                    "{} must not receive an empty {} group",
                    name,
                    timing_type
                );
            }
        }
    }

    /// The clock is never constrained against itself.
    ///
    /// A transparent latch is characterised clock-to-output, so the clock is a
    /// genuine arc source and does end up in the setup map -- it is excluded from
    /// the constraint targets by name, not by having nothing to offer. It takes a
    /// cell whose output arcs are all related to the clock to observe that: in a
    /// cell characterised data-to-output the clock is never a source in the first
    /// place, and the exclusion is unobservable.
    ///
    /// Killed by: `process_cell`'s `has_constraints` tested `name != ""` instead of `name != clock_name`.
    #[test]
    fn the_clock_is_never_constrained_against_itself() {
        let mut lib = liberty_parser::parse_lib(&format!(
            r#"
library(clock_sourced_test) {{
  lu_table_template(T) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}
  cell(TRANSPARENT) {{
    latch(IQ, IQN) {{ clear: "!RN"; data_in: "A"; enable: "G"; }}
    pin(G) {{ direction: input; clock: true; }}
    pin(RN) {{ direction: input; }}
    pin(A) {{ direction: input; }}
    pin(Q) {{
      direction: output;
      function: "IQ";
      {}
    }}
  }}
}}"#,
            arc("G", "rising_edge", 1.0)
        ))
        .expect("parse clock-sourced fixture");

        process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        let cell = lib[0].get_cell("TRANSPARENT").expect("TRANSPARENT");

        for name in ["G", "A", "RN"] {
            let pin = cell
                .get_pin(name)
                .unwrap_or_else(|| panic!("{} pin not found", name));
            assert!(
                pin.simple_attribute("nextstate_type").is_none(),
                "{} must not be marked as data",
                name
            );
            for timing_type in ["setup_rising", "hold_rising"] {
                assert!(
                    arcs_of_type(pin, timing_type).is_empty(),
                    "{} must not be given a {}",
                    name,
                    timing_type
                );
            }
        }
    }

    // --- a cell only half of whose outputs can be characterised --------------

    /// A two-output latch the library only half characterised.
    ///
    /// `Q` carries all four tables, so `select_reference_arc` yields a reference
    /// for it. `QN`'s arc is missing `fall_transition`, one of the four that
    /// function requires, so it yields none for `QN` -- while the arc still holds
    /// enough tables for `extract_timing_tables_from_arc` to accept it, which
    /// puts `QN` among the outputs the conversion undertakes to re-describe.
    ///
    /// So this is the mixed case: `Q` converts, `QN` is skipped at output scope, and
    /// `A` drives both. Convertibility is decided per output, so the cell is not lost
    /// for `QN`'s sake -- but `A`'s constraint must be computed over `Q` alone.
    fn half_characterised_lib() -> Liberty {
        liberty_parser::parse_lib(&format!(
            r#"
library(half_characterised_test) {{
  lu_table_template(T) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}
  cell(HALF) {{
    latch(IQ, IQN) {{ enable: "G"; data_in: "A"; }}
    pin(G) {{ direction: input; clock: true; }}
    pin(A) {{ direction: input; }}
    pin(Q) {{
      direction: output;
      function: "IQ";
      {}
    }}
    pin(QN) {{
      direction: output;
      function: "IQN";
      timing() {{
        related_pin: "A";
        timing_sense : positive_unate;
        timing_type: combinational;
        cell_rise(T) {{ values("2.0, 3.0", "4.0, 5.0"); }}
        cell_fall(T) {{ values("2.5, 3.5", "4.5, 5.5"); }}
        rise_transition(T) {{ values("0.1, 0.2", "0.3, 0.4"); }}
      }}
    }}
  }}
}}"#,
            arc("A", "combinational", 1.0)
        ))
        .expect("parse half-characterised fixture")
    }

    /// A two-output latch whose second output carries a non-reset timing group with
    /// no characterisation table at all -- `related_pin` and `timing_type` and
    /// nothing else.
    ///
    /// This is the shape that escapes notice most easily. Such an arc is refused by
    /// `extract_timing_tables_from_arc`, so `QN` never reaches `source_order`, and a skip
    /// decision keyed on that map would not consider `QN` at all: it would silently take
    /// no clock arc, with nothing on standard error and nothing in the report.
    fn tableless_output_lib() -> Liberty {
        liberty_parser::parse_lib(&format!(
            r#"
library(tableless_test) {{
  lu_table_template(T) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}
  cell(TABLELESS) {{
    latch(IQ, IQN) {{ enable: "G"; data_in: "A"; }}
    pin(G) {{ direction: input; clock: true; }}
    pin(A) {{ direction: input; }}
    pin(Q) {{
      direction: output;
      function: "IQ";
      {}
    }}
    pin(QN) {{
      direction: output;
      function: "IQN";
      timing() {{
        related_pin: "A";
        timing_type: combinational;
      }}
    }}
  }}
}}"#,
            arc("A", "combinational", 1.0)
        ))
        .expect("parse tableless-output fixture")
    }

    /// An input driving both a converted and a skipped output is constrained over
    /// the converted output alone, and the skipped output keeps what it had.
    ///
    /// Derivation, entirely from the model. `T` is 2 slews x 2 loads, and a
    /// reference arc is the middle row and column of a table -- row 1, column 1 for
    /// a 2x2. `arc("A", .., 1.0)` characterises `Q` as
    ///
    ///     cell_rise = [[1, 2], [3, 4]]        cell_fall = [[1.5, 2.5], [3.5, 4.5]]
    ///
    /// so `Q`'s own reference row is `cell_rise = [3, 4]`, `cell_fall = [3.5, 4.5]`.
    /// `QN` supplies no reference -- its arc has no `fall_transition`, one of the
    /// four a complete reference needs -- so the converted set is `{Q}` and the
    /// cell-wide mean is `ref(Q)` itself.
    ///
    /// `A` drives both outputs. Constrained over the converted output alone, its
    /// arc mean is `Q`'s arc unchanged, sampled at the reference column:
    ///
    /// The arc is positive unate, so its `cell_rise` values were measured with the
    /// input rising and land in the input-rise constraint, and its `cell_fall`
    /// values in the input-fall one:
    ///
    ///     setup_input_rise(A) = [2, 4]     - 4   = [-2, 0]
    ///     setup_input_fall(A) = [2.5, 4.5] - 4.5 = [-2, 0]
    ///
    /// and hold is the negation, `[2, 0]`. Were `QN` allowed into the arithmetic,
    /// `A` would be charged a delay measured on an output nothing characterised
    /// against the clock -- a number describing no real path.
    ///
    /// Killed by: the retain restricting the arc maps to converted outputs dropped, AND
    /// `ref_sum` taking `ref_arcs.get(outpin).map_or(select(mean_ref)[col], |r| select(r)[col])`
    /// so that a missing reference falls back to the cell-wide mean. `QN`'s arc then reaches
    /// the arithmetic and `A` is charged a delay measured on `Q`. Observed to redden this
    /// test alone, and through the constraint values it pins rather than through a panic.
    ///
    /// Dropping the retain alone also reddens it, by way of the guard that then fires, but
    /// reddens the latch-mode test with it: that shows only the guard is reachable, not what
    /// the constraint should be.
    #[test]
    fn an_input_driving_a_converted_and_a_skipped_output_is_constrained_over_the_converted_one() {
        let mut lib = half_characterised_lib();
        let produced = process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        let cell = lib[0].get_cell("HALF").expect("HALF");

        // The converted output gets the flop model: its combinational arc is
        // replaced by the one clock arc, carrying its own reference row.
        let q = cell.get_pin("Q").expect("Q");
        assert_eq!(arc_census(q), census(&[("G", "rising_edge", 1)]));
        let clock_arc = arcs_of_type(q, "rising_edge");
        let cell_rise: Vec<&Group> = clock_arc[0].iter_subgroups_of_type("cell_rise").collect();
        assert_eq!(table_values(cell_rise[0]), vec![3.0, 4.0]);

        // The skipped output keeps exactly the arc the input wrote it, and gains no
        // clock arc: an output with nothing being split has no delay to state.
        let qn = cell.get_pin("QN").expect("QN");
        assert_eq!(arc_census(qn), census(&[("A", "combinational", 1)]));

        // `A`'s constraints, over the converted output alone.
        let a = cell.get_pin("A").expect("A");
        assert_eq!(
            a.simple_attribute("nextstate_type").expect("data").expr(),
            "data"
        );
        for (timing_type, expected) in [("setup_rising", -2.0), ("hold_rising", 2.0)] {
            let groups = arcs_of_type(a, timing_type);
            assert_eq!(groups.len(), 1, "A carries one {}", timing_type);
            for table_type in ["rise_constraint", "fall_constraint"] {
                let tables: Vec<&Group> = groups[0].iter_subgroups_of_type(table_type).collect();
                assert_eq!(
                    table_values(tables[0]),
                    vec![expected, 0.0],
                    "{} {}",
                    timing_type,
                    table_type
                );
            }
        }

        // Convertibility is decided per output, so one skipped output does not cost
        // the cell the output that can be converted: the state element is a flop.
        assert_eq!(cell.iter_subgroups_of_type("latch").count(), 0);
        assert_eq!(cell.iter_subgroups_of_type("ff").count(), 1);

        // And the skip reached the report, not only standard error.
        assert_eq!(
            produced.refusals,
            vec![Refusal {
                library: "half_characterised_test".to_owned(),
                cell: "HALF".to_owned(),
                output: Some("QN".to_owned()),
                reason: "no non-reset source supplies a complete reference".to_owned(),
            }]
        );
    }

    /// An output whose only non-reset arc carries no characterisation table is
    /// skipped and recorded, not passed over in silence.
    ///
    /// Deciding this over the cell's output leaves is what makes it visible. Such an
    /// output never enters `source_order`, because the arc is refused before it gets
    /// there, so a decision keyed on that map would not consider this output at all.
    ///
    /// Killed by: the skipped set was built from `source_order` rather than from the
    /// cell's output leaves, so `QN` produced no refusal at all.
    #[test]
    fn an_output_whose_only_arc_carries_no_table_is_skipped_and_recorded() {
        let mut lib = tableless_output_lib();
        let produced = process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        let cell = lib[0].get_cell("TABLELESS").expect("TABLELESS");

        // The first output still converts.
        assert_eq!(
            arc_census(cell.get_pin("Q").expect("Q")),
            census(&[("G", "rising_edge", 1)])
        );

        // The second keeps its one tableless arc and gains no clock arc.
        assert_eq!(
            arc_census(cell.get_pin("QN").expect("QN")),
            census(&[("A", "combinational", 1)])
        );

        // The refusal names it, with the reason distinguishing it from an output
        // that did carry tables but supplied no complete reference.
        assert_eq!(
            produced.refusals,
            vec![Refusal {
                library: "tableless_test".to_owned(),
                cell: "TABLELESS".to_owned(),
                output: Some("QN".to_owned()),
                reason: "no non-reset timing arc carrying a characterisation table".to_owned(),
            }]
        );
    }

    /// Skipped-ness is mode-independent: the same output is skipped under `--latch`,
    /// where what differs is only what a skip means.
    ///
    /// Under `--latch` the original arcs survive by construction, so a converted
    /// output gains the pseudo arc *alongside* them and a skipped output simply
    /// gains nothing. The latch group stays a latch either way.
    ///
    /// Killed by: phase 3's skip was made conditional on `!latch`, so under `--latch`
    /// the skipped output fell through and was handed the cell-wide mean arc --
    /// giving `QN` a clock-to-output delay for a path nothing characterised.
    #[test]
    fn under_latch_mode_a_skipped_output_gains_nothing_and_the_converted_one_gains_one_arc() {
        let mut lib = half_characterised_lib();
        process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                true,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        let cell = lib[0].get_cell("HALF").expect("HALF");

        // Converted: the original arc is preserved and the pseudo arc added to it.
        assert_eq!(
            arc_census(cell.get_pin("Q").expect("Q")),
            census(&[("A", "combinational", 1), ("G", "rising_edge", 1)])
        );

        // Skipped: the original arc alone, exactly as under the default mode.
        assert_eq!(
            arc_census(cell.get_pin("QN").expect("QN")),
            census(&[("A", "combinational", 1)])
        );

        // The latch model keeps the latch group; this is the sign-off library.
        assert_eq!(cell.iter_subgroups_of_type("ff").count(), 0);
        assert_eq!(cell.iter_subgroups_of_type("latch").count(), 1);
    }

    // --- a cell whose outputs sit on different lookup templates -------------

    /// Two candidate cells: the first with its two outputs characterised on two
    /// different declared two-axis templates, the second an ordinary convertible cell.
    ///
    /// A cell like the first is one the conversion cannot describe. Its pseudo template
    /// pair is derived per cell from one template, and averaging references indexed on
    /// different axes would combine quantities that are not comparable. A bare assertion on
    /// that condition would abort the entire run and take every other cell with it, which is
    /// what the second cell here guards against.
    fn mixed_template_lib() -> Liberty {
        liberty_parser::parse_lib(
            r#"
library(mixed_template_test) {
  lu_table_template(TA) {
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }
  lu_table_template(TB) {
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.02, 0.2");
    index_2("0.006, 0.06");
  }
  cell(MIXED) {
    latch(IQ, IQN) { enable: "G"; data_in: "A"; }
    pin(G) { direction: input; clock: true; }
    pin(A) { direction: input; }
    pin(Q) {
      direction: output;
      function: "IQ";
      timing() {
        related_pin: "A";
        timing_sense : positive_unate;
        timing_type: combinational;
        cell_rise(TA) { values("1.0, 2.0", "3.0, 4.0"); }
        cell_fall(TA) { values("1.5, 2.5", "3.5, 4.5"); }
        rise_transition(TA) { values("0.1, 0.2", "0.3, 0.4"); }
        fall_transition(TA) { values("0.11, 0.21", "0.31, 0.41"); }
      }
    }
    pin(QN) {
      direction: output;
      function: "IQN";
      timing() {
        related_pin: "A";
        timing_sense : positive_unate;
        timing_type: combinational;
        cell_rise(TB) { values("2.0, 3.0", "4.0, 5.0"); }
        cell_fall(TB) { values("2.5, 3.5", "4.5, 5.5"); }
        rise_transition(TB) { values("0.2, 0.3", "0.4, 0.5"); }
        fall_transition(TB) { values("0.21, 0.31", "0.41, 0.51"); }
      }
    }
  }
  cell(PLAIN) {
    latch(IQ, IQN) { enable: "G"; data_in: "A"; }
    pin(G) { direction: input; clock: true; }
    pin(A) { direction: input; }
    pin(Q) {
      direction: output;
      function: "IQ";
      timing() {
        related_pin: "A";
        timing_sense : positive_unate;
        timing_type: combinational;
        cell_rise(TA) { values("1.0, 2.0", "3.0, 4.0"); }
        cell_fall(TA) { values("1.5, 2.5", "3.5, 4.5"); }
        rise_transition(TA) { values("0.1, 0.2", "0.3, 0.4"); }
        fall_transition(TA) { values("0.11, 0.21", "0.31, 0.41"); }
      }
    }
  }
}
"#,
        )
        .expect("parse mixed-template fixture")
    }

    /// A cell whose outputs draw references from different templates is flagged and
    /// skipped, and every other cell in the library still converts.
    ///
    /// The run continues: one cell the conversion cannot describe is not a reason to
    /// discard the rest, and a Liberty file holds many cells in its single library
    /// block. The check is made before anything is mutated, so the cell is emitted
    /// exactly as it arrived -- whole-cell equality carries that, because the claim is
    /// about everything the engine did not do to it.
    ///
    /// Killed by: the template-agreement check moved below phase 3, so `MIXED`'s
    /// convertible output was given its clock arc before the cell was abandoned and the
    /// whole-cell comparison no longer matched.
    #[test]
    fn a_cell_whose_outputs_sit_on_different_templates_is_skipped_and_the_others_convert() {
        let original = mixed_template_lib();
        let mut lib = mixed_template_lib();
        let produced = process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );

        // Verbatim, and never mutated on the way to being abandoned.
        let mixed = lib[0].get_cell("MIXED").expect("MIXED");
        assert_eq!(
            format!("{:?}", original[0].get_cell("MIXED").expect("MIXED")),
            format!("{:?}", mixed),
            "a cell the conversion cannot describe comes through verbatim"
        );
        assert_eq!(mixed.iter_subgroups_of_type("latch").count(), 1);
        assert_eq!(mixed.iter_subgroups_of_type("ff").count(), 0);

        // The part an abort would destroy: the rest of the library.
        let plain = lib[0].get_cell("PLAIN").expect("PLAIN");
        assert_eq!(plain.iter_subgroups_of_type("ff").count(), 1);
        assert_eq!(
            arc_census(plain.get_pin("Q").expect("Q")),
            census(&[("G", "rising_edge", 1)])
        );

        // Recorded at cell scope: no output named, because the whole cell was left.
        assert_eq!(
            produced.refusals,
            vec![Refusal {
                library: "mixed_template_test".to_owned(),
                cell: "MIXED".to_owned(),
                output: None,
                reason: "arcs on more than one domain: TA at 2x2, TB at 2x2".to_owned(),
            }]
        );
    }

    /// Two outputs on ONE declared template whose tables carry different dimensions:
    /// `Q` at 2x3 and `QN` at 2x2.
    ///
    /// Differing dimensions are a differing template whatever the name says, so this is the
    /// same refusal as two different names. Note the trap this fixture exists for: a
    /// reference arc's `col` and `row` are HALVED index positions, so 3/2 and 2/2 are both 1
    /// — comparing those instead of the dimensions themselves lets this pair through, and the
    /// run then dies adding arrays of different shapes.
    fn same_name_mixed_dimensions_lib() -> Liberty {
        liberty_parser::parse_lib(
            r#"
library(mixed_dimensions_test) {
  lu_table_template(TA) {
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }
  cell(WIDE) {
    latch(IQ, IQN) { enable: "G"; data_in: "A"; }
    pin(G) { direction: input; clock: true; }
    pin(A) { direction: input; }
    pin(Q) {
      direction: output;
      function: "IQ";
      timing() {
        related_pin: "A";
        timing_sense : positive_unate;
        timing_type: combinational;
        cell_rise(TA) { values("1.0, 2.0, 3.0", "4.0, 5.0, 6.0"); }
        cell_fall(TA) { values("1.5, 2.5, 3.5", "4.5, 5.5, 6.5"); }
        rise_transition(TA) { values("0.1, 0.2, 0.3", "0.4, 0.5, 0.6"); }
        fall_transition(TA) { values("0.11, 0.21, 0.31", "0.41, 0.51, 0.61"); }
      }
    }
    pin(QN) {
      direction: output;
      function: "IQN";
      timing() {
        related_pin: "A";
        timing_sense : positive_unate;
        timing_type: combinational;
        cell_rise(TA) { values("2.0, 3.0", "4.0, 5.0"); }
        cell_fall(TA) { values("2.5, 3.5", "4.5, 5.5"); }
        rise_transition(TA) { values("0.2, 0.3", "0.4, 0.5"); }
        fall_transition(TA) { values("0.21, 0.31", "0.41, 0.51"); }
      }
    }
  }
}
"#,
        )
        .expect("parse mixed-dimensions fixture")
    }

    /// Arcs on one template name but different dimensions refuse the cell, rather than
    /// killing the run.
    ///
    /// Killed by: the domain judged by template name alone —
    /// `domains.iter().map(|(t, _)| t).collect::<BTreeSet<_>>().len() > 1` — so the
    /// dimensions stop counting while the message still reports them. The cell then passes
    /// the check and the run dies, `ShapeError/IncompatibleShape` while adding a 2x3
    /// reference to a 2x2 one, which is the failure this refusal exists to prevent.
    /// Observed to redden this test alone: the other two domain tests disagree on the name
    /// as well, so they are still caught.
    ///
    /// Dropping the dimensions from the domain key itself also reddens this test, but it
    /// reddens the other two with it, because it changes the dimensions the message
    /// prints. That mutation shows only that the message is asserted, so it is not the one
    /// recorded here.
    #[test]
    fn arcs_on_one_template_name_with_different_dimensions_refuse_the_cell() {
        let original = same_name_mixed_dimensions_lib();
        let mut lib = same_name_mixed_dimensions_lib();
        let produced = process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );

        let cell = lib[0].get_cell("WIDE").expect("WIDE");
        assert_eq!(
            format!("{:?}", original[0].get_cell("WIDE").expect("WIDE")),
            format!("{:?}", cell),
            "the cell comes through verbatim rather than the run dying"
        );

        // The message names the dimensions, because the name alone would read as a
        // template disagreeing with itself.
        assert_eq!(
            produced.refusals,
            vec![Refusal {
                library: "mixed_dimensions_test".to_owned(),
                cell: "WIDE".to_owned(),
                output: None,
                reason: "arcs on more than one domain: TA at 2x2, TA at 2x3".to_owned(),
            }]
        );
    }

    /// One timing group whose four families sit on two declared two-axis templates with
    /// different dimensions: the delays on `T2` (2 rows), the transitions on `T3` (3).
    fn mixed_family_lib() -> Liberty {
        liberty_parser::parse_lib(
            r#"
library(mixed_family_test) {
  lu_table_template(T2) {
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }
  lu_table_template(T3) {
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1, 1.0");
    index_2("0.005, 0.05");
  }
  cell(SPLITFAM) {
    latch(IQ, IQN) { enable: "G"; data_in: "A"; }
    pin(G) { direction: input; clock: true; }
    pin(A) { direction: input; }
    pin(Q) {
      direction: output;
      function: "IQ";
      timing() {
        related_pin: "A";
        timing_sense : positive_unate;
        timing_type: combinational;
        cell_rise(T2) { values("1.0, 2.0", "3.0, 4.0"); }
        cell_fall(T2) { values("1.5, 2.5", "3.5, 4.5"); }
        rise_transition(T3) { values("0.1, 0.2", "0.3, 0.4", "0.5, 0.6"); }
        fall_transition(T3) { values("0.11, 0.21", "0.31, 0.41", "0.51, 0.61"); }
      }
    }
  }
}
"#,
        )
        .expect("parse mixed-family fixture")
    }

    /// The four families of one timing group must agree on their domain too, not only
    /// the outputs of a cell with each other.
    ///
    /// This is the case that yields silently wrong numbers rather than a failure if it is
    /// not refused. The axis check reads the template of the first present family alone,
    /// and every table is sliced at a row taken from `cell_rise`, so a transition family
    /// on a taller template would have one of its rows published under a template derived
    /// from the delays': exit 0, no warning, wrong values in the product.
    ///
    /// Killed by: the domains collected per timing GROUP — inserting only the
    /// representative `(lut_template, dimensions)` — instead of per family. The
    /// disagreement inside the group then becomes invisible and the cell converts.
    /// Neither of the other two domain tests reddens under it: in both of those the
    /// representative template already differs between outputs.
    #[test]
    fn one_timing_groups_families_must_agree_on_their_domain() {
        let original = mixed_family_lib();
        let mut lib = mixed_family_lib();
        let produced = process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );

        let cell = lib[0].get_cell("SPLITFAM").expect("SPLITFAM");
        assert_eq!(
            format!("{:?}", original[0].get_cell("SPLITFAM").expect("SPLITFAM")),
            format!("{:?}", cell),
            "the cell comes through verbatim rather than emitting a mixed arc"
        );
        // Specifically: no clock arc was published carrying a transition row taken from
        // the taller template.
        assert!(arcs_of_type(cell.get_pin("Q").expect("Q"), "rising_edge").is_empty());

        assert_eq!(
            produced.refusals,
            vec![Refusal {
                library: "mixed_family_test".to_owned(),
                cell: "SPLITFAM".to_owned(),
                output: None,
                reason: "arcs on more than one domain: T2 at 2x2, T3 at 3x2".to_owned(),
            }]
        );
    }

    // --- timing_sense: which constraint family an arc lands in ---------------

    /// The `timing_sense` line an arc declares, or nothing at all where the
    /// library omitted it.
    fn sense_line(sense: Option<&str>) -> String {
        match sense {
            Some(s) => format!("timing_sense : {};", s),
            None => String::new(),
        }
    }

    /// A complete arc -- all four families, so it can supply a reference -- with
    /// the delay tables the caller names.
    fn full_sense_arc(related_pin: &str, sense: Option<&str>, rise: &str, fall: &str) -> String {
        format!(
            r#"
      timing() {{
        related_pin: "{}";
        {}
        timing_type: combinational;
        cell_rise(T) {{ values({}); }}
        cell_fall(T) {{ values({}); }}
        rise_transition(T) {{ values("0.1, 0.2", "0.3, 0.4"); }}
        fall_transition(T) {{ values("0.11, 0.21", "0.31, 0.41"); }}
      }}"#,
            related_pin,
            sense_line(sense),
            rise,
            fall
        )
    }

    /// A complete arc of the given sense, characterised under a `when`.
    fn full_sense_arc_when(
        related_pin: &str,
        sense: &str,
        when: Option<&str>,
        rise: &str,
        fall: &str,
    ) -> String {
        let condition = match when {
            Some(w) => format!("when : \"{}\";", w),
            None => String::new(),
        };
        full_sense_arc(related_pin, Some(sense), rise, fall).replace(
            "timing_type: combinational;",
            &format!("{}\n        timing_type: combinational;", condition),
        )
    }

    /// An arc carrying one delay family alone. It supplies no reference, so a
    /// cell built from these needs another source that does.
    fn one_family_arc(related_pin: &str, sense: &str, family: &str, values: &str) -> String {
        format!(
            r#"
      timing() {{
        related_pin: "{}";
        timing_sense : {};
        timing_type: combinational;
        {}(T) {{ values({}); }}
      }}"#,
            related_pin, sense, family, values
        )
    }

    /// A candidate latch on one shared 2x2 template, whose output Q carries
    /// exactly the timing groups the caller supplies.
    fn sense_lib(inputs: &[&str], q_arcs: &str) -> Liberty {
        let pins: String = inputs
            .iter()
            .map(|p| format!("    pin({}) {{ direction: input; }}\n", p))
            .collect();
        liberty_parser::parse_lib(&format!(
            r#"
library(sense_test) {{
  lu_table_template(T) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}
  cell(SENSE) {{
    latch(IQ, IQN) {{ enable: "G"; data_in: "A"; }}
    pin(G) {{ direction: input; clock: true; }}
{}
    pin(Q) {{
      direction: output;
      function: "IQ";
      {}
    }}
  }}
}}"#,
            pins, q_arcs
        ))
        .expect("parse sense fixture")
    }

    /// The values of one constraint table of a pin's single group of that type,
    /// or `None` where that table was not emitted at all.
    fn constraint_values(pin: &Group, timing_type: &str, table_type: &str) -> Option<Vec<f64>> {
        let groups = arcs_of_type(pin, timing_type);
        assert_eq!(
            groups.len(),
            1,
            "pin {} carries one {}",
            pin.name,
            timing_type
        );
        groups[0]
            .iter_subgroups_of_type(table_type)
            .next()
            .map(table_values)
    }

    /// The two constraints of one arc, under a stated `timing_sense`.
    ///
    /// Derivation, entirely from the model. `T` is 2 slews x 2 loads, so a middle
    /// anchor reads row 1 and column 1. With
    ///
    ///     cell_rise = [[1, 2], [3, 4]]        cell_fall = [[10, 20], [30, 40]]
    ///
    /// the reference for Q is `rise.delay = [3, 4]` crossing `4`, and
    /// `fall.delay = [30, 40]` crossing `40`. A constraint is the arc's column 1
    /// minus the crossing of the reference half its own FAMILY names -- a
    /// `cell_rise` table is a delay of an output rise whatever made the input
    /// move -- so
    ///
    ///     from cell_rise: [2, 4]  - 4  = [-2, 0]
    ///     from cell_fall: [20, 40] - 40 = [-20, 0]
    ///
    /// The two are deliberately far apart, so which of them reaches
    /// `rise_constraint` is visible.
    fn sense_constraints(sense: &str) -> (Option<Vec<f64>>, Option<Vec<f64>>) {
        let mut lib = sense_lib(
            &["A"],
            &full_sense_arc(
                "A",
                Some(sense),
                r#""1.0, 2.0", "3.0, 4.0""#,
                r#""10.0, 20.0", "30.0, 40.0""#,
            ),
        );
        process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let a = cell.get_pin("A").expect("A");
        (
            constraint_values(a, "setup_rising", "rise_constraint"),
            constraint_values(a, "setup_rising", "fall_constraint"),
        )
    }

    /// Under `positive_unate` an incoming rise makes a local rise (RM p.329), so
    /// the `cell_rise` values were measured with the input rising and belong in
    /// `rise_constraint`, which is keyed on the constrained pin's own transition.
    ///
    /// Killed by: phase 1 routed a `positive_unate` arc to the opposite edge, leaving every other sense alone. Observed to redden this test and the two below that also carry a positive arc -- and to leave the negative test beside it green, which is the pair that separates the two directions: the mutation recorded there does exactly the reverse.
    #[test]
    fn a_positive_unate_arc_constrains_the_input_edge_its_output_family_names() {
        let (rise, fall) = sense_constraints("positive_unate");
        assert_eq!(rise, Some(vec![-2.0, 0.0]));
        assert_eq!(fall, Some(vec![-20.0, 0.0]));
    }

    /// Under `negative_unate` an incoming rise makes a local FALL (RM p.329), so
    /// the `cell_fall` values are the ones measured with the input rising, and it
    /// is those that belong in `rise_constraint`. The pair is exactly the
    /// positive-unate pair swapped -- nothing else about the arc changed.
    ///
    /// Killed by: phase 1 routed a `negative_unate` arc as if it were positive, leaving every other sense alone. Observed to redden this test and the two below that also carry a negative arc -- and to leave the positive test above green, which together with the mutation recorded there is what separates the two directions.
    #[test]
    fn a_negative_unate_arc_lands_in_the_opposite_constraint_family() {
        let (rise, fall) = sense_constraints("negative_unate");
        assert_eq!(rise, Some(vec![-20.0, 0.0]));
        assert_eq!(fall, Some(vec![-2.0, 0.0]));
    }

    /// An arc whose input direction cannot be derived enters nothing at all, and
    /// its output is refused with the reason that says so.
    ///
    /// `non_unate` and a missing attribute are treated identically: neither
    /// determines which constraint family the values belong in, and no Liberty
    /// default for `timing_sense` is stated to fall back on. The skip is
    /// mode-independent, because constraint placement needs the input's direction
    /// under every reference mode.
    ///
    /// Killed by: phase 1's sense match gained a `None => TimingSense::Positive` arm, so a missing `timing_sense` was treated as positive instead of skipped and `QN` gained a clock arc instead of a refusal. Observed to redden this test alone, through the missing-sense case; the `non_unate` case is what still covers the arm the mutation leaves standing.
    #[test]
    fn an_arc_with_no_derivable_input_direction_is_skipped_and_enters_nothing() {
        let reset_name = Regex::new("(R|S)N?").unwrap();
        for sense in [Some("non_unate"), None] {
            let mut lib = liberty_parser::parse_lib(&format!(
                r#"
library(sense_test) {{
  lu_table_template(T) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}
  cell(SENSE) {{
    latch(IQ, IQN) {{ enable: "G"; data_in: "A"; }}
    pin(G) {{ direction: input; clock: true; }}
    pin(A) {{ direction: input; }}
    pin(Q) {{
      direction: output;
      function: "IQ";
      {}
    }}
    pin(QN) {{
      direction: output;
      function: "IQN";
      {}
    }}
  }}
}}"#,
                full_sense_arc(
                    "A",
                    Some("positive_unate"),
                    r#""1.0, 2.0", "3.0, 4.0""#,
                    r#""10.0, 20.0", "30.0, 40.0""#
                ),
                full_sense_arc(
                    "A",
                    sense,
                    r#""5.0, 6.0", "7.0, 8.0""#,
                    r#""50.0, 60.0", "70.0, 80.0""#
                ),
            ))
            .expect("parse underivable-sense fixture");

            let produced = process_library(
                &mut lib[0],
                &opts(
                    "G",
                    &reset_name,
                    false,
                    ReferenceMode::PerOutput,
                    WhenMerge::Mean,
                ),
            );

            assert_eq!(
                produced.refusals,
                vec![Refusal {
                    library: "sense_test".to_owned(),
                    cell: "SENSE".to_owned(),
                    output: Some("QN".to_owned()),
                    reason: "every non-reset arc was skipped: no derivable input direction"
                        .to_owned(),
                }],
                "sense {:?}",
                sense
            );

            // The output keeps exactly the arc the input wrote it and gains no
            // clock arc, as for any other output-scope skip.
            let cell = lib[0].get_cell("SENSE").expect("SENSE");
            assert_eq!(
                arc_census(cell.get_pin("QN").expect("QN")),
                census(&[("A", "combinational", 1)]),
                "sense {:?}",
                sense
            );

            // And the arc reached neither the report's raw arcs nor the tables the
            // constraints were built from: skipped means it entered nothing.
            let report = produced
                .cells
                .iter()
                .find(|r| r.cell == "SENSE")
                .expect("a report for SENSE");
            assert!(
                report.raw_arcs.iter().all(|a| a.output != "QN"),
                "sense {:?}: {:?}",
                sense,
                report.raw_arcs
            );
            assert!(
                report.constraint_arcs.keys().all(|key| key.outpin != "QN"),
                "sense {:?}",
                sense
            );
        }
    }

    /// Two arcs on one (source, output) pair differing only in `timing_sense` --
    /// the shape an XOR is characterised in -- are routed to opposite input edges
    /// rather than blended together.
    ///
    /// Derivation from the model. `B` supplies Q's reference: with
    /// `cell_rise = [[1,2],[3,4]]` and `cell_fall = [[10,20],[30,40]]` its middle
    /// row and column give `rise.crossing = 4`, `fall.crossing = 40`. `A` carries
    /// two `cell_rise`-only arcs, `[[5,6],[7,8]]` positive and
    /// `[[50,60],[70,80]]` negative. A `cell_rise` table is charged against the
    /// output-rise reference either way, so both subtract 4, but the positive one
    /// describes an input that rose and the negative one an input that fell:
    ///
    ///     input rise: column 1 of [[5,6],[7,8]]     = [6, 8]   - 4 = [2, 4]
    ///     input fall: column 1 of [[50,60],[70,80]] = [60, 80] - 4 = [56, 76]
    ///
    /// Blending them in one accumulator would instead give the mean of the two
    /// tables, `[33, 44] - 4 = [29, 40]`, in the rise family alone and nothing at
    /// all in the fall family -- two opposite input directions averaged together.
    ///
    /// Killed by: the constraint key carried the OUTPUT's edge in place of the input's, so both of `A`'s arcs landed in one accumulator and `rise_constraint` came out as the blend `[29, 40]` with no `fall_constraint` at all. Observed to redden this test and the three other fixtures whose constraint values come off a negative arc -- 146 passed, 4 failed: `a_negative_unate_arc_lands_in_the_opposite_constraint_family` above, `a_pooled_group_of_mixed_families_is_referenced_entry_by_entry` below, and the per-state `a_negative_unate_arcs_per_state_values_land_in_the_flipped_family`; the positive test stays green, because with one sense the two keys coincide -- which is exactly the point this test makes and that one cannot.
    #[test]
    fn two_senses_on_one_pin_pair_are_routed_to_opposite_input_edges() {
        let mut lib = sense_lib(
            &["A", "B"],
            &format!(
                "{}\n{}\n{}",
                full_sense_arc(
                    "B",
                    Some("positive_unate"),
                    r#""1.0, 2.0", "3.0, 4.0""#,
                    r#""10.0, 20.0", "30.0, 40.0""#
                ),
                one_family_arc(
                    "A",
                    "positive_unate",
                    "cell_rise",
                    r#""5.0, 6.0", "7.0, 8.0""#
                ),
                one_family_arc(
                    "A",
                    "negative_unate",
                    "cell_rise",
                    r#""50.0, 60.0", "70.0, 80.0""#
                ),
            ),
        );
        process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );

        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let a = cell.get_pin("A").expect("A");
        assert_eq!(
            constraint_values(a, "setup_rising", "rise_constraint"),
            Some(vec![2.0, 4.0])
        );
        assert_eq!(
            constraint_values(a, "setup_rising", "fall_constraint"),
            Some(vec![56.0, 76.0])
        );
    }

    /// A pooled group whose entries come from two different table families has no
    /// single reference constant, so it takes the per-entry summed path.
    ///
    /// Pooled normally subtracts one constant, and that expression is kept because
    /// `(c + c + ... + c) / n` is not bit-identical to `c`. It only holds while
    /// every entry of the group names one family, which takes two senses on one
    /// pin pair to break -- a shape no corpus produces, so this fixture invents it.
    ///
    /// Derivation from the model. `B` gives Q the reference `rise.crossing = 4`,
    /// `fall.crossing = 40`, and with one output the cell-wide mean IS that
    /// reference. `A`'s two arcs both describe an input RISE: the positive
    /// `cell_rise` directly, the negative `cell_fall` by inversion. So the
    /// input-rise group holds two entries of different families:
    ///
    ///     mean arc  = ([[100,200],[300,400]] + [[1000,2000],[3000,4000]]) / 2
    ///               = [[550, 1100], [1650, 2200]]
    ///     column 1  = [1100, 2200]
    ///     reference = (4 + 40) / 2 = 22
    ///     setup(A)  = [1100 - 22, 2200 - 22] = [1078, 2178]
    ///
    /// Charging one family's constant to the whole group would give 40 or 4
    /// throughout -- [1060, 2160] or [1096, 2196] -- neither of which describes
    /// both halves of what was averaged.
    ///
    /// Killed by: the `families.len() == 1` guard dropped, so `Pooled` always took the single-constant expression and charged the whole group `cell_fall`'s crossing, giving [1060, 2160]. Observed to redden this test alone -- every other pooled group in the suite has one family, where the two paths agree.
    #[test]
    fn a_pooled_group_of_mixed_families_is_referenced_entry_by_entry() {
        let mut lib = sense_lib(
            &["A", "B"],
            &format!(
                "{}\n{}\n{}",
                full_sense_arc(
                    "B",
                    Some("positive_unate"),
                    r#""1.0, 2.0", "3.0, 4.0""#,
                    r#""10.0, 20.0", "30.0, 40.0""#
                ),
                one_family_arc(
                    "A",
                    "positive_unate",
                    "cell_rise",
                    r#""100.0, 200.0", "300.0, 400.0""#
                ),
                one_family_arc(
                    "A",
                    "negative_unate",
                    "cell_fall",
                    r#""1000.0, 2000.0", "3000.0, 4000.0""#
                ),
            ),
        );
        process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::Pooled,
                WhenMerge::Mean,
            ),
        );

        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let a = cell.get_pin("A").expect("A");
        assert_eq!(
            constraint_values(a, "setup_rising", "rise_constraint"),
            Some(vec![1078.0, 2178.0])
        );
        // Nothing described an input fall, so no table is emitted for one.
        assert_eq!(
            constraint_values(a, "setup_rising", "fall_constraint"),
            None
        );
    }

    // --- post-settled state classification ----------------------------------

    /// The classes of one cell whose single output is characterised by the arcs
    /// supplied, run through the whole engine.
    fn classes_of(q_arcs: &str) -> Vec<StateClass> {
        let mut lib = sense_lib(&["A", "B"], q_arcs);
        let produced = process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        produced
            .cells
            .iter()
            .find(|r| r.cell == "SENSE")
            .expect("a report for SENSE")
            .classes
            .clone()
    }

    /// The literal in a post-settled condition follows the INPUT's transition, not
    /// the output family the values were read from.
    ///
    /// Derivation from the model. The arc is `negative_unate` and conditioned on
    /// `B`. Its `cell_rise` values describe the output rising, which under that
    /// sense is what an input FALL produces (RM p.329), so the state they describe
    /// is `B` with `A` settled low: `B * !A`. Its `cell_fall` values describe the
    /// output falling, which took an input rise: `B * A`. A construction keyed on
    /// the output's direction instead would give the two the other way round.
    ///
    /// Killed by: the literal was built as `input_edge == Transition::Fall`, which swapped the two conditions.
    #[test]
    fn a_negative_unate_arcs_post_settled_literal_follows_its_input() {
        let classes = classes_of(&full_sense_arc_when(
            "A",
            "negative_unate",
            Some("B"),
            r#""1.0, 2.0", "3.0, 4.0""#,
            r#""10.0, 20.0", "30.0, 40.0""#,
        ));

        let condition = |edge: Transition| {
            classes
                .iter()
                .find(|c| c.edge == edge && c.output == "Q")
                .unwrap_or_else(|| panic!("no {:?} class: {:?}", edge, classes))
                .condition
                .clone()
        };
        assert_eq!(condition(Transition::Rise), Some("B * !A".to_owned()));
        assert_eq!(condition(Transition::Fall), Some("B * A".to_owned()));
    }

    /// An arc stating no `when` is the catch-all for its output and edge: it covers
    /// whatever the conditioned arcs do not, so it is recorded with no condition
    /// rather than run through the construction.
    ///
    /// Running it through would be worse than useless: a whenless rise arc on `A`
    /// would come out conditioned on `A`, a claim the library never made.
    ///
    /// Killed by: the whenless arm built `ArcPost::Settled` from `Condition::literal` alone, so the catch-all row came out conditioned on `A`.
    #[test]
    fn a_whenless_arc_is_the_catch_all_and_states_no_condition() {
        let classes = classes_of(&full_sense_arc(
            "A",
            Some("positive_unate"),
            r#""1.0, 2.0", "3.0, 4.0""#,
            r#""10.0, 20.0", "30.0, 40.0""#,
        ));

        assert_eq!(classes.len(), 2, "one row per output edge: {:?}", classes);
        for class in &classes {
            assert_eq!(class.condition, None, "{:?}", class);
            assert_eq!(class.members, vec![("A".to_owned(), None)], "{:?}", class);
        }
    }

    /// A `when` this tool cannot read leaves the arc classless -- and converts it
    /// anyway.
    ///
    /// Liberty's postfix complement is outside the parse surface, so `A'` is a
    /// condition the tool must refuse to interpret rather than misread. That
    /// refusal is confined to the classification: a mode that draws one reference
    /// per output never consults a condition, so the arc's tables are split exactly
    /// as any other arc's would be. Contrast the sense skip, which is mode-uniform
    /// because constraint placement needs the input's direction everywhere.
    ///
    /// Killed by: the unreadable arm fell back to `ArcPost::CatchAll`, so the arc took a catch-all row instead of staying classless.
    #[test]
    fn an_unreadable_when_leaves_the_arc_classless_and_still_converts() {
        let mut lib = sense_lib(
            &["A", "B"],
            &full_sense_arc_when(
                "A",
                "positive_unate",
                Some("A'"),
                r#""1.0, 2.0", "3.0, 4.0""#,
                r#""10.0, 20.0", "30.0, 40.0""#,
            ),
        );
        let produced = process_library(
            &mut lib[0],
            &opts(
                "G",
                &Regex::new("(R|S)N?").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        let report = produced
            .cells
            .iter()
            .find(|r| r.cell == "SENSE")
            .expect("a report for SENSE");

        // No class at all, and no catch-all row standing in for one.
        assert!(report.classes.is_empty(), "{:?}", report.classes);
        for arc in &report.raw_arcs {
            assert_eq!(arc.class_rise, None);
            assert_eq!(arc.class_fall, None);
        }

        // The conversion itself is untouched: the output still gains its clock arc
        // and the input still gains its constraints.
        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        assert_eq!(
            arc_census(cell.get_pin("Q").expect("Q")),
            census(&[("G", "rising_edge", 1)])
        );
        assert!(constraint_is_populated(
            cell.get_pin("A").expect("A"),
            "setup_rising"
        ));
    }

    /// A class's row is headed by the condition its arc is emitted under, and never
    /// by the condition of the member that opened the row.
    ///
    /// Derivation from the model. `D` is characterised under `A * B` and `E` under
    /// `A * B * C`, both `positive_unate`, so the post-settled conditions are
    /// `A * B * D`, `A * B * !D`, `A * B * C * E` and `A * B * C * !E`. `D` and `E`
    /// are independent pins, so every condition drawn from one shares assignments
    /// with both drawn from the other, and the closure of that makes the four one
    /// class -- one row per output edge. Their union is
    /// `A * B * (D + !D) + A * B * C * (E + !E)`, which is `A * B`: the least
    /// restrictive condition of the class, and the one Liberty UG p.7-49–50 obliges
    /// the single emitted arc to state, since the four cannot be emitted as four
    /// mutually exclusive states.
    ///
    /// So both rows read `A * B`. No member states it: each of the four fixes `D` or
    /// `E`, which `A * B` leaves free, so each is strictly narrower than the state the
    /// row heads. A row headed by its first member would read `A * B * D` on the rise
    /// and `A * B * !D` on the fall -- conditions the class's other arcs hold outside,
    /// and neither of them the condition the emitted arc carries.
    ///
    /// The label is spelled from the cover espresso returns, whose column order is its
    /// own, so the literals are compared as a set.
    ///
    /// Killed by: the row was headed by `condition.liberty()` of the member that opened it, passed into `place` at row-construction time, as it was before the merged label reached the report. Observed to redden this test alone: it is the only fixture whose class has no member equal to its union AND asserts the report's row rather than the emitted group, so it is the only one that can tell the two labels apart in the report.
    #[test]
    fn a_class_row_is_headed_by_the_merged_condition_and_not_by_its_first_member() {
        let mut lib = sense_lib(
            &["D", "E", "A", "B", "C"],
            &format!(
                "{}\n{}",
                full_sense_arc_when(
                    "D",
                    "positive_unate",
                    Some("A * B"),
                    r#""1.0, 2.0", "3.0, 4.0""#,
                    r#""10.0, 20.0", "30.0, 40.0""#,
                ),
                full_sense_arc_when(
                    "E",
                    "positive_unate",
                    Some("A * B * C"),
                    r#""5.0, 6.0", "7.0, 8.0""#,
                    r#""50.0, 60.0", "70.0, 80.0""#,
                ),
            ),
        );
        let produced = process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let classes = &produced
            .cells
            .iter()
            .find(|r| r.cell == "SENSE")
            .expect("a report for SENSE")
            .classes;

        assert_eq!(
            classes.len(),
            2,
            "four colliding conditions are one class over two output edges: {:?}",
            classes
        );
        for class in classes {
            let condition = class
                .condition
                .as_deref()
                .unwrap_or_else(|| panic!("a conditioned row states its condition: {:?}", class));
            assert_eq!(
                condition.split(" * ").collect::<BTreeSet<&str>>(),
                BTreeSet::from(["A", "B"]),
                "{:?}",
                class
            );
        }
    }

    // --- per-state emission -------------------------------------------------

    /// The text of one simple attribute, read structurally: `Value::string` panics on
    /// a value spelled as a bare expression, and `default_timing` is one.
    fn attribute(group: &Group, name: &str) -> Option<String> {
        group.simple_attribute(name).map(|v| match v {
            Value::Expression(text) | Value::String(text) => text.clone(),
            other => panic!("attribute {} is not a text value: {:?}", name, other),
        })
    }

    /// How one emitted group states its condition, as a line a test can compare.
    ///
    /// The panic is part of the assertion: a `when` without its `sdf_cond`, or the
    /// reverse, conditions one consumer of the library and leaves the other
    /// unconditioned, so no test may express such a group at all.
    fn guard_of(group: &Group) -> String {
        match (
            attribute(group, "when"),
            attribute(group, "sdf_cond"),
            attribute(group, "default_timing"),
        ) {
            (Some(when), Some(sdf), None) => format!("when {} | sdf {}", when, sdf),
            (None, None, Some(default)) => format!("default_timing {}", default),
            (None, None, None) => "unguarded".to_owned(),
            halves => panic!("a group states its condition by halves: {:?}", halves),
        }
    }

    /// The types of the tables one timing group carries, in emitted order.
    fn table_types(group: &Group) -> Vec<&str> {
        group.subgroups.iter().map(|g| g.type_.as_str()).collect()
    }

    /// The knobs of a per-state run.
    fn per_state<'a>(clock: &'a str, reset: &'a Regex) -> CellOptions<'a> {
        opts(
            clock,
            reset,
            false,
            ReferenceMode::PerState,
            WhenMerge::Mean,
        )
    }

    /// The two conditioned arcs and one whenless arc the per-state fixtures below
    /// are built from, on a source the caller names.
    ///
    /// The three are an order of magnitude apart in every family, so a table filed
    /// under the wrong state cannot land on the right value by coincidence.
    fn per_state_arcs(source: &str, sense: &str) -> [String; 3] {
        [
            full_sense_arc_when(
                source,
                sense,
                Some("B"),
                r#""1.0, 2.0", "3.0, 4.0""#,
                r#""10.0, 20.0", "30.0, 40.0""#,
            ),
            full_sense_arc_when(
                source,
                sense,
                Some("!B"),
                r#""100.0, 200.0", "300.0, 400.0""#,
                r#""1000.0, 2000.0", "3000.0, 4000.0""#,
            ),
            full_sense_arc(
                source,
                Some(sense),
                r#""5.0, 6.0", "7.0, 8.0""#,
                r#""50.0, 60.0", "70.0, 80.0""#,
            ),
        ]
    }

    /// An output gains one clock arc per post-settled state it was characterised in,
    /// plus one catch-all last, each stating its state both ways and carrying only
    /// the edge that state describes.
    ///
    /// Derivation from the construction. Two conditioned arcs on `A`, both
    /// `positive_unate`, under `B` and `!B`; a third on `B` stating no condition at
    /// all. A positive arc's `cell_rise` describes an input that ROSE, so under `B`
    /// the rise tables leave the cell in `B * A` and the fall tables in `B * !A` --
    /// four states over the two arcs, in the order the library declares them. Each
    /// state was characterised on one edge alone, so each emits that edge's delay and
    /// slew and nothing else. The whenless arc is no state: it covers whatever the
    /// four do not, which is what `default_timing` says, and being complete it emits
    /// all four tables.
    ///
    /// `T` is 2 slews x 2 loads, so a middle anchor reads row 1: `cell_rise` of the
    /// first arc gives `[3, 4]`, of the second `[300, 400]`, and the whenless arc's
    /// gives `[7, 8]`.
    ///
    /// Killed by: phase 3 emitted the catch-all scope as `Guard::Unguarded`, so the whenless arc's group stated neither a condition nor a default and read as a fifth unconditioned arc among four conditioned ones. Observed to redden this test alone -- it is the only fixture whose output carries a whenless arc.
    #[test]
    fn per_state_emits_one_conditioned_clock_arc_per_state_and_a_catch_all_last() {
        let arcs = per_state_arcs("A", "positive_unate");
        let mut lib = sense_lib(
            &["A", "B"],
            &format!(
                "{}\n{}\n{}",
                arcs[0],
                arcs[1],
                arcs[2].replace("\"A\"", "\"B\"")
            ),
        );
        process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let q = cell.get_pin("Q").expect("Q");
        let groups: Vec<&Group> = q.iter_subgroups_of_type("timing").collect();

        let stated: Vec<(String, Vec<&str>)> = groups
            .iter()
            .map(|g| (guard_of(g), table_types(g)))
            .collect();
        assert_eq!(
            stated,
            vec![
                (
                    "when B * A | sdf B == 1'B1 && A == 1'B1".to_owned(),
                    vec!["rise_transition", "cell_rise"]
                ),
                (
                    "when B * !A | sdf B == 1'B1 && A == 1'B0".to_owned(),
                    vec!["fall_transition", "cell_fall"]
                ),
                (
                    "when !B * A | sdf B == 1'B0 && A == 1'B1".to_owned(),
                    vec!["rise_transition", "cell_rise"]
                ),
                (
                    "when !B * !A | sdf B == 1'B0 && A == 1'B0".to_owned(),
                    vec!["fall_transition", "cell_fall"]
                ),
                (
                    "default_timing true".to_owned(),
                    vec![
                        "rise_transition",
                        "fall_transition",
                        "cell_rise",
                        "cell_fall"
                    ]
                ),
            ]
        );

        // Each state carries its own arc's row 1, not another state's.
        assert_eq!(table_values(&groups[0].subgroups[1]), vec![3.0, 4.0]);
        assert_eq!(table_values(&groups[1].subgroups[1]), vec![30.0, 40.0]);
        assert_eq!(table_values(&groups[2].subgroups[1]), vec![300.0, 400.0]);
        assert_eq!(table_values(&groups[3].subgroups[1]), vec![3000.0, 4000.0]);
        assert_eq!(table_values(&groups[4].subgroups[2]), vec![7.0, 8.0]);
        assert_eq!(table_values(&groups[4].subgroups[3]), vec![70.0, 80.0]);
    }

    /// An input pin is checked once per condition the library characterised it under,
    /// stating that condition verbatim, with a catch-all pair last.
    ///
    /// The emitted `when` is the SOURCE condition and nothing else: no literal
    /// conjoined, no edge. Which edge a check constrains is said by the constraint
    /// family the values sit in, and which clock edge by `timing_type`; conjoining
    /// the pin's own direction into the condition would state a check that holds only
    /// after the very transition it constrains.
    ///
    /// A condition characterised in both input directions is ONE group carrying two
    /// constraint families, not two groups: UG p.7-56 asks a constraint group for at
    /// least one lookup table, not for a particular one, and two groups under
    /// equivalent conditions would overlap.
    ///
    /// Killed by: the caller's `None => Guard::CatchAll` arm was written `None => Guard::Unguarded`, so the whenless arcs' pair stated no default and read as a third unconditioned check overlapping the two conditioned ones. Observed to redden this test alone -- it is the only per-state fixture whose constrained pin carries a whenless arc. (Taking the guard from the post-settled condition instead of the source one reddens this and the two per-state check neighbours, because all three read the emitted `when`.)
    #[test]
    fn per_state_checks_state_the_source_condition_verbatim_with_a_catch_all_last() {
        let arcs = per_state_arcs("A", "positive_unate");
        let mut lib = sense_lib(&["A", "B"], &arcs.join("\n"));
        process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let a = cell.get_pin("A").expect("A");

        for timing_type in ["setup_rising", "hold_rising"] {
            let groups = arcs_of_type(a, timing_type);
            let stated: Vec<(String, Vec<&str>)> = groups
                .iter()
                .map(|g| (guard_of(g), table_types(g)))
                .collect();
            assert_eq!(
                stated,
                vec![
                    // The library's own text, and both directions in the one group.
                    (
                        "when B | sdf B == 1'B1".to_owned(),
                        vec!["rise_constraint", "fall_constraint"]
                    ),
                    (
                        "when !B | sdf B == 1'B0".to_owned(),
                        vec!["rise_constraint", "fall_constraint"]
                    ),
                    (
                        "default_timing true".to_owned(),
                        vec!["rise_constraint", "fall_constraint"]
                    ),
                ],
                "{}",
                timing_type
            );
        }
    }

    /// The values a per-state check carries are its own state's arc minus its own
    /// state's crossing.
    ///
    /// Derivation from the model, on the same fixture. `T` is 2 slews x 2 loads, so a
    /// middle anchor reads column 1 for a constraint and the middle element for the
    /// crossing. The state `B * A` was characterised by the first arc's `cell_rise`
    /// alone, so its reference is that table: crossing `4`, and the constraint is that
    /// table's column 1 minus it,
    ///
    ///     [2, 4] - 4 = [-2, 0]
    ///
    /// and the state `B * !A`, characterised by the same arc's `cell_fall`, gives
    /// `[20, 40] - 40 = [-20, 0]`. The second arc is an order of magnitude larger
    /// throughout, so `!B * A` gives `[200, 400] - 400 = [-200, 0]` and `!B * !A`
    /// gives `[2000, 4000] - 4000 = [-2000, 0]`. Charging any state another state's
    /// crossing -- or the mean of the four -- lands on none of these.
    ///
    /// Hold is setup negated, which is the same four rows with their signs turned.
    ///
    /// Killed by: `constraints_from_arcs` charged per-state the cell-wide mean crossing -- its `ReferenceMode::Pooled` arm widened to `Pooled | PerState` -- which gave `[2, 4] - 202` for the first state rather than `[2, 4] - 4`. Observed to redden this test alone: its four states have four different crossings, where the neighbouring per-state fixtures read guards and table families rather than values. (Looking the reference up at `(outpin, Scope::Whole)` reddens six neighbours with it, because under per-state there is no such key and every one of them panics.)
    #[test]
    fn per_state_checks_carry_their_own_states_arc_minus_its_own_crossing() {
        let arcs = per_state_arcs("A", "positive_unate");
        // The two conditioned arcs alone: the whenless one would add a catch-all
        // pair, which is the previous test's subject rather than this one's.
        let mut lib = sense_lib(&["A", "B"], &format!("{}\n{}", arcs[0], arcs[1]));
        process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let a = cell.get_pin("A").expect("A");

        for (timing_type, sign) in [("setup_rising", 1.0), ("hold_rising", -1.0)] {
            let groups = arcs_of_type(a, timing_type);
            assert_eq!(groups.len(), 2, "{}", timing_type);

            let values: Vec<Vec<f64>> = groups
                .iter()
                .flat_map(|g| g.subgroups.iter().map(table_values))
                .collect();
            let want = |v: [f64; 2]| vec![sign * v[0], sign * v[1]];
            assert_eq!(
                values,
                vec![
                    want([-2.0, 0.0]),
                    want([-20.0, 0.0]),
                    want([-200.0, 0.0]),
                    want([-2000.0, 0.0]),
                ],
                "{}",
                timing_type
            );
        }
    }

    /// A negative-unate arc's values land in the opposite constraint family under a
    /// per-state reference too, and its states are conditioned on the input direction
    /// that produced them.
    ///
    /// Derivation from the model. Under `negative_unate` an incoming rise makes a
    /// local FALL (RM p.329), so this arc's `cell_fall` values are the ones measured
    /// with the input rising and belong in `rise_constraint`, and its `cell_rise`
    /// values in `fall_constraint`. The states follow the input the same way: the
    /// rise tables leave the cell in `B * !A` and the fall tables in `B * A`.
    ///
    /// Each state is again referred to its own crossing -- `cell_fall`'s is 40 and
    /// `cell_rise`'s is 4 -- so `rise_constraint` is `[20, 40] - 40 = [-20, 0]` and
    /// `fall_constraint` is `[2, 4] - 4 = [-2, 0]`. That is exactly the
    /// positive-unate pair swapped, and nothing else about the arc changed.
    ///
    /// Killed by: the post-settled literal keyed on `output_edge == Transition::Rise` in place of the input's direction, so each state followed the family that carried the table rather than the pin that moved, and `Q`'s two clock arcs came out `when B * A` then `when B * !A` -- the pair swapped. Observed to redden this test and `a_negative_unate_arcs_post_settled_literal_follows_its_input`, the classifier's own test one layer below -- 148 passed, 2 failed -- and nothing else: under `positive_unate` the two edges agree, so no other fixture can tell the two keyings apart. (Carrying the OUTPUT's edge into the constraint key instead -- `derived_input_edge(arc.sense, output_edge, ..)` replaced by `output_edge` -- was applied too, and observed to kill the values half of this test rather than the states, 146 passed and 4 failed -- this test and the three other fixtures whose constraint values come off a negative arc, which is the mutation recorded on `two_senses_on_one_pin_pair_are_routed_to_opposite_input_edges`.)
    #[test]
    fn a_negative_unate_arcs_per_state_values_land_in_the_flipped_family() {
        let mut lib = sense_lib(
            &["A", "B"],
            &full_sense_arc_when(
                "A",
                "negative_unate",
                Some("B"),
                r#""1.0, 2.0", "3.0, 4.0""#,
                r#""10.0, 20.0", "30.0, 40.0""#,
            ),
        );
        process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let a = cell.get_pin("A").expect("A");
        let groups = arcs_of_type(a, "setup_rising");
        assert_eq!(groups.len(), 1, "one condition, one check");
        assert_eq!(guard_of(groups[0]), "when B | sdf B == 1'B1");
        assert_eq!(
            table_types(groups[0]),
            vec!["rise_constraint", "fall_constraint"]
        );
        // The fall family reaches the rise constraint, and the rise family the fall.
        assert_eq!(table_values(&groups[0].subgroups[0]), vec![-20.0, 0.0]);
        assert_eq!(table_values(&groups[0].subgroups[1]), vec![-2.0, 0.0]);

        // And the states carry the input's direction, not the output family's.
        let q = cell.get_pin("Q").expect("Q");
        let stated: Vec<String> = q.iter_subgroups_of_type("timing").map(guard_of).collect();
        assert_eq!(
            stated,
            vec![
                "when B * !A | sdf B == 1'B1 && A == 1'B0".to_owned(),
                "when B * A | sdf B == 1'B1 && A == 1'B1".to_owned(),
            ]
        );
    }

    /// Two spellings of one condition are one check group, not two overlapping ones.
    ///
    /// `B * C` and `C * B` are different token streams and the same Boolean function.
    /// Emitting a group under each would state two checks that hold at the same time,
    /// which Liberty's conditioned groups may not; grouping on the function rather
    /// than on the text is what makes that impossible. The surviving spelling is the
    /// first the library wrote, so the emitted text is one the source actually used.
    ///
    /// Killed by: `collision_classes` compared `condition.liberty()` strings instead of BDD handles, which split the one condition into two check groups spelled `B * C` and `C * B` and gave the output four clock arcs instead of two. That also reddens `collision_classes_intern_by_function_in_first_appearance_order`, which asks the same question of the classifier alone; this test is what shows the answer reaches the emitted library.
    #[test]
    fn two_spellings_of_one_condition_are_one_check_group() {
        let mut lib = sense_lib(
            &["A", "B", "C"],
            &format!(
                "{}\n{}",
                full_sense_arc_when(
                    "A",
                    "positive_unate",
                    Some("B * C"),
                    r#""1.0, 2.0", "3.0, 4.0""#,
                    r#""10.0, 20.0", "30.0, 40.0""#,
                ),
                full_sense_arc_when(
                    "A",
                    "positive_unate",
                    Some("C * B"),
                    r#""5.0, 6.0", "7.0, 8.0""#,
                    r#""50.0, 60.0", "70.0, 80.0""#,
                ),
            ),
        );
        process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let a = cell.get_pin("A").expect("A");
        let stated: Vec<String> = arcs_of_type(a, "setup_rising")
            .into_iter()
            .map(guard_of)
            .collect();
        assert_eq!(
            stated,
            vec!["when B * C | sdf B == 1'B1 && C == 1'B1".to_owned()]
        );

        // The two arcs describe one state per edge, so the output carries two clock
        // arcs and not four.
        let q = cell.get_pin("Q").expect("Q");
        assert_eq!(q.iter_subgroups_of_type("timing").count(), 2);
    }

    /// A condition covering another is one state with it, merged at computation
    /// time: one clock arc per edge, its delay the merge of both arcs, and one check
    /// group under the covering condition.
    ///
    /// Derivation from the model. Both arcs are on `D` and `positive_unate`, under
    /// `A * B` and `A * B * C`. `A * B` holds wherever `A * B * C` does, so the two
    /// cannot be emitted as separate states -- Liberty UG p.7-49–50 requires a pin's
    /// state-dependent conditions to be mutually exclusive. Conjoining the direction
    /// the input settled in gives `A * B * D` and `A * B * C * D` for the rise
    /// tables, which collide, and `A * B * !D` and `A * B * C * !D` for the fall,
    /// which collide with each other and with neither of the first two. So two
    /// states, one per edge, each stated under its covering member.
    ///
    /// The merge is the existing `--when-merge` machinery, reached because both arcs
    /// now key on one scope. `T` is 2 slews x 2 loads, so a middle anchor reads row 1
    /// of the merged delay: `cell_rise` is the mean of `[[1, 2], [3, 4]]` and
    /// `[[5, 6], [7, 8]]`, whose row 1 is `[5, 6]`, and `cell_fall` the mean of the
    /// ten-times tables, whose row 1 is `[50, 60]`.
    ///
    /// The check is charged against that same merged reference, which is the point of
    /// merging at computation time. Its constraint is the merged arc's column 1 minus
    /// the merged arc's crossing: `[4, 6] - 6 = [-2, 0]` rising and
    /// `[40, 60] - 60 = [-20, 0]` falling. A check computed against either arc alone
    /// lands on neither.
    ///
    /// Killed by: `collision_classes` reverted to interning on BDD-handle equality, which split the class in two and emitted four clock arcs carrying two unmerged tables. Observed to redden this test and the two overlap tests below it, which need the same class to exist at all, while `disjoint_conditions_sharing_a_literal_stay_separate_states` stays green -- equality keeps disjoint conditions apart and splits overlapping ones, which is the defect. (Reverting `merged_class_conditions` to the first-appearance representative was applied and observed to leave this test GREEN: the first member of this class IS its covering one, so this fixture pins the merge and not the choice of label. That choice is pinned by the two tests below.)
    #[test]
    fn a_covered_condition_merges_into_the_covering_one_at_computation_time() {
        let mut lib = sense_lib(
            &["D", "A", "B", "C"],
            &format!(
                "{}\n{}",
                full_sense_arc_when(
                    "D",
                    "positive_unate",
                    Some("A * B"),
                    r#""1.0, 2.0", "3.0, 4.0""#,
                    r#""10.0, 20.0", "30.0, 40.0""#,
                ),
                full_sense_arc_when(
                    "D",
                    "positive_unate",
                    Some("A * B * C"),
                    r#""5.0, 6.0", "7.0, 8.0""#,
                    r#""50.0, 60.0", "70.0, 80.0""#,
                ),
            ),
        );
        process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let q = cell.get_pin("Q").expect("Q");
        let groups: Vec<&Group> = q.iter_subgroups_of_type("timing").collect();

        let stated: Vec<(String, Vec<&str>)> = groups
            .iter()
            .map(|g| (guard_of(g), table_types(g)))
            .collect();
        assert_eq!(
            stated,
            vec![
                (
                    "when A * B * D | sdf A == 1'B1 && B == 1'B1 && D == 1'B1".to_owned(),
                    vec!["rise_transition", "cell_rise"]
                ),
                (
                    "when A * B * !D | sdf A == 1'B1 && B == 1'B1 && D == 1'B0".to_owned(),
                    vec!["fall_transition", "cell_fall"]
                ),
            ]
        );
        assert_eq!(table_values(&groups[0].subgroups[1]), vec![5.0, 6.0]);
        assert_eq!(table_values(&groups[1].subgroups[1]), vec![50.0, 60.0]);

        // One check group, under the covering condition the library itself wrote.
        let d = cell.get_pin("D").expect("D");
        let checks = arcs_of_type(d, "setup_rising");
        assert_eq!(checks.len(), 1, "one state, one check group");
        assert_eq!(
            guard_of(checks[0]),
            "when A * B | sdf A == 1'B1 && B == 1'B1"
        );
        assert_eq!(
            table_types(checks[0]),
            vec!["rise_constraint", "fall_constraint"]
        );
        assert_eq!(table_values(&checks[0].subgroups[0]), vec![-2.0, 0.0]);
        assert_eq!(table_values(&checks[0].subgroups[1]), vec![-20.0, 0.0]);
    }

    /// Two arcs from different source pins whose conditions overlap are one state,
    /// stated under the least restrictive condition -- and its reference is one
    /// arc's, not a blend of the two.
    ///
    /// Derivation from the model. `D` is characterised under `A * B` and `E` under
    /// `A * B * C`, the shape of the overlapping pairs the emitted libraries carried.
    /// The four post-settled conditions are `A * B * D`, `A * B * !D`,
    /// `A * B * C * E` and `A * B * C * !E`: `D` and `E` are independent pins, so
    /// every condition drawn from one shares assignments with both drawn from the
    /// other, and the closure of that makes all four one state. Their union is
    /// `A * B * (D + !D) + A * B * C * (E + !E)`, which is `A * B` -- the least
    /// restrictive condition of the class, and a single product term, so the label is
    /// `A * B` with nothing conjoined.
    ///
    /// One state means one clock arc, and it carries both edges because the source
    /// supplying its reference was characterised on both. That source is `D`, the
    /// first the library declares: a reference is drawn from ONE pin's arcs, because
    /// the propagation half belongs to the output while the setup half belongs to the
    /// input, so blending two inputs into it would put an input's quantity in an
    /// output's. Row 1 of `D`'s own tables is `[3, 4]` rising and `[30, 40]` falling;
    /// the blend with `E` would be `[5, 6]` and `[50, 60]`, which is what this rules
    /// out.
    ///
    /// Killed by: `merged_class_conditions` reverted to the first-appearance representative, which stated the merged state as `A * B * D` -- a condition `E`'s arcs hold outside, so the emitted state would have been narrower than the tables filed under it. Observed to redden this test alone: it is the only fixture whose class has no member equal to its union, so it is the only one whose label the representative rule can get wrong.
    #[test]
    fn overlapping_conditions_on_two_pins_are_one_state_with_one_pins_reference() {
        let mut lib = sense_lib(
            &["D", "E", "A", "B", "C"],
            &format!(
                "{}\n{}",
                full_sense_arc_when(
                    "D",
                    "positive_unate",
                    Some("A * B"),
                    r#""1.0, 2.0", "3.0, 4.0""#,
                    r#""10.0, 20.0", "30.0, 40.0""#,
                ),
                full_sense_arc_when(
                    "E",
                    "positive_unate",
                    Some("A * B * C"),
                    r#""5.0, 6.0", "7.0, 8.0""#,
                    r#""50.0, 60.0", "70.0, 80.0""#,
                ),
            ),
        );
        process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let q = cell.get_pin("Q").expect("Q");
        let groups: Vec<&Group> = q.iter_subgroups_of_type("timing").collect();
        assert_eq!(groups.len(), 1, "four colliding conditions are one state");

        // A minimised label is spelled from the cover espresso returns, whose column
        // order is its own, so the literals are compared as a set. `guard_of` is what
        // asserts the `when` and its `sdf_cond` are both there.
        let guard = guard_of(groups[0]);
        let (when, sdf) = guard
            .split_once(" | sdf ")
            .unwrap_or_else(|| panic!("a conditioned group states both halves: {}", guard));
        assert_eq!(
            when.trim_start_matches("when ")
                .split(" * ")
                .collect::<BTreeSet<&str>>(),
            BTreeSet::from(["A", "B"])
        );
        assert_eq!(
            sdf.split(" && ").collect::<BTreeSet<&str>>(),
            BTreeSet::from(["A == 1'B1", "B == 1'B1"])
        );

        // The reference is D's own arc, unblended with E's.
        assert_eq!(
            table_types(groups[0]),
            vec![
                "rise_transition",
                "fall_transition",
                "cell_rise",
                "cell_fall"
            ]
        );
        assert_eq!(table_values(&groups[0].subgroups[2]), vec![3.0, 4.0]);
        assert_eq!(table_values(&groups[0].subgroups[3]), vec![30.0, 40.0]);
    }

    /// One pin's check conditions that overlap without either covering the other are
    /// one group, stated under their union.
    ///
    /// Derivation from the model. Both arcs are on `D`, under `A * B` and `B * C`;
    /// both hold with `A`, `B` and `C` high, so Liberty UG p.7-49–50 forbids emitting
    /// them as two check groups, and neither covers the other, so the group can be
    /// stated under neither member. Their union is `A * B + B * C`, whose prime
    /// implicants are exactly those two products and both are essential -- `A * B` is
    /// the only cover of `A * B * !C` and `B * C` the only cover of `!A * B * C`.
    ///
    /// How espresso spells that function -- flat, or factored as `B * (A + C)` -- is
    /// the crate's to choose, and this layer cannot put a text back to a BDD to ask
    /// which function it denotes without naming an espresso type, which `conditions`
    /// reserves to itself; that the merge yields the union is pinned there, by
    /// equivalence. What is pinned here is what this layer decides: that the group's
    /// condition comes from both arcs rather than from the one that opened the slot.
    /// So both halves must state the literals of both arcs -- `A`, `B` and `C`, and no
    /// fourth -- and both must state an OR, since a minimal cover of the union needs
    /// two cubes and no single product term spells a two-cube function.
    ///
    /// Killed by: `check_groups` took its group's condition from the arc that opened the slot, as it did before the classes began merging, which stated the group under `A * B` alone -- a condition the second arc's values were not confined to. Observed to redden this test alone; every other fixture's class has a member equal to its union, so the arc that opened the slot happens to state it and no other test can tell the two rules apart.
    #[test]
    fn overlapping_check_conditions_on_one_pin_are_grouped_under_their_union() {
        let mut lib = sense_lib(
            &["D", "A", "B", "C"],
            &format!(
                "{}\n{}",
                full_sense_arc_when(
                    "D",
                    "positive_unate",
                    Some("A * B"),
                    r#""1.0, 2.0", "3.0, 4.0""#,
                    r#""10.0, 20.0", "30.0, 40.0""#,
                ),
                full_sense_arc_when(
                    "D",
                    "positive_unate",
                    Some("B * C"),
                    r#""5.0, 6.0", "7.0, 8.0""#,
                    r#""50.0, 60.0", "70.0, 80.0""#,
                ),
            ),
        );
        process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let d = cell.get_pin("D").expect("D");
        let checks = arcs_of_type(d, "setup_rising");
        assert_eq!(
            checks.len(),
            1,
            "two colliding conditions are one check group"
        );

        // A minimised label is spelled from the cover espresso returns, and how it
        // arranges the operators is its own, so each half is compared as the set of
        // literals it states. `guard_of` is what asserts the `when` and its
        // `sdf_cond` are both there.
        let guard = guard_of(checks[0]);
        let (when, sdf) = guard
            .split_once(" | sdf ")
            .unwrap_or_else(|| panic!("a conditioned group states both halves: {}", guard));
        fn literals<'a>(text: &'a str, operators: &[char]) -> BTreeSet<&'a str> {
            text.split(operators)
                .map(str::trim)
                .filter(|literal| !literal.is_empty())
                .collect()
        }
        let when = when.trim_start_matches("when ");
        assert_eq!(
            literals(when, &['*', '+', '(', ')']),
            BTreeSet::from(["A", "B", "C"])
        );
        assert_eq!(
            literals(sdf, &['&', '|', '(', ')']),
            BTreeSet::from(["A == 1'B1", "B == 1'B1", "C == 1'B1"])
        );

        // Two cubes cannot be spelled without one.
        assert!(when.contains('+'), "the union disjoins: {}", when);
        assert!(sdf.contains("||"), "the union disjoins: {}", sdf);

        // The two arcs describe one state per edge, so the output carries two clock
        // arcs and not four.
        let q = cell.get_pin("Q").expect("Q");
        assert_eq!(q.iter_subgroups_of_type("timing").count(), 2);
    }

    /// Conditions that cannot hold at once are still separate states: sharing a
    /// literal is not sharing an assignment.
    ///
    /// Derivation from the model. Both arcs are on `D`, under `A * B` and `A * !B`.
    /// The two agree on `A` and disagree on `B`, so no assignment satisfies both and
    /// nothing may be merged; conjoining the settled direction gives four conditions
    /// that are pairwise exclusive, hence four states. Each was characterised by one
    /// arc's one family, so each emits that family alone, at that arc's own row 1:
    /// `[3, 4]`, `[30, 40]`, `[7, 8]` and `[70, 80]`. A criterion that collided them
    /// would emit one arc of the mean, `[5, 6]`, under a label neither library
    /// condition wrote.
    ///
    /// Killed by: `collision_classes` treated every conjunction as satisfiable -- `is_contradiction()` replaced by `false` -- which collapsed the four states into one. Observed to redden this test and fifteen others: every fixture in the tree whose conditions have to stay apart. No mutation separates them, because over-eager collision is one defect and they are its fixtures; this is the one whose two conditions share a literal, which is the shape a criterion keyed on shared pins rather than on shared assignments would get wrong.
    #[test]
    fn disjoint_conditions_sharing_a_literal_stay_separate_states() {
        let mut lib = sense_lib(
            &["D", "A", "B"],
            &format!(
                "{}\n{}",
                full_sense_arc_when(
                    "D",
                    "positive_unate",
                    Some("A * B"),
                    r#""1.0, 2.0", "3.0, 4.0""#,
                    r#""10.0, 20.0", "30.0, 40.0""#,
                ),
                full_sense_arc_when(
                    "D",
                    "positive_unate",
                    Some("A * !B"),
                    r#""5.0, 6.0", "7.0, 8.0""#,
                    r#""50.0, 60.0", "70.0, 80.0""#,
                ),
            ),
        );
        process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let q = cell.get_pin("Q").expect("Q");
        let groups: Vec<&Group> = q.iter_subgroups_of_type("timing").collect();

        let stated: Vec<String> = groups.iter().map(|g| guard_of(g)).collect();
        assert_eq!(
            stated,
            vec![
                "when A * B * D | sdf A == 1'B1 && B == 1'B1 && D == 1'B1".to_owned(),
                "when A * B * !D | sdf A == 1'B1 && B == 1'B1 && D == 1'B0".to_owned(),
                "when A * !B * D | sdf A == 1'B1 && B == 1'B0 && D == 1'B1".to_owned(),
                "when A * !B * !D | sdf A == 1'B1 && B == 1'B0 && D == 1'B0".to_owned(),
            ]
        );
        assert_eq!(table_values(&groups[0].subgroups[1]), vec![3.0, 4.0]);
        assert_eq!(table_values(&groups[1].subgroups[1]), vec![30.0, 40.0]);
        assert_eq!(table_values(&groups[2].subgroups[1]), vec![7.0, 8.0]);
        assert_eq!(table_values(&groups[3].subgroups[1]), vec![70.0, 80.0]);

        // And two check groups, each under the condition the library wrote.
        let d = cell.get_pin("D").expect("D");
        let checks: Vec<String> = arcs_of_type(d, "setup_rising")
            .into_iter()
            .map(guard_of)
            .collect();
        assert_eq!(
            checks,
            vec![
                "when A * B | sdf A == 1'B1 && B == 1'B1".to_owned(),
                "when A * !B | sdf A == 1'B1 && B == 1'B0".to_owned(),
            ]
        );
    }

    /// The two classifications can split what the other merged: a third pin's
    /// condition bridging one source's disjoint `when`s into a single delay state
    /// leaves that source checked twice all the same.
    ///
    /// Derivation from the model. `D` is characterised under `A * B` and `A * !B`,
    /// which no assignment meets together; `M` under `A`, which every assignment
    /// meeting either of `D`'s meets too. On the pin pair `G -> Q` the post-settled
    /// conditions are `A * B * D`, `A * B * !D`, `A * !B * D` and `A * !B * !D` from
    /// `D`'s two arcs and `A * M`, `A * !M` from `M`'s, each of `D`'s four holding at
    /// once with each of `M`'s two -- neither names the other's settling pin -- so
    /// collision's transitive closure (§6) makes the six ONE state, whose union
    /// `A * M + A * !M + ...` is `A`. The checks are classified over the source `when`s
    /// of one input pin alone, where `M`'s condition is not present at all, so `D`'s
    /// two mutually exclusive `when`s stay two groups.
    ///
    /// The values follow from that one state having one reference. `D` is the first
    /// source declared, so the state's reference is `D`'s own accumulated arc, the mean
    /// of its two conditioned tables: `cell_rise` averages to
    /// `[[50.5, 101], [151.5, 202]]`, and a middle anchor reads row 1 for the emitted
    /// delay and its column 1 for the crossing. Each check carries its OWN condition's
    /// arc at column 1 against that shared crossing,
    ///
    ///     A * B  : [2, 4]     - 202 = [-200, -198]
    ///     A * !B : [200, 400] - 202 = [-2, 198]
    ///
    /// and the same on the ten-times fall tables, whose crossing is 2020:
    /// `[-2000, -1980]` and `[-20, 1980]`. Hold is setup negated. Charging both groups
    /// the merged state's own arc -- the reading the split does not take -- would give
    /// `[101, 202] - 202 = [-101, 0]` twice.
    ///
    /// Killed by: `check_groups` closed each pin's classification over the whole cell's source conditions rather than over that pin's own -- the pin's `conditions` extended with every other pin's before `collision_classes`, and the pin's own ids read back off the front. `M`'s `A` then bridged `D`'s two, giving `D` ONE check group stated `when A` carrying `[-101, 0]` and `[-1010, 0]`, the merged state's mean arc. Observed to redden this test alone -- 147 passed, 1 failed -- because every other per-state fixture whose pin carries more than one condition carries them on the only pin that has any, so no other test has a second pin's condition to bridge with.
    #[test]
    fn a_bridged_delay_state_still_leaves_its_source_two_check_groups() {
        let mut lib = sense_lib(
            &["D", "M", "A", "B"],
            &format!(
                "{}\n{}\n{}",
                full_sense_arc_when(
                    "D",
                    "positive_unate",
                    Some("A * B"),
                    r#""1.0, 2.0", "3.0, 4.0""#,
                    r#""10.0, 20.0", "30.0, 40.0""#,
                ),
                full_sense_arc_when(
                    "D",
                    "positive_unate",
                    Some("A * !B"),
                    r#""100.0, 200.0", "300.0, 400.0""#,
                    r#""1000.0, 2000.0", "3000.0, 4000.0""#,
                ),
                full_sense_arc_when(
                    "M",
                    "positive_unate",
                    Some("A"),
                    r#""5.0, 6.0", "7.0, 8.0""#,
                    r#""50.0, 60.0", "70.0, 80.0""#,
                ),
            ),
        );
        process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let cell = lib[0].get_cell("SENSE").expect("SENSE");

        // The delay side merged: one state, stated under the union of all six
        // post-settled conditions. A single-literal function has one spelling, so
        // this half is compared as text where a minimised union of several cubes
        // could not be.
        let q = cell.get_pin("Q").expect("Q");
        let states: Vec<&Group> = q.iter_subgroups_of_type("timing").collect();
        assert_eq!(states.len(), 1, "the bridged conditions are one state");
        assert_eq!(guard_of(states[0]), "when A | sdf A == 1'B1".to_owned());
        assert_eq!(
            table_types(states[0]),
            vec![
                "rise_transition",
                "fall_transition",
                "cell_rise",
                "cell_fall"
            ]
        );
        assert_eq!(table_values(&states[0].subgroups[2]), vec![151.5, 202.0]);
        assert_eq!(table_values(&states[0].subgroups[3]), vec![1515.0, 2020.0]);

        // The check side did not: two groups, under the two `when`s the library
        // wrote, each carrying its own arc against the one crossing above.
        let d = cell.get_pin("D").expect("D");
        for (timing_type, sign) in [("setup_rising", 1.0), ("hold_rising", -1.0)] {
            let groups = arcs_of_type(d, timing_type);
            assert_eq!(
                groups.iter().map(|g| guard_of(g)).collect::<Vec<String>>(),
                vec![
                    "when A * B | sdf A == 1'B1 && B == 1'B1".to_owned(),
                    "when A * !B | sdf A == 1'B1 && B == 1'B0".to_owned(),
                ],
                "{}",
                timing_type
            );

            let values: Vec<Vec<f64>> = groups
                .iter()
                .flat_map(|g| g.subgroups.iter().map(table_values))
                .collect();
            let want = |v: [f64; 2]| vec![sign * v[0], sign * v[1]];
            assert_eq!(
                values,
                vec![
                    want([-200.0, -198.0]),
                    want([-2000.0, -1980.0]),
                    want([-2.0, 198.0]),
                    want([-20.0, 1980.0]),
                ],
                "{}",
                timing_type
            );
        }
    }

    /// A candidate latch with two outputs on one shared 2x2 template, each carrying
    /// exactly the timing groups the caller supplies.
    fn two_output_lib(inputs: &[&str], q1_arcs: &str, q2_arcs: &str) -> Liberty {
        let pins: String = inputs
            .iter()
            .map(|p| format!("    pin({}) {{ direction: input; }}\n", p))
            .collect();
        liberty_parser::parse_lib(&format!(
            r#"
library(two_output_test) {{
  lu_table_template(T) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}
  cell(TWOOUT) {{
    latch(IQ, IQN) {{ enable: "G"; data_in: "D"; }}
    pin(G) {{ direction: input; clock: true; }}
{}
    pin(Q1) {{
      direction: output;
      function: "IQ1";
      {}
    }}
    pin(Q2) {{
      direction: output;
      function: "IQ2";
      {}
    }}
  }}
}}"#,
            pins, q1_arcs, q2_arcs
        ))
        .expect("parse two-output fixture")
    }

    /// Two outputs whose conditions overlap are not one state: a class is drawn
    /// within one output, so each output's arcs are stated over conditions that
    /// output was actually characterised under.
    ///
    /// Derivation from the model. `D` drives `Q1`, characterised under `A * B`
    /// alone, and `Q2`, characterised under `B * C` alone, both `positive_unate`.
    /// Conjoining the direction the input settled in gives `A * B * D` and
    /// `A * B * !D` on `Q1`, and `B * C * D` and `B * C * !D` on `Q2`. The two rise
    /// conditions can hold at once -- with `A`, `B`, `C` and `D` all high -- and so
    /// can the two fall ones.
    ///
    /// That overlap is not a collision. Liberty UG p.7-49–50 requires mutually
    /// exclusive conditions for the state-dependent timing arcs of one pin pair, and
    /// after the split `G -> Q1` and `G -> Q2` are two different pin pairs: nothing
    /// ever required `Q1`'s conditions to exclude `Q2`'s, and no consumer reads the
    /// two as alternatives to each other. Only the arcs of one output, all of them
    /// `G -> Q` once converted and separated by their `when` alone, have to be
    /// resolved against one another.
    ///
    /// So four states, two per output, each a class of one and therefore stated under
    /// its own condition -- the union of a singleton being that member. Neither of
    /// `Q1`'s states names `C` nor either of `Q2`'s names `A`: a class drawn across
    /// the cell would hold both rise conditions and state both outputs' arcs under
    /// their union `A * B * D + B * C * D`, which holds over `!A * B * C * D`, a state
    /// `Q1` carries no table for and `Q1`'s numbers would then claim.
    ///
    /// Each state carries its own output's numbers, read at row 1 of that output's
    /// own 2x2 tables under a middle anchor: `[3, 4]` and `[30, 40]` for `Q1`,
    /// `[7, 8]` and `[70, 80]` for `Q2`.
    ///
    /// Killed by: `classify_states` collected every output's conditions into one group -- the per-output partition replaced by pushing all of them at index 0 -- which is the cell-wide classification this replaced. Observed to redden this test alone: it is the only fixture with two outputs whose conditions overlap. It stated all four arcs under two unions, `Q1`'s rise arc reading `when B * C * D + A * B * D` and `Q2`'s the same, so each output claimed its own numbers over the other's characterisation.
    #[test]
    fn two_outputs_whose_conditions_overlap_are_not_one_state() {
        let mut lib = two_output_lib(
            &["D", "A", "B", "C"],
            &full_sense_arc_when(
                "D",
                "positive_unate",
                Some("A * B"),
                r#""1.0, 2.0", "3.0, 4.0""#,
                r#""10.0, 20.0", "30.0, 40.0""#,
            ),
            &full_sense_arc_when(
                "D",
                "positive_unate",
                Some("B * C"),
                r#""5.0, 6.0", "7.0, 8.0""#,
                r#""50.0, 60.0", "70.0, 80.0""#,
            ),
        );
        process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let cell = lib[0].get_cell("TWOOUT").expect("TWOOUT");
        for (outpin, states, delays) in [
            (
                "Q1",
                vec![
                    "when A * B * D | sdf A == 1'B1 && B == 1'B1 && D == 1'B1".to_owned(),
                    "when A * B * !D | sdf A == 1'B1 && B == 1'B1 && D == 1'B0".to_owned(),
                ],
                vec![vec![3.0, 4.0], vec![30.0, 40.0]],
            ),
            (
                "Q2",
                vec![
                    "when B * C * D | sdf B == 1'B1 && C == 1'B1 && D == 1'B1".to_owned(),
                    "when B * C * !D | sdf B == 1'B1 && C == 1'B1 && D == 1'B0".to_owned(),
                ],
                vec![vec![7.0, 8.0], vec![70.0, 80.0]],
            ),
        ] {
            let pin = cell.get_pin(outpin).expect(outpin);
            let groups: Vec<&Group> = pin.iter_subgroups_of_type("timing").collect();
            assert_eq!(
                groups.iter().map(|g| guard_of(g)).collect::<Vec<String>>(),
                states,
                "the states {} was emitted in",
                outpin
            );

            // Each state was characterised in one direction, so it emits that
            // direction's transition and delay alone -- the delay second.
            for (group, want) in groups.iter().zip(delays) {
                assert_eq!(group.subgroups.len(), 2, "one edge's two tables");
                assert_eq!(
                    table_values(&group.subgroups[1]),
                    want,
                    "the delay {} carries under {}",
                    outpin,
                    guard_of(group)
                );
            }
        }
    }

    /// A pin driving two outputs under one check condition carries ONE constraint,
    /// averaged over both of them -- however differently their states are referred.
    ///
    /// Derivation from the model. `D` drives `Q1` under `A * B` and `Q2` under
    /// `B * C`, both `positive_unate`. The two `when`s can both hold -- with `A`, `B`
    /// and `C` high -- and a check sits on the pin pair `D -> G`, so Liberty UG
    /// p.7-49–50 forbids stating them as two checks on `D`: they are one group, under
    /// their union. Their post-settled conditions, `A * B * D` on `Q1` and
    /// `B * C * D` on `Q2`, are conditions of two DIFFERENT pin pairs once split, so
    /// they stay two states with two references of their own. One check group and two
    /// references is exactly the shape a single value has to stand in for.
    ///
    /// What that value is follows from what the group states. The check holds over
    /// both paths, so it must be the mean of them -- the same average per-output
    /// takes over every output a pin drives, specialised to the outputs it drives
    /// under this condition. `T` is 2 slews x 2 loads, so a middle anchor reads
    /// column 1 for a constraint and row 1 for a reference, whose crossing is its
    /// column 1. Rising:
    ///
    ///     ([2, 4] + [200, 400]) / 2 - (4 + 400) / 2 = [101, 202] - 202 = [-101, 0]
    ///
    /// and falling, on the ten-times tables of each,
    ///
    ///     ([20, 40] + [2000, 4000]) / 2 - (40 + 4000) / 2 = [-1010, 0].
    ///
    /// The two outputs are two orders of magnitude apart, so no other reading lands
    /// nearby: `Q1`'s own arc gives `[-2, 0]`, `Q2`'s `[-200, 0]`, and the mean arc
    /// charged one output's crossing alone gives `[97, 198]` or `[-299, -198]`.
    ///
    /// Killed by: the whole of this keying reverted -- `CheckGroup` carrying a delay-side scope per input direction again, each written by every member arc in turn, the constraint entries keyed `check: scope`, and the checks read back at those two scopes. That is the grouping this replaced, and under it the group carried whatever the last member wrote: `setup_rising` came out `[-200, 0]`, `Q2`'s arc against `Q2`'s crossing, with `Q1`'s value computed and never emitted. Observed to redden this test alone -- 146 passed, 1 failed -- because it is the only fixture whose pin drives two outputs under conditions that merge into one check, so in every other fixture the two partitions agree and no test can tell the two groupings apart.
    #[test]
    fn one_check_condition_over_two_outputs_carries_their_mean() {
        let mut lib = two_output_lib(
            &["D", "A", "B", "C"],
            &full_sense_arc_when(
                "D",
                "positive_unate",
                Some("A * B"),
                r#""1.0, 2.0", "3.0, 4.0""#,
                r#""10.0, 20.0", "30.0, 40.0""#,
            ),
            &full_sense_arc_when(
                "D",
                "positive_unate",
                Some("B * C"),
                r#""100.0, 200.0", "300.0, 400.0""#,
                r#""1000.0, 2000.0", "3000.0, 4000.0""#,
            ),
        );
        process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let cell = lib[0].get_cell("TWOOUT").expect("TWOOUT");
        let d = cell.get_pin("D").expect("D");

        for (timing_type, sign) in [("setup_rising", 1.0), ("hold_rising", -1.0)] {
            let groups = arcs_of_type(d, timing_type);
            assert_eq!(
                groups.len(),
                1,
                "two outputs under one condition are one {}",
                timing_type
            );

            // Under the merged condition and not under either member: a group stated
            // under one of them alone would name that member's own literal and not
            // the other's. How the union is spelled is settled where it is derived.
            let guard = guard_of(groups[0]);
            for literal in ["A", "C"] {
                assert!(
                    guard.contains(literal),
                    "{} states both members: {}",
                    timing_type,
                    guard
                );
            }

            assert_eq!(
                table_types(groups[0]),
                vec!["rise_constraint", "fall_constraint"],
                "{}",
                timing_type
            );
            let want = |v: f64| vec![sign * v, 0.0];
            assert_eq!(
                table_values(&groups[0].subgroups[0]),
                want(-101.0),
                "{}",
                timing_type
            );
            assert_eq!(
                table_values(&groups[0].subgroups[1]),
                want(-1010.0),
                "{}",
                timing_type
            );
        }
    }

    /// Under a per-state reference an arc whose `when` cannot be read is skipped and
    /// named, and the rest of its output converts.
    ///
    /// There is no scope to file it under: the mode keys everything on the state an
    /// arc leaves the cell in, and an unreadable condition names none. That is a
    /// different answer from the one the other modes give the same arc -- they draw
    /// one reference per output, which no condition bears on -- so the skip is stated
    /// per mode rather than as a property of the condition.
    ///
    /// Killed by: the per-state skip's guard was written `if false && matches!(post, ArcPost::Unreadable)`, so the arc was kept: it reached the raw arcs, was filed under no scope at all, and its output converted with no refusal to say an arc had been dropped. Observed to redden this test alone -- no other fixture offers per-state a `when` it cannot read.
    #[test]
    fn per_state_skips_an_unreadable_when_and_converts_the_rest_of_the_output() {
        let mut lib = sense_lib(
            &["A", "B"],
            &format!(
                "{}\n{}",
                full_sense_arc_when(
                    "A",
                    "positive_unate",
                    Some("A'"),
                    r#""1.0, 2.0", "3.0, 4.0""#,
                    r#""10.0, 20.0", "30.0, 40.0""#,
                ),
                full_sense_arc_when(
                    "A",
                    "positive_unate",
                    Some("B"),
                    r#""5.0, 6.0", "7.0, 8.0""#,
                    r#""50.0, 60.0", "70.0, 80.0""#,
                ),
            ),
        );
        let produced = process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        // Named at output scope, so a run that exits 0 still says the arc was lost.
        assert_eq!(
            produced.refusals,
            vec![Refusal {
                library: "sense_test".to_owned(),
                cell: "SENSE".to_owned(),
                output: Some("Q".to_owned()),
                reason: "an arc was skipped: its `when` could not be read, and a per-state \
                         reference is drawn per condition"
                    .to_owned(),
            }]
        );

        // It entered nothing: not the raw arcs, not the tables the constraints were
        // built from.
        let report = produced
            .cells
            .iter()
            .find(|r| r.cell == "SENSE")
            .expect("a report for SENSE");
        assert_eq!(report.raw_arcs.len(), 1, "{:?}", report.raw_arcs);
        assert_eq!(report.raw_arcs[0].when.as_deref(), Some("B"));

        // And the readable arc's two states still converted.
        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let stated: Vec<String> = cell
            .get_pin("Q")
            .expect("Q")
            .iter_subgroups_of_type("timing")
            .map(guard_of)
            .collect();
        assert_eq!(
            stated,
            vec![
                "when B * A | sdf B == 1'B1 && A == 1'B1".to_owned(),
                "when B * !A | sdf B == 1'B1 && A == 1'B0".to_owned(),
            ]
        );
    }

    /// Under a per-state reference, an output whose `when` could not be read on
    /// two separate arcs carries exactly one refusal for it, not one per skipped
    /// arc: the reason is identical across them, so a second, third, ... copy of it
    /// would tell the reader nothing the first did not.
    ///
    /// Killed by: reverting the refusal loop back to a plain `for outpin_name in
    /// &unreadable_arcs` (dropping the `.unique()`). Observed to redden this test
    /// alone -- `produced.refusals` then carries `Q`'s refusal twice -- while
    /// `per_state_skips_an_unreadable_when_and_converts_the_rest_of_the_output`,
    /// whose output has only one skipped arc, stays green under the same mutation.
    #[test]
    fn per_state_dedups_the_unreadable_when_refusal_across_two_skipped_arcs() {
        let mut lib = sense_lib(
            &["A", "B"],
            &format!(
                "{}\n{}\n{}",
                full_sense_arc_when(
                    "A",
                    "positive_unate",
                    Some("A'"),
                    r#""1.0, 2.0", "3.0, 4.0""#,
                    r#""10.0, 20.0", "30.0, 40.0""#,
                ),
                full_sense_arc_when(
                    "A",
                    "positive_unate",
                    Some("B'"),
                    r#""1.5, 2.5", "3.5, 4.5""#,
                    r#""15.0, 25.0", "35.0, 45.0""#,
                ),
                full_sense_arc_when(
                    "A",
                    "positive_unate",
                    Some("B"),
                    r#""5.0, 6.0", "7.0, 8.0""#,
                    r#""50.0, 60.0", "70.0, 80.0""#,
                ),
            ),
        );
        let produced = process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        // One refusal for Q, not two, despite two arcs having been skipped for it.
        assert_eq!(
            produced.refusals,
            vec![Refusal {
                library: "sense_test".to_owned(),
                cell: "SENSE".to_owned(),
                output: Some("Q".to_owned()),
                reason: "an arc was skipped: its `when` could not be read, and a per-state \
                         reference is drawn per condition"
                    .to_owned(),
            }]
        );

        // And the surviving, readable arc's two states still converted.
        let cell = lib[0].get_cell("SENSE").expect("SENSE");
        let stated: Vec<String> = cell
            .get_pin("Q")
            .expect("Q")
            .iter_subgroups_of_type("timing")
            .map(guard_of)
            .collect();
        assert_eq!(
            stated,
            vec![
                "when B * A | sdf B == 1'B1 && A == 1'B1".to_owned(),
                "when B * !A | sdf B == 1'B1 && A == 1'B0".to_owned(),
            ]
        );
    }

    /// A two-output latch under a per-state reference: `Q`'s only non-reset arc
    /// carries an unreadable `when`, and `QN`'s is fully characterised and
    /// unconditioned, so the cell still converts.
    ///
    /// `Q`'s arc carries all four families -- the shape the output-scope ladder's
    /// fallback wording denies -- so a defect that let the ladder fall through to
    /// that fallback would misdescribe a `when` this tool refused to read as an
    /// output that carried no table at all.
    fn per_state_unreadable_when_is_the_only_arc_lib() -> Liberty {
        liberty_parser::parse_lib(&format!(
            r#"
library(sense_test) {{
  lu_table_template(T) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}
  cell(SENSE) {{
    latch(IQ, IQN) {{ enable: "G"; data_in: "A"; }}
    pin(G) {{ direction: input; clock: true; }}
    pin(A) {{ direction: input; }}
    pin(Q) {{
      direction: output;
      function: "IQ";
      {}
    }}
    pin(QN) {{
      direction: output;
      function: "IQN";
      {}
    }}
  }}
}}"#,
            full_sense_arc_when(
                "A",
                "positive_unate",
                Some("A'"),
                r#""1.0, 2.0", "3.0, 4.0""#,
                r#""10.0, 20.0", "30.0, 40.0""#,
            ),
            full_sense_arc(
                "A",
                Some("positive_unate"),
                r#""1.0, 2.0", "3.0, 4.0""#,
                r#""10.0, 20.0", "30.0, 40.0""#,
            ),
        ))
        .expect("parse per-state unreadable-when-only-arc fixture")
    }

    /// Under a per-state reference, an output whose only non-reset arc carries an
    /// unreadable `when` carries exactly one refusal -- the arc-scope one now
    /// recorded into `arc_skipped` -- and not also the output-scope ladder's
    /// generic fallback, which would say `Q` carried no characterisation table at
    /// all when in fact its one arc carried all four families.
    ///
    /// Killed by: deleting the `arc_skipped.entry(...)` insertion this step adds
    /// alongside the `unreadable_arcs.push` at the per-state skip site. With that
    /// insertion gone, `Q` reaches neither `source_order` nor `arc_skipped`, so the
    /// ladder falls through to the generic wording. Observed to redden this test
    /// alone; `per_state_skips_an_unreadable_when_and_converts_the_rest_of_the_output`
    /// stays green under the same mutation, because there `Q` converts by its other,
    /// readable arc and the ladder never runs for it.
    #[test]
    fn per_state_names_the_unreadable_when_reason_when_it_is_the_outputs_only_arc() {
        let mut lib = per_state_unreadable_when_is_the_only_arc_lib();
        let produced = process_library(
            &mut lib[0],
            &per_state("G", &Regex::new("(R|S)N?").unwrap()),
        );

        let q_refusals: Vec<&Refusal> = produced
            .refusals
            .iter()
            .filter(|r| r.output.as_deref() == Some("Q"))
            .collect();
        assert_eq!(q_refusals.len(), 1, "{:?}", produced.refusals);
        assert!(
            q_refusals[0].reason.contains("`when` could not be read"),
            "{:?}",
            q_refusals[0]
        );
        assert!(
            !q_refusals[0]
                .reason
                .contains("no non-reset timing arc carrying a characterisation table"),
            "{:?}",
            q_refusals[0]
        );
    }

    // --- constraint arithmetic at full characterisation size ----------------

    /// The emitted setup constraint on a full-size 10x10 characterisation is the
    /// arc sampled at the reference column, minus the clock-to-output delay the
    /// pseudo-flop model charges for that path; hold is its negation.
    ///
    /// Killed by: `constraints_from_arcs` added the reference delay to the sampled column instead of subtracting it.
    #[test]
    fn setup_constraints_are_the_arc_minus_the_reference_at_10x10() {
        // Deliberately NOT separable in (slew, load): for a table of the form
        // f(row) + g(col) the reference column cancels out of the subtraction, so
        // every column would yield the same constraint and the choice of reference
        // would be unobservable.
        let value = |row: usize, col: usize| {
            (row as f64 + 1.0) * 0.1 + (col as f64 + 1.0) * 0.01 + (row * col) as f64 * 0.001
        };
        let table = (0..10)
            .map(|row| {
                let cells: Vec<String> = (0..10)
                    .map(|col| format!("{:.3}", value(row, col)))
                    .collect();
                format!("\"{}\"", cells.join(", "))
            })
            .collect::<Vec<_>>()
            .join(", ");

        let mut liberty = liberty_parser::parse_lib(&format!(
            r#"
library(large_table_test) {{
  lu_table_template(large_template) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07, 0.08, 0.09, 0.1");
    index_2("0.001, 0.002, 0.003, 0.004, 0.005, 0.006, 0.007, 0.008, 0.009, 0.01");
  }}
  cell(LARGE_LATCH) {{
    latch(IQ) {{ enable: "CLK"; data_in: "D"; }}
    pin(CLK) {{ direction: input; clock: true; }}
    pin(D) {{ direction: input; }}
    pin(Q) {{
      direction: output;
      function: "IQ";
      timing() {{
        related_pin: "D";
        timing_sense : positive_unate;
        timing_type: combinational;
        cell_rise(large_template) {{ values({0}); }}
        cell_fall(large_template) {{ values({0}); }}
        rise_transition(large_template) {{ values({0}); }}
        fall_transition(large_template) {{ values({0}); }}
      }}
    }}
  }}
}}"#,
            table
        ))
        .expect("parse 10x10 fixture");

        process_library(
            &mut liberty[0],
            &opts(
                "CLK",
                &Regex::new("RST").unwrap(),
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );

        // The reference arc is taken from the middle of a 10x10 table: row 5,
        // column 5. D drives the only output, so it is referenced against its own
        // arc and the constraint is that column minus the reference sample.
        let expected: Vec<f64> = (0..10).map(|row| value(row, 5) - value(5, 5)).collect();

        let d = liberty[0]
            .get_cell("LARGE_LATCH")
            .expect("LARGE_LATCH")
            .get_pin("D")
            .expect("D");

        for (timing_type, sign) in [("setup_rising", 1.0), ("hold_rising", -1.0)] {
            let groups = arcs_of_type(d, timing_type);
            assert_eq!(groups.len(), 1, "D carries one {}", timing_type);

            for table_type in ["rise_constraint", "fall_constraint"] {
                let tables: Vec<&Group> = groups[0].iter_subgroups_of_type(table_type).collect();
                assert_eq!(tables.len(), 1, "one {} in {}", table_type, timing_type);

                let got = table_values(tables[0]);
                assert_eq!(got.len(), expected.len(), "{} {}", timing_type, table_type);
                for (row, (got, want)) in got.iter().zip(expected.iter()).enumerate() {
                    assert!(
                        (got - sign * want).abs() < 1e-9,
                        "{} {} row {}: got {}, expected {}",
                        timing_type,
                        table_type,
                        row,
                        got,
                        sign * want
                    );
                }
            }
        }
    }

    // --- constraint placement across data expression shapes -----------------

    /// Every input the library characterised against the output is given one setup
    /// and one hold group, whatever shape the latch's data expression takes.
    ///
    /// Killed by: `create_setup_timing_group` declared `timing_type: setup_falling`.
    #[test]
    fn every_characterised_input_is_constrained_across_data_in_shapes() {
        let reset_name = Regex::new("(R|S)N?").unwrap();

        for (data_in, inputs, label) in [
            ("A*B+A*IQ+B*IQ", &["A", "B"][..], "basic_rcelem"),
            ("A+B", &["A", "B"][..], "simple_or"),
            ("A*B", &["A", "B"][..], "simple_and"),
            ("A*B*C+A*IQ+B*IQ+C*IQ", &["A", "B", "C"][..], "three_input"),
            ("D", &["D"][..], "single_input"),
        ] {
            let pins: String = inputs
                .iter()
                .map(|pin| format!("pin({}) {{ direction: input; }}", pin))
                .collect::<Vec<_>>()
                .join("\n    ");
            let arcs: String = inputs
                .iter()
                .enumerate()
                .map(|(i, pin)| arc(pin, "combinational", 1.0 + i as f64))
                .collect();

            let mut liberty = liberty_parser::parse_lib(&format!(
                r#"
library({}_test) {{
  lu_table_template(T) {{
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }}
  cell(RCELEM_VARIANT) {{
    latch(IQ,IQN) {{ clear: "!RN"; data_in: "{}"; enable: "G"; }}
    pin(G) {{ direction: input; clock: true; }}
    pin(RN) {{ direction: input; }}
    {}
    pin(Q) {{
      direction: output;
      function: "IQ";
      {}
    }}
  }}
}}"#,
                label, data_in, pins, arcs
            ))
            .unwrap_or_else(|e| panic!("failed to parse {} variation: {}", label, e));

            process_library(
                &mut liberty[0],
                &opts(
                    "G",
                    &reset_name,
                    false,
                    ReferenceMode::PerOutput,
                    WhenMerge::Mean,
                ),
            );

            let cell = liberty[0]
                .get_cell("RCELEM_VARIANT")
                .expect("RCELEM_VARIANT");
            let ffs: Vec<&Group> = cell.iter_subgroups_of_type("ff").collect();
            assert_eq!(ffs.len(), 1, "{} should carry one ff group", label);
            assert_eq!(
                ffs[0].simple_attribute("next_state").unwrap().string(),
                data_in,
                "{} should keep its data expression",
                label
            );

            for name in inputs {
                let pin = cell
                    .get_pin(name)
                    .unwrap_or_else(|| panic!("pin {} missing in {}", name, label));
                assert_eq!(
                    pin.simple_attribute("nextstate_type").unwrap().expr(),
                    "data",
                    "pin {} in {} should be marked as data",
                    name,
                    label
                );
                for timing_type in ["setup_rising", "hold_rising"] {
                    assert_eq!(
                        arcs_of_type(pin, timing_type).len(),
                        1,
                        "pin {} in {} should carry one {}",
                        name,
                        label,
                        timing_type
                    );
                    assert!(
                        constraint_is_populated(pin, timing_type),
                        "pin {} in {}: {} carries no constraint table",
                        name,
                        label,
                        timing_type
                    );
                }
            }
        }
    }

    // --- the real ASCEND libraries -----------------------------------------
    //
    // Semantic assertions against the real libraries, deliberately not byte comparisons
    // against the committed `_pseudoflop.lib` and `_pseudolatch.lib` outputs: a golden
    // file either passes vacuously or fails on every intended change, so those files are
    // documentation and are not read here.
    //
    // The paths go through `CARGO_MANIFEST_DIR` rather than being relative, because a test
    // must not depend on the directory it happens to be run from.

    const ASCEND_FF_1V25: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/ASCEND_FREEPDK45_ALHO_ff_1.25V_0C.lib"
    );

    const ASCEND_NOM_1V10: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/ASCEND_FREEPDK45_ALHO_nom_1.10V_25C.lib"
    );

    /// Clock, reset and both cells under test are the same across every ASCEND
    /// corner, so the whole inventory is stated once.
    const ASCEND_CLOCK: &str = "G";
    const ASCEND_RESET: &str = "(R|S)N?";

    /// Pin Q of RACELEM21X1 as the ASCEND libraries characterise it: 28 arcs from
    /// the five data inputs and 34 owned by the reset. Identical in the nom and ff
    /// corners.
    ///
    /// This and [`lbtiex1_original_q`] describe the INPUT libraries, not the tool.
    /// Each entry is a `timing` group written out in the `.lib` text, so the census
    /// is read off the fixture and is a claim about it -- asserted before the run
    /// so a failure there is a changed fixture rather than a changed conversion.
    fn racelem21x1_original_q() -> ArcCensus {
        census(&[
            ("A", "combinational", 2),
            ("A", "combinational_fall", 3),
            ("A", "combinational_rise", 3),
            ("M1", "combinational_fall", 5),
            ("M2", "combinational_fall", 5),
            ("P1", "combinational_rise", 5),
            ("P2", "combinational_rise", 5),
            ("RN", "clear", 29),
            ("RN", "preset", 5),
        ])
    }

    /// Pin Q of LBTIEX1, the simplest single-output latch in the library.
    fn lbtiex1_original_q() -> ArcCensus {
        census(&[
            ("A", "combinational", 1),
            ("RN", "clear", 1),
            ("RN", "preset", 1),
        ])
    }

    /// The named output pin of the named cell, found by name rather than position.
    fn output_pin<'a>(lib: &'a Group, cell: &str, pin: &str) -> &'a Group {
        let group = lib
            .get_cell(cell)
            .unwrap_or_else(|| panic!("cell {} not found", cell))
            .get_pin(pin)
            .unwrap_or_else(|| panic!("pin {} not found in cell {}", pin, cell));
        assert!(
            is_output_pin(group),
            "pin {} of cell {} is not an output",
            pin,
            cell
        );
        group
    }

    /// The cells the run is expected to touch, captured before processing --
    /// afterwards they no longer qualify, because the latch group they were
    /// recognised by is gone.
    fn qualifying_cells(lib: &Group) -> Vec<String> {
        lib.iter_cells()
            .filter(|c| cell_qualifies(c, ASCEND_CLOCK))
            .map(|c| c.name.clone())
            .collect()
    }

    /// Reset arcs are excluded from the pseudo-flop model but kept in the library.
    ///
    /// RACELEM21X1 is the only cell anywhere in the fixtures whose reset pin
    /// genuinely owns arcs to the output -- 34 of them -- so it is the only place
    /// where both halves of the reset treatment are observable: `process_cell`
    /// keeps reset arcs out of the model it builds, and
    /// `add_pseudo_timing_to_output_pin` keeps them in the library it emits.
    ///
    /// Killed by: `process_cell`'s phase 1 reset skip disabled -- `if reset_name.is_match(&related_pin) && false`.
    #[test]
    fn racelem21x1_excludes_reset_arcs_from_the_model_and_keeps_them_in_the_library() {
        let mut liberty =
            parse_liberty_file(Path::new(ASCEND_NOM_1V10)).expect("parse the ASCEND nom library");
        let reset_name = Regex::new(ASCEND_RESET).unwrap();

        let cell = liberty[0].get_cell("RACELEM21X1").expect("RACELEM21X1");
        assert!(cell_qualifies(cell, ASCEND_CLOCK));

        let latch = cell
            .iter_subgroups_of_type("latch")
            .next()
            .expect("RACELEM21X1 latch group");
        let data_in = latch.simple_attribute("data_in").unwrap().string();
        assert_eq!(data_in, "A*IQ+A*P1*P2+IQ*M1+IQ*M2");
        assert_eq!(latch.simple_attribute("enable").unwrap().string(), "G");
        assert_eq!(latch.simple_attribute("clear").unwrap().string(), "!RN");
        let original_q = arc_census(cell.get_pin("Q").expect("Q"));
        assert_eq!(original_q, racelem21x1_original_q());

        // The reset pin's own arcs, captured rather than written out: the claim
        // below is that they are unchanged, and a recorded number would state it as
        // a coincidence instead. It only bites because this reset owns arcs at all.
        let original_rn = arc_census(cell.get_pin("RN").expect("RN"));
        assert!(
            !original_rn.is_empty(),
            "RACELEM21X1's reset pin owns the arcs this test checks are left alone"
        );

        let reports = process_library(
            &mut liberty[0],
            &opts(
                ASCEND_CLOCK,
                &reset_name,
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        let report = reports
            .cells
            .iter()
            .find(|r| r.cell == "RACELEM21X1")
            .expect("a report for RACELEM21X1");

        // No reset arc reaches the model: not as a folded arc, not as the reference
        // the delays and constraints are drawn against, not as a constraint of its
        // own. Were the reset skip dropped, RN's 34 arcs would sit in every one of
        // these.
        for source in report.constraint_arcs.keys().map(|key| &key.src) {
            assert!(
                !reset_name.is_match(source),
                "reset pin {} was folded into the model",
                source
            );
        }
        for (source, _) in report
            .setup_input_rise
            .keys()
            .chain(report.setup_input_fall.keys())
        {
            assert!(
                !reset_name.is_match(source),
                "reset pin {} was given a constraint",
                source
            );
        }
        assert_eq!(
            report.ref_arcs[&("Q".to_owned(), Scope::Whole)].related_pin,
            "A",
            "the reference for Q must come from a data pin"
        );

        // The emitted library keeps every reset arc, and gains exactly one pseudo
        // arc in place of the 28 data arcs it dropped -- see [`ff_census`], which
        // derives that from the arcs the pin started with.
        let cell = liberty[0].get_cell("RACELEM21X1").expect("RACELEM21X1");
        assert_eq!(
            arc_census(cell.get_pin("Q").expect("Q")),
            ff_census(&original_q, ASCEND_CLOCK, &reset_name)
        );

        // The latch became a flip-flop carrying the same control expressions.
        assert_eq!(cell.iter_subgroups_of_type("latch").count(), 0);
        let ffs: Vec<&Group> = cell.iter_subgroups_of_type("ff").collect();
        assert_eq!(ffs.len(), 1, "exactly one ff group");
        assert_eq!(
            ffs[0].simple_attribute("next_state").unwrap().string(),
            data_in
        );
        assert_eq!(ffs[0].simple_attribute("clocked_on").unwrap().string(), "G");
        assert_eq!(ffs[0].simple_attribute("clear").unwrap().string(), "!RN");

        // Every data input is constrained; the reset pin keeps its own 24
        // min_pulse_width arcs and gains nothing.
        for name in ["A", "M1", "M2", "P1", "P2"] {
            let pin = cell
                .get_pin(name)
                .unwrap_or_else(|| panic!("pin {} not found", name));
            assert_eq!(
                pin.simple_attribute("nextstate_type").unwrap().expr(),
                "data",
                "pin {} should be marked as data",
                name
            );
            for timing_type in ["setup_rising", "hold_rising"] {
                assert_eq!(
                    arcs_of_type(pin, timing_type).len(),
                    1,
                    "pin {} should carry one {}",
                    name,
                    timing_type
                );
            }
        }

        let rn = cell.get_pin("RN").expect("RN");
        assert!(
            rn.simple_attribute("nextstate_type").is_none(),
            "the reset pin must not be marked as data"
        );
        // The reset is excluded from the constraint targets and no phase writes to
        // an input the conversion did not select, so its own arcs come through as
        // the fixture declared them, whatever that was.
        assert_eq!(
            arc_census(rn),
            original_rn,
            "the reset pin's own arcs must be left alone"
        );
    }

    /// The whole pseudo-flop conversion of the real ASCEND library.
    ///
    /// Replaces `test_ascend_freepdk45_comprehensive_comparison`, which diffed the
    /// output against a committed 121k-line golden file. Nothing below is a
    /// recorded output: the arcs the fixture declares are asserted before the run
    /// and the arcs expected after it are derived from those by [`ff_census`].
    ///
    /// Killed by: `add_pseudo_timing_to_output_pin`'s retain negated its reset test -- `|| !reset_name.is_match(..)`.
    #[test]
    fn ascend_ff_mode_emits_the_pseudo_flop_model_for_every_qualifying_cell() {
        let mut liberty =
            parse_liberty_file(Path::new(ASCEND_FF_1V25)).expect("parse the ASCEND ff library");
        assert_eq!(liberty.len(), 1, "the fixture holds one library");
        let reset_name = Regex::new(ASCEND_RESET).unwrap();

        // A property of the library text: of the 73 cells the fixture declares, 26
        // carry a `latch`-prefixed group, and every one of those also declares a pin
        // named `G`. `cell_qualifies` asks for exactly those two things, so 26 is
        // what the fixture offers the conversion -- and the loops below iterate it,
        // so a wrong number here would leave them silently covering less.
        let qualifying = qualifying_cells(&liberty[0]);
        assert_eq!(
            qualifying.len(),
            26,
            "cells qualifying for conversion: {:?}",
            qualifying
        );

        // Q's arc population before the run, so what is retained afterwards can be
        // counted against what there was to keep.
        let originals: Vec<(&str, ArcCensus)> = ["RACELEM21X1", "LBTIEX1"]
            .iter()
            .map(|name| (*name, arc_census(output_pin(&liberty[0], name, "Q"))))
            .collect();
        assert_eq!(originals[0].1, racelem21x1_original_q());
        assert_eq!(originals[1].1, lbtiex1_original_q());

        // The reset pins' own arcs, likewise captured rather than written out: the
        // claim afterwards is that the conversion left them alone.
        let reset_pins_before: Vec<(&str, usize)> = ["RACELEM21X1", "LBTIEX1"]
            .iter()
            .map(|name| {
                (
                    *name,
                    liberty[0]
                        .get_cell(name)
                        .expect("cell")
                        .get_pin("RN")
                        .expect("RN")
                        .iter_subgroups_of_type("timing")
                        .count(),
                )
            })
            .collect();

        process_library(
            &mut liberty[0],
            &opts(
                ASCEND_CLOCK,
                &reset_name,
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        let lib = &liberty[0];

        // (i) Every qualifying cell is now a flip-flop and no longer a latch. The
        // type prefix is matched rather than the whole name because a `latch_bank`
        // becomes an `ff_bank`.
        for name in &qualifying {
            let cell = lib
                .get_cell(name)
                .unwrap_or_else(|| panic!("cell {} not found", name));
            assert_eq!(
                cell.iter_subgroups()
                    .filter(|g| g.type_.starts_with("latch"))
                    .count(),
                0,
                "cell {} still carries a latch group",
                name
            );
            assert_eq!(
                cell.iter_subgroups()
                    .filter(|g| g.type_.starts_with("ff"))
                    .count(),
                1,
                "cell {} should carry exactly one ff group",
                name
            );
        }

        // (ii) and (iii): one pseudo arc against the clock, and every other arc
        // left on the output belongs to the reset -- with its original count.
        for (cell_name, original) in &originals {
            assert_eq!(
                arc_census(output_pin(lib, cell_name, "Q")),
                ff_census(original, ASCEND_CLOCK, &reset_name),
                "{}.Q",
                cell_name
            );
        }

        for cell_name in ["RACELEM21X1", "LBTIEX1"] {
            let pin = output_pin(lib, cell_name, "Q");
            let pseudo = pseudo_output_arc(pin, ASCEND_CLOCK);
            assert_eq!(
                pseudo.simple_attribute("timing_sense").unwrap().expr(),
                "non_unate",
                "{} pseudo arc",
                cell_name
            );
            assert_pseudo_delay_tables(pseudo);

            for timing in pin.iter_subgroups_of_type("timing") {
                let related = timing.simple_attribute("related_pin").unwrap().string();
                assert!(
                    related == ASCEND_CLOCK || reset_name.is_match(&related),
                    "{}: arc from {} survived ff mode but is neither the clock nor a reset",
                    cell_name,
                    related
                );
            }
        }

        // (iv) Each constrained input declares itself data and carries one setup
        // and one hold group against the clock, both with a populated table. M1/M2
        // are characterised falling only and P1/P2 rising only, so which of the two
        // constraint tables is present varies -- at least one must be.
        for (cell_name, inputs) in [
            ("RACELEM21X1", &["A", "M1", "M2", "P1", "P2"][..]),
            ("LBTIEX1", &["A"][..]),
        ] {
            let cell = lib.get_cell(cell_name).expect(cell_name);
            for name in inputs {
                let pin = cell
                    .get_pin(name)
                    .unwrap_or_else(|| panic!("pin {} not found in {}", name, cell_name));
                assert_eq!(
                    pin.simple_attribute("nextstate_type").unwrap().expr(),
                    "data",
                    "{}.{}",
                    cell_name,
                    name
                );

                for timing_type in ["setup_rising", "hold_rising"] {
                    let groups = arcs_of_type(pin, timing_type);
                    assert_eq!(
                        groups.len(),
                        1,
                        "{}.{} should carry one {}",
                        cell_name,
                        name,
                        timing_type
                    );
                    assert_eq!(
                        groups[0].simple_attribute("related_pin").unwrap().string(),
                        ASCEND_CLOCK,
                        "{}.{} {} must be against the clock",
                        cell_name,
                        name,
                        timing_type
                    );

                    let tables: Vec<&Group> = groups[0]
                        .iter_subgroups()
                        .filter(|g| g.type_ == "rise_constraint" || g.type_ == "fall_constraint")
                        .collect();
                    assert!(
                        !tables.is_empty(),
                        "{}.{} {} carries no constraint table",
                        cell_name,
                        name,
                        timing_type
                    );
                    for table in tables {
                        assert!(
                            !table_values(table).is_empty(),
                            "{}.{} {} {} table is empty",
                            cell_name,
                            name,
                            timing_type,
                            table.type_
                        );
                    }
                }
            }
        }

        // (v) The clock is never constrained against itself and the reset gains
        // nothing; the reset keeps only the arcs it already had, whatever the
        // fixture gave it -- neither pin is a constraint target, so the conversion
        // has no phase that writes to either.
        for &(cell_name, reset_pin_arcs) in &reset_pins_before {
            let cell = lib.get_cell(cell_name).expect(cell_name);
            for name in [ASCEND_CLOCK, "RN"] {
                let pin = cell
                    .get_pin(name)
                    .unwrap_or_else(|| panic!("pin {} not found in {}", name, cell_name));
                assert!(
                    pin.simple_attribute("nextstate_type").is_none(),
                    "{}.{} must not be marked as data",
                    cell_name,
                    name
                );
                for timing_type in ["setup_rising", "hold_rising"] {
                    assert!(
                        arcs_of_type(pin, timing_type).is_empty(),
                        "{}.{} must not be given a {}",
                        cell_name,
                        name,
                        timing_type
                    );
                }
            }
            assert_eq!(
                cell.get_pin(ASCEND_CLOCK)
                    .expect("clock pin")
                    .iter_subgroups_of_type("timing")
                    .count(),
                0,
                "{}: the clock pin carries no timing at all",
                cell_name
            );
            assert_eq!(
                cell.get_pin("RN")
                    .expect("RN")
                    .iter_subgroups_of_type("timing")
                    .count(),
                reset_pin_arcs,
                "{}: the reset pin's own arcs must be left alone",
                cell_name
            );
        }
    }

    /// The propagation-preserving latch view of the same library.
    ///
    /// Replaces `test_ascend_freepdk45_pseudolatch_comparison`. Latch mode is
    /// purely additive, so the assertion is stated as the original arc census plus
    /// the one pseudo arc rather than as a fresh set of numbers.
    ///
    /// Killed by: `add_pseudo_timing_to_output_pin` guarded its retain with `if latch` instead of `if !latch`, stripping the original arcs in latch mode.
    #[test]
    fn ascend_latch_mode_keeps_every_original_arc_and_appends_the_pseudo_arc() {
        let mut liberty =
            parse_liberty_file(Path::new(ASCEND_FF_1V25)).expect("parse the ASCEND ff library");
        let reset_name = Regex::new(ASCEND_RESET).unwrap();

        let qualifying = qualifying_cells(&liberty[0]);
        let originals: Vec<(&str, ArcCensus)> = ["RACELEM21X1", "LBTIEX1"]
            .iter()
            .map(|name| (*name, arc_census(output_pin(&liberty[0], name, "Q"))))
            .collect();
        assert_eq!(originals[0].1, racelem21x1_original_q());
        assert_eq!(originals[1].1, lbtiex1_original_q());

        process_library(
            &mut liberty[0],
            &opts(
                ASCEND_CLOCK,
                &reset_name,
                true,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
            ),
        );
        let lib = &liberty[0];

        // The latch survives: no cell is converted.
        for name in &qualifying {
            let cell = lib
                .get_cell(name)
                .unwrap_or_else(|| panic!("cell {} not found", name));
            assert_eq!(
                cell.iter_subgroups()
                    .filter(|g| g.type_.starts_with("latch"))
                    .count(),
                1,
                "cell {} should keep its latch group",
                name
            );
            assert_eq!(
                cell.iter_subgroups()
                    .filter(|g| g.type_.starts_with("ff"))
                    .count(),
                0,
                "cell {} must gain no ff group in latch mode",
                name
            );
        }

        // Every original arc is still there, plus exactly one pseudo arc.
        for (cell_name, original) in &originals {
            let mut expected = original.clone();
            *expected
                .entry((ASCEND_CLOCK.to_owned(), "rising_edge".to_owned()))
                .or_default() += 1;

            let pin = output_pin(lib, cell_name, "Q");
            assert_eq!(arc_census(pin), expected, "{}.Q", cell_name);
            assert_pseudo_delay_tables(pseudo_output_arc(pin, ASCEND_CLOCK));
        }

        // The same totals, spelled out: the census above sums to
        // 2+3+3+5+5+5+5+29+5 = 62 characterised arcs, plus the one appended; and
        // 1+1+1 = 3, plus one.
        assert_eq!(
            output_pin(lib, "RACELEM21X1", "Q")
                .iter_subgroups_of_type("timing")
                .count(),
            63
        );
        assert_eq!(
            output_pin(lib, "LBTIEX1", "Q")
                .iter_subgroups_of_type("timing")
                .count(),
            4
        );
    }
}
