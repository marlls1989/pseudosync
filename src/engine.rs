//! The conversion itself: what each cell's constraints and delays are drawn
//! against, and how a library is walked.

use crate::arcs::{
    arc_domains, extract_timing_tables_from_arc, mean_reference_arc, select_reference_arc,
    ArcAccumulator, RefArc, ReferenceMode, References, WhenMerge,
};
use crate::emit::{
    convert_latch_to_flipflop, create_hold_timing_group, create_pseudo_output_timing_arc,
    create_setup_timing_group, generate_pseudo_lut_templates,
};
use crate::pins::{
    cell_qualifies, constraint_targets_mut, is_output_pin, timing_leaves, timing_leaves_mut,
};
use crate::report::{
    collect_arc_errors, ArcError, CellReport, ConditionedArc, LibraryReport, Refusal, FALL, RISE,
};
use crate::templates::Templates;
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
                    // Substituting the cell-wide mean here would charge this input a
                    // delay measured on a *different* output, for a path nothing
                    // characterised. The caller's restriction to converted outputs
                    // makes that unreachable, so this fails loudly instead: if the
                    // restriction is ever relaxed, the failure is unambiguous rather
                    // than a plausible number in the emitted library.
                    ref_sum += ref_arcs.get(outpin).map(|r| select(r)[col]).unwrap_or_else(|| {
                        panic!(
                            "arc to output {} reached the constraint arithmetic with no reference of its own",
                            outpin
                        )
                    });
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

/// The knobs that decide how one cell is converted. They are chosen once per
/// run and travel together, so they are passed as a unit.
struct CellOptions<'a> {
    clock_name: &'a str,
    reset_name: &'a Regex,
    latch: bool,
    mode: ReferenceMode,
    when_merge: WhenMerge,
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

    // Every output leaf, whether or not it turns out to carry a usable arc. The skip
    // decision below is taken over this list rather than over the outputs that
    // reached the model: an output whose only non-reset arc carries no
    // characterisation table never enters `source_order`, and keying the decision on
    // that map is what let such an output pass unnoticed.
    let mut output_leaves: Vec<String> = Vec::new();
    // The domain of every table the conversion will transform: the lookup template named,
    // and the dimensions its table carries. Judged once, after this loop.
    let mut domains: BTreeSet<(String, (usize, usize))> = BTreeSet::new();
    // Outputs an arc of which was skipped at arc scope, so that the output-scope warning
    // can give the reason that is true rather than the one that merely fits.
    let mut axis_skipped: BTreeSet<String> = BTreeSet::new();

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
                    axis_skipped.insert(outpin_name.clone());
                    continue;
                }

                // This arc will be transformed, so its tables' domains are among those
                // that must agree. Recorded per family rather than per group, because the
                // four families of one group can name different templates.
                domains.extend(arc_domains(timing_group));

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
    let skipped: Vec<String> = output_leaves
        .iter()
        .filter(|name| !ref_arcs.contains_key(*name))
        .cloned()
        .collect();
    for outpin_name in &skipped {
        // All three are the same output-scope refusal; the wording says which of the three
        // ways the output fell short, so the warning can be acted on. The arc-scope case
        // has to be asked about separately: an arc skipped there never reaches
        // `source_order`, so judging by that map alone would report an output whose every
        // arc was skipped for its template as having carried no table at all.
        let reason = if source_order.contains_key(outpin_name) {
            "no non-reset source supplies a complete reference"
        } else if axis_skipped.contains(outpin_name) {
            "every non-reset arc was skipped: no lookup template with both axes"
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

    // Phase 3: Add pseudo timing to each output pin
    for outpin in timing_leaves_mut(cell, is_output_pin) {
        let outpin_name = &outpin.name;

        // A skipped output is left exactly as the input wrote it, and phase 2 has
        // already named it. Note that the retain which strips a converted output's
        // original non-reset arcs lives inside `add_pseudo_timing_to_output_pin`, so
        // skipping here is what keeps a skipped output's originals in the default
        // mode -- under `--latch` they survive by construction either way.
        let Some(output_transitions) = ref_arcs.get(outpin_name) else {
            continue;
        };

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
    cell_rise_arcs.retain(|(_, output), _| ref_arcs.contains_key(output));
    cell_fall_arcs.retain(|(_, output), _| ref_arcs.contains_key(output));

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

    reports.cells.push(CellReport {
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
    //
    // Reset pins need no test of their own: phase 1 skips every arc whose
    // `related_pin` matches `reset_name`, so a reset name never becomes a key of
    // `accumulated`, hence never of `cell_{rise,fall}_arcs`, hence never of the
    // setup maps those are grouped into. Membership below therefore already
    // implies the name is not a reset. The clock has no such skip -- a latch
    // characterises its output against the enable -- so that test is live.
    let has_constraints = |name: &str| {
        name != clock_name && (setup_rise.contains_key(name) || setup_fall.contains_key(name))
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
    Ok(ref_arc.lut_template)
}

/// Process a library to convert latches to flip-flops or add pseudo-synchronous
/// timing, choosing how the clock-to-output reference is drawn.
///
/// Returns a [`CellReport`] per converted cell -- carrying the original arcs, the
/// reconstruction and its residual, so the cost of the chosen [`ReferenceMode`] can be
/// measured against the library it replaced -- together with a [`Refusal`] for every
/// candidate cell, or output of one, the conversion could not honour.
pub(crate) fn process_library(
    lib: &mut Group,
    clock_name: &str,
    reset_name: &Regex,
    latch: bool,
    mode: ReferenceMode,
    when_merge: WhenMerge,
) -> LibraryReport {
    eprintln!("Processing library {}", lib.name);

    let opts = CellOptions {
        clock_name,
        reset_name,
        latch,
        mode,
        when_merge,
    };
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
        match process_cell(cell, &opts, &templates, &lib_name, &mut reports) {
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
    use crate::arcs::{mean_reference_arc, RefArc, ReferenceMode, WhenMerge};
    use crate::liberty_io::parse_liberty_file;
    use crate::pins::{cell_qualifies, is_output_pin};
    use liberty_parser::{
        ast::Value,
        liberty::{Group, Liberty},
    };
    use regex::Regex;
    use std::collections::BTreeMap;
    use std::path::Path;

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

    // --- bundle traversal --------------------------------------------------

    /// Four timing tables for one arc, so `select_reference_arc` accepts it.
    fn arc(related_pin: &str, timing_type: &str, base: f64) -> String {
        format!(
            r#"
        timing() {{
          related_pin: "{}";
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            latch,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
    ///     setup_rise(A) = [2, 4]     - 4   = [-2, 0]
    ///     setup_fall(A) = [2.5, 4.5] - 4.5 = [-2, 0]
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            true,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
            "G",
            &Regex::new("(R|S)N?").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
            "CLK",
            &Regex::new("RST").unwrap(),
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
                "G",
                &reset_name,
                false,
                ReferenceMode::PerOutput,
                WhenMerge::Mean,
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
            ASCEND_CLOCK,
            &reset_name,
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
        for (source, _) in report
            .cell_rise_arcs
            .keys()
            .chain(report.cell_fall_arcs.keys())
        {
            assert!(
                !reset_name.is_match(source),
                "reset pin {} was folded into the model",
                source
            );
        }
        for source in report.setup_rise.keys().chain(report.setup_fall.keys()) {
            assert!(
                !reset_name.is_match(source),
                "reset pin {} was given a constraint",
                source
            );
        }
        assert_eq!(
            report.ref_arcs["Q"].related_pin, "A",
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
            ASCEND_CLOCK,
            &reset_name,
            false,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
            ASCEND_CLOCK,
            &reset_name,
            true,
            ReferenceMode::PerOutput,
            WhenMerge::Mean,
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
