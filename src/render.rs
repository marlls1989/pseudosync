//! Rendering of the reconstruction report: the tables, the per-arc statistics
//! and the sections they are laid out in.

use crate::arcs::{input_transition, Anchor, ReferenceMode, Scope, Transition};
use crate::conditions::ClassId;
use crate::report::{ArcError, CellReport, ConditionedArc, Refusal};
use gpoint::GPoint;
use itertools::Itertools;
use ndarray::prelude::*;
use std::{error::Error, io::Write};

fn g(v: f64) -> String {
    format!("{}", GPoint(v))
}

/// Which axes a table is indexed on, for captioning its rows and columns.
///
/// A residual read against a row number says nothing; read against the slew and load
/// it occurs at, it says which regime the conversion is failing in. An axis is `None`
/// where the library declares none, or where its length does not match the table it
/// would caption — a mismatched axis would misattribute every value in the table, so
/// it is not used at all.
#[derive(Default, Clone, Copy)]
struct Labels<'a> {
    rows: Option<&'a [f64]>,
    columns: Option<&'a [f64]>,
}

impl<'a> Labels<'a> {
    fn rows(v: Option<&'a Vec<f64>>) -> Self {
        Labels {
            rows: v.map(|x| x.as_slice()),
            columns: None,
        }
    }

    fn columns(v: Option<&'a Vec<f64>>) -> Self {
        Labels {
            rows: None,
            columns: v.map(|x| x.as_slice()),
        }
    }

    fn grid(rows: Option<&'a Vec<f64>>, columns: Option<&'a Vec<f64>>) -> Self {
        Labels {
            rows: rows.map(|x| x.as_slice()),
            columns: columns.map(|x| x.as_slice()),
        }
    }

    /// The axes that match the shape actually being drawn; a mismatch drops that axis.
    fn fitted(self, rows: usize, columns: usize) -> Self {
        Labels {
            rows: self.rows.filter(|a| a.len() == rows),
            columns: self.columns.filter(|a| a.len() == columns),
        }
    }
}

/// Render a table the way the original report did: one prettytable row per row
/// of the array, values in %g. A 1-D arc renders as the single row it is.
///
/// Where the axes are known the grid is captioned with them: the column header
/// carries the load each column was measured at, and the leading cell of each row the
/// input slew. The corner names which is which.
fn dump<D: Dimension>(
    sink: &mut dyn Write,
    label: &str,
    a: &ArrayBase<ndarray::OwnedRepr<f64>, D>,
    labels: Labels,
) -> Result<(), Box<dyn Error>> {
    writeln!(sink, "{}", label)?;

    let shape: Vec<usize> = a.rows().into_iter().map(|r| r.len()).collect();
    let labels = labels.fitted(shape.len(), shape.first().copied().unwrap_or(0));

    let mut table = prettytable::Table::new();
    if let Some(columns) = labels.columns {
        let mut header = Vec::new();
        if labels.rows.is_some() {
            header.push(prettytable::Cell::new("slew \\ load"));
        }
        header.extend(columns.iter().map(|v| prettytable::Cell::new(&g(*v))));
        table.add_row(prettytable::Row::new(header));
    }
    for (i, row) in a.rows().into_iter().enumerate() {
        let mut cells = Vec::new();
        if let Some(rows) = labels.rows {
            cells.push(prettytable::Cell::new(&g(rows[i])));
        }
        cells.extend(row.iter().map(|v| prettytable::Cell::new(&g(*v))));
        table.add_row(prettytable::Row::new(cells));
    }
    writeln!(sink, "{}", table)?;

    Ok(())
}

/// How a reference profile was read off its table, for the caption above it.
///
/// An averaged reference is never captioned with a row index: it was not taken at
/// one, and printing the number the middle anchor would have used would name a
/// measurement the profile is not.
fn anchor_caption(anchor: Anchor, row: usize) -> String {
    match anchor {
        Anchor::Middle => format!("(row {})", row),
        Anchor::Average => "(average row)".to_owned(),
    }
}

/// The same for the cell-wide mean reference, which names both indices.
fn mean_anchor_caption(anchor: Anchor, col: usize, row: usize) -> String {
    match anchor {
        Anchor::Middle => format!("(col {}, row {})", col, row),
        Anchor::Average => "(average col, average row)".to_owned(),
    }
}

/// Whether this report was drawn per state, which is what the per-class sections
/// are for.
///
/// Read off the keys rather than handed in: a report holding a reference at anything
/// finer than the whole output is a per-state one, and one holding none has nothing
/// per-class to say.
fn is_per_state(r: &CellReport) -> bool {
    r.ref_arcs.keys().any(|(_, scope)| *scope != Scope::Whole)
}

/// How a table filed under a scope is captioned, given what the classes of that
/// scope's own partition denote.
///
/// Empty for a whole-output scope, because that is the output's only reference and
/// naming it would say nothing -- so every caption drawn under a mode that has only
/// that scope reads exactly as it always has.
fn caption<'a>(scope: &Scope, named: impl Fn(&ClassId) -> Option<&'a str>) -> String {
    match scope {
        Scope::Whole => String::new(),
        // A `when`-less arc states no condition; it covers whatever the conditioned
        // arcs of its output do not.
        Scope::CatchAll => " [catch-all]".to_owned(),
        Scope::State(class) => format!(" [{}]", named(class).unwrap_or("unnamed state")),
    }
}

/// The post-settled state an OUTPUT's table was filed under.
fn scope_caption(scope: &Scope, r: &CellReport) -> String {
    caption(scope, |class| {
        r.class_conditions.get(class).map(String::as_str)
    })
}

/// The condition an INPUT pin's checks were grouped under.
///
/// A different partition from the states above and numbered per pin, so the pin is
/// part of the lookup. Captioning a constraint from the states instead would name
/// whichever output's state happens to share its number, which is a condition the
/// constraint was never confined to.
fn check_caption(pin: &str, scope: &Scope, r: &CellReport) -> String {
    caption(scope, |class| {
        r.check_conditions
            .get(&(pin.to_owned(), *class))
            .map(String::as_str)
    })
}

/// Whether one raw arc's own edge was filed under `scope`.
///
/// Stated without reference to the mode: a whole-output scope covers every arc of
/// its output, a catch-all covers the arcs that state no condition, and a state
/// covers the arcs whose edge fell in that class.
fn in_scope(arc: &ConditionedArc, family: &str, scope: &Scope) -> bool {
    match scope {
        Scope::Whole => true,
        Scope::CatchAll => arc.when.is_none(),
        Scope::State(class) => {
            let edge = match family_output(family) {
                Transition::Rise => arc.class_rise,
                Transition::Fall => arc.class_fall,
            };
            edge == Some(*class)
        }
    }
}

/// Whether one raw arc's own source condition was grouped under `scope`.
///
/// The same three cases read on the other partition: one check per pin covers every
/// arc of it, a catch-all covers the arcs that state no condition, and a class covers
/// the arcs whose `when` fell in it.
fn in_check_scope(arc: &ConditionedArc, scope: &Scope) -> bool {
    match scope {
        Scope::Whole => true,
        Scope::CatchAll => arc.when.is_none(),
        Scope::State(class) => arc.check_class == Some(*class),
    }
}

fn dump_cell(sink: &mut dyn Write, r: &CellReport) -> Result<(), Box<dyn Error>> {
    writeln!(sink, "cell {} of library {}", r.cell, r.library)?;

    // The reduced arcs the constraints were derived from. The per-condition
    // originals are printed with their comparisons below.
    let grid = Labels::grid(r.slews.as_ref(), r.loads.as_ref());
    let by_slew = Labels::rows(r.slews.as_ref());
    let by_load = Labels::columns(r.loads.as_ref());
    let per_state = is_per_state(r);

    // Captioned with both keys the values are filed under: the family they were
    // characterised in, which names the OUTPUT's direction, and the input's own
    // direction, which is what the constraint built from them is keyed on. Naming
    // only one of the two would read as a claim that they agree. Under a per-state
    // reference each pin is named with the condition its own tables are filed under:
    // the source with the condition its checks are grouped by, the output with the
    // state its delay was drawn at. Both are needed to tell two entries apart.
    for (key, v) in &r.constraint_arcs {
        dump(
            sink,
            &format!(
                "mean {} arc {}{} -> {}{} (input {}):",
                key.family,
                key.src,
                check_caption(&key.src, &key.check, r),
                key.outpin,
                scope_caption(&key.delay, r),
                key.input_edge.name()
            ),
            v,
            grid,
        )?;
    }

    for ((out, scope), v) in &r.ref_arcs {
        let at = anchor_caption(v.anchor, v.row);
        let state = scope_caption(scope, r);
        // Only the edges this reference carries: a state characterised in one
        // direction alone has no profile to print for the other.
        for (edge_name, edge) in [("rise", v.rise.as_ref()), ("fall", v.fall.as_ref())] {
            let Some(edge) = edge else { continue };
            dump(
                sink,
                &format!(
                    "ref {} arc {} -> {}{} {}:",
                    edge_name, v.related_pin, out, state, at
                ),
                &edge.delay,
                by_load,
            )?;
            // The constant the constraint half of this state is offset by. Printed
            // per state only: with one reference per output it is the same number
            // for every table below, and is read off the setup arcs there.
            if per_state {
                writeln!(sink, "crossing: {}", g(edge.crossing))?;
            }
        }
    }

    if let Some(rise) = r.mean_ref.rise.as_ref() {
        dump(
            sink,
            &format!(
                "mean ref rise arc {}:",
                mean_anchor_caption(r.mean_ref.anchor, r.mean_ref.col, r.mean_ref.row)
            ),
            &rise.delay,
            by_load,
        )?;
    }
    if let Some(fall) = r.mean_ref.fall.as_ref() {
        dump(sink, "mean ref fall arc:", &fall.delay, by_load)?;
    }

    // Captioned with the condition the pin's own checks are grouped under, which is
    // what these values are keyed by: one figure per condition, averaged over the
    // outputs that pin drives under it.
    for ((k, scope), v) in &r.setup_input_rise {
        let label = format!(
            "setup arc {}{} (input rise):",
            k,
            check_caption(k, scope, r)
        );
        dump(sink, &label, v, by_slew)?;
    }
    for ((k, scope), v) in &r.setup_input_fall {
        let label = format!(
            "setup arc {}{} (input fall):",
            k,
            check_caption(k, scope, r)
        );
        dump(sink, &label, v, by_slew)?;
    }

    // The hold constraints are the setup ones negated, so with one group per pin
    // they say nothing the section above does not. Per state there is one pair per
    // condition, and which condition carries which figure is what the section is
    // for.
    if per_state {
        for ((k, scope), v) in &r.hold_input_rise {
            let label = format!("hold arc {}{} (input rise):", k, check_caption(k, scope, r));
            dump(sink, &label, v, by_slew)?;
        }
        for ((k, scope), v) in &r.hold_input_fall {
            let label = format!("hold arc {}{} (input fall):", k, check_caption(k, scope, r));
            dump(sink, &label, v, by_slew)?;
        }
    }

    dump_check_conditions(sink, r)?;

    // How much error the conversion introduces over the characterised arcs it
    // aims to replace, and in which regions and regimes that error is most
    // prominent. So the baseline is the arc as the source library characterised
    // it, never the `when`-merged one: the merge is itself a source of error, and
    // measuring against it would report less error than the conversion introduces.
    // The tables are slew x load, in the shape the arc was characterised in, so
    // the region can be seen rather than inferred from a statistic; the normalised
    // one makes regions of different magnitude comparable.
    for a in &r.arcs {
        let condition = match &a.when {
            Some(w) => format!("  when {}", w),
            None => "  unconditioned".to_owned(),
        };
        let head = format!("{} arc {} -> {}{}", a.edge, a.source, a.output, condition);

        dump(sink, &format!("{}\noriginal:", head), &a.original, grid)?;
        dump(sink, "reconstructed:", &a.reconstructed, grid)?;
        dump(sink, "error:", &a.error, grid)?;
        dump(sink, "error % of original:", &a.relative_error(), grid)?;
        writeln!(sink, "{}\n", stat_line(a))?;
    }

    Ok(())
}

/// The conditions each input pin's setup and hold groups were stated under.
///
/// Deliberately its own block, and deliberately not the `collision classes` one: that
/// numbers the states the cell's OUTPUTS settle in and is what the emitted delays are
/// conditioned on, while this numbers the conditions the library characterised each
/// INPUT under and is what the emitted checks are conditioned on. The two are
/// classified separately and their class numbers mean different things, so they are
/// never printed as one list.
///
/// Empty under a mode that emits one unconditioned check per pin, where the checks
/// were grouped under nothing at all.
fn dump_check_conditions(sink: &mut dyn Write, r: &CellReport) -> Result<(), Box<dyn Error>> {
    if r.check_classes.is_empty() {
        return Ok(());
    }

    writeln!(sink, "check conditions")?;
    writeln!(
        sink,
        "the source `when` each pin's setup and hold groups are stated under, classified per pin\n"
    )?;
    for class in &r.check_classes {
        writeln!(
            sink,
            "  {} {:<44} {} arc(s): {}",
            class.pin,
            class.condition.as_deref().unwrap_or("catch-all"),
            class.members.len(),
            class.members.join(", ")
        )?;
    }
    writeln!(sink)?;

    Ok(())
}

/// Statistics of an arbitrary residual, in the same shape as `stat_line`.
fn stats_of(err: &Array2<f64>, reference: &Array2<f64>) -> String {
    let n = err.len();
    let mean = err.iter().sum::<f64>() / n as f64;
    let sd = (err.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
    let rms = (err.iter().map(|v| v * v).sum::<f64>() / n as f64).sqrt();
    let scale = reference.iter().map(|v| v.abs()).sum::<f64>() / n as f64;
    let (min, max) = err
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    format!(
        "stats: n {}  scale {}  bias {}  sd {}  rms {}  min {}  max {}  rms/scale {:.2}%",
        n,
        g(scale),
        g(mean),
        g(sd),
        g(rms),
        g(min),
        g(max),
        if scale == 0.0 {
            0.0
        } else {
            100.0 * rms / scale
        }
    )
}

/// The raw table a delay family names, on one characterised arc.
fn family_table<'a>(arc: &'a ConditionedArc, family: &str) -> Option<&'a Array2<f64>> {
    match family {
        "cell_rise" => arc.cell_rise.as_ref(),
        _ => arc.cell_fall.as_ref(),
    }
}

/// The direction a delay family says the OUTPUT moved in.
fn family_output(family: &str) -> Transition {
    match family {
        "cell_rise" => Transition::Rise,
        _ => Transition::Fall,
    }
}

/// What the `when` reduction alone costs, before the pseudo-flop split.
///
/// Each pin pair is characterised once per operating state and the engine keeps
/// one mean of them. This measures that mean against each state it stands in
/// for, so the reduction's error can be read separately from the error of the
/// separable setup-plus-delay form that consumes it.
fn dump_reduction(sink: &mut dyn Write, r: &CellReport) -> Result<(), Box<dyn Error>> {
    writeln!(
        sink,
        "== when-reduction error, cell {} of library {} ==",
        r.cell, r.library
    )?;
    writeln!(
        sink,
        "mean arc measured against each condition it replaces, no split involved\n"
    )?;

    // The conditions this group was merged from: the ones carrying the family's
    // table AND routed to this input direction AND filed under both of the key's
    // scopes. Every part of the key is needed -- a pin pair characterised under two
    // senses contributes its `cell_rise` tables to two different groups, under a
    // per-state reference one condition's arcs are merged only with the arcs
    // describing the same state, and arcs whose `when`s put them in different check
    // groups are merged only within their own group -- and measuring one group
    // against another's conditions would report a reduction error that no reduction
    // made.
    for (key, mean) in &r.constraint_arcs {
        let (source, output, family) = (&key.src, &key.outpin, key.family);
        let conditions: Vec<&ConditionedArc> = r
            .raw_arcs
            .iter()
            .filter(|a| &a.source == source && &a.output == output)
            .filter(|a| family_table(a, family).is_some())
            .filter(|a| input_transition(a.sense, family_output(family)) == Some(key.input_edge))
            .filter(|a| in_scope(a, family, &key.delay))
            .filter(|a| in_check_scope(a, &key.check))
            .collect();

        // Printed so the reduction can be checked without trusting it: for
        // all-positive delay tables the mean's own magnitude must equal the
        // mean of the conditions' magnitudes. If those two disagree, the
        // reduction is not averaging what it claims to be averaging.
        let scale_of = |t: &Array2<f64>| t.iter().map(|v| v.abs()).sum::<f64>() / t.len() as f64;
        let condition_scales: Vec<f64> = conditions
            .iter()
            .map(|c| scale_of(family_table(c, family).unwrap()))
            .collect();
        let mean_of_scales =
            condition_scales.iter().sum::<f64>() / condition_scales.len().max(1) as f64;

        writeln!(
            sink,
            "{} arc {}{} -> {}{} (input {}): mean of {} condition(s)  |  mean scale {}  mean-of-condition scales {}",
            family,
            source,
            check_caption(source, &key.check, r),
            output,
            scope_caption(&key.delay, r),
            key.input_edge.name(),
            conditions.len(),
            g(scale_of(mean)),
            g(mean_of_scales)
        )?;

        let mut worst: f64 = 0.0;
        for c in &conditions {
            let raw = family_table(c, family).unwrap();
            if raw.raw_dim() != mean.raw_dim() {
                continue;
            }
            let err = mean - raw;
            worst = worst.max(err.iter().fold(0.0_f64, |m, v| m.max(v.abs())));
            writeln!(
                sink,
                "  {:<44} {}",
                c.when.clone().unwrap_or("unconditioned".to_owned()),
                stats_of(&err, raw)
            )?;
        }
        let spread = condition_scales
            .iter()
            .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
        if spread.0 > 0.0 && spread.1 / spread.0 > 2.0 {
            writeln!(
                sink,
                "  WIDE SPREAD: conditions range {} .. {} ({:.1}x) -- the merged arc \n               represents no single operating state",
                g(spread.0),
                g(spread.1),
                spread.1 / spread.0
            )?;
        }
        writeln!(sink, "  worst |mean - condition|: {}\n", g(worst))?;
    }

    dump_collision_classes(sink, r)
}

/// The states one cell's arcs describe, grouped by OVERLAP: conditions that can hold
/// at once are one row, which is a weaker grouping than denoting the same function.
///
/// So a row is headed by its class's merged condition -- the one its arc carries in
/// the emitted library -- and not by any one member's, which can be strictly narrower
/// than the state the row heads. The members are listed beside it either way.
///
/// Informational under every mode: nothing in the split consults a condition, so
/// this changes no emitted byte. What it says is how much of the cell a per-state
/// model would have to distinguish, and where two arcs already claim one state.
fn dump_collision_classes(sink: &mut dyn Write, r: &CellReport) -> Result<(), Box<dyn Error>> {
    writeln!(sink, "collision classes")?;
    writeln!(
        sink,
        "each arc's `when` conjoined with the literal for the direction its input settled in\n"
    )?;

    for class in &r.classes {
        let members: Vec<String> = class
            .members
            .iter()
            .map(|(pin, when)| match when {
                Some(when) => format!("{}({})", pin, when),
                None => pin.clone(),
            })
            .collect();
        writeln!(
            sink,
            "  {} {}  {:<44} {} arc(s): {}",
            class.output,
            class.edge.name(),
            // A whenless arc states no condition; it covers whatever the
            // conditioned arcs of its output and edge do not.
            class.condition.as_deref().unwrap_or("catch-all"),
            class.members.len(),
            members.join(", ")
        )?;
    }

    // An arc whose `when` could not be read describes a state that cannot be
    // named, so it belongs to no class and to no catch-all either. It is
    // recognisable exactly there: a conditioned arc that carries a delay family --
    // so it would have been classified -- and yet holds no class for either edge.
    for arc in r.raw_arcs.iter().filter(|a| {
        a.when.is_some()
            && a.class_rise.is_none()
            && a.class_fall.is_none()
            && (a.cell_rise.is_some() || a.cell_fall.is_some())
    }) {
        writeln!(
            sink,
            "  UNREADABLE CONDITION: arc {} -> {} when {}",
            arc.source,
            arc.output,
            arc.when.as_deref().unwrap_or("")
        )?;
    }
    writeln!(sink)?;

    Ok(())
}

/// One-line statistical summary of a single comparison.
fn stat_line(a: &ArcError) -> String {
    let n = a.error.len();
    let mean = a.bias();
    // Spread about the mean, distinct from the rms about zero: a large rms with
    // a small sd is a systematic offset, the reverse is scatter.
    let sd = (a.error.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n as f64).sqrt();
    let (min, max) = a
        .error
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), v| (lo.min(*v), hi.max(*v)));
    format!(
        "stats: n {}  scale {}  bias {}  sd {}  rms {}  min {}  max {}  |max| {}  rms/scale {:.2}%",
        n,
        g(a.scale()),
        g(mean),
        g(sd),
        g(a.rms()),
        g(min),
        g(max),
        g(a.max_abs()),
        a.rms_percent()
    )
}

fn write_summary(sink: &mut dyn Write, arcs: &[&ArcError]) -> Result<(), Box<dyn Error>> {
    let mut table = prettytable::Table::new();
    table.add_row(prettytable::row![
        "cell",
        "arc",
        "edge",
        "condition",
        "scale",
        "bias",
        "rms",
        "|max|",
        "rms/scale"
    ]);
    for a in arcs {
        table.add_row(prettytable::row![
            a.cell,
            format!("{} -> {}", a.source, a.output),
            a.edge,
            a.when.as_deref().unwrap_or("-"),
            g(a.scale()),
            g(a.bias()),
            g(a.rms()),
            g(a.max_abs()),
            format!("{:.2}%", a.rms_percent()),
        ]);
    }
    writeln!(sink, "{}", table)?;

    // Quadratic quantities combine as means of squares, not means of roots.
    let rollup = |name: &str, sel: &dyn Fn(&ArcError) -> bool, sink: &mut dyn Write| {
        let chosen: Vec<&&ArcError> = arcs.iter().filter(|a| sel(a)).collect();
        if chosen.is_empty() {
            return Ok(());
        }
        let n = chosen.len() as f64;
        let rms = (chosen.iter().map(|a| a.rms() * a.rms()).sum::<f64>() / n).sqrt();
        let scale = chosen.iter().map(|a| a.scale()).sum::<f64>() / n;
        let bias = chosen.iter().map(|a| a.bias()).sum::<f64>() / n;
        let worst = chosen.iter().fold(0.0_f64, |m, a| m.max(a.max_abs()));
        let worst_rel = chosen.iter().fold(0.0_f64, |m, a| m.max(a.rms_percent()));
        writeln!(
            sink,
            "{:<26} arcs {:>3}  bias {:>10}  rms {:>10}  |max| {:>10}  rms/scale {:>7.2}%  worst arc {:>7.2}%",
            name,
            chosen.len(),
            g(bias),
            g(rms),
            g(worst),
            if scale == 0.0 { 0.0 } else { 100.0 * rms / scale },
            worst_rel
        )
    };

    // `dedup` would only drop *adjacent* repeats. Nothing in this function's signature
    // guarantees that one cell's arcs arrive adjacent, so it does not assume they do:
    // `unique` is correct for any ordering, `dedup` only for one. Order is first
    // appearance, not sorted, because the rollup lines are compared between runs.
    let cells: Vec<&str> = arcs.iter().map(|a| a.cell.as_str()).unique().collect();
    for cell in cells {
        rollup(cell, &|a: &ArcError| a.cell == cell, sink)?;
    }
    rollup("ALL", &|_: &ArcError| true, sink)?;
    Ok(())
}

/// The relative error over a chosen set of arcs, extremes and robust band alike.
///
/// The extremes alone are misleading here. A characterised delay legitimately passes
/// through zero and goes negative — the output arrives before the input has finished
/// switching — and a point-by-point ratio taken next to that crossing is arbitrarily
/// large however small the residual is. So the percentiles are reported beside the
/// extremes: they say what the error is away from those few points, which is what the
/// conversion's accuracy actually turns on.
///
/// A point characterised as exactly zero has no ratio at all and is counted as
/// `undef` rather than folded in as a substituted number.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Band {
    min: f64,
    p5: f64,
    p50: f64,
    p95: f64,
    max: f64,
    mean: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct RelativeStats {
    points: usize,
    undef: usize,
    /// `None` when no point of the group could be measured at all.
    band: Option<Band>,
}

/// Nearest-rank percentile of an ascending slice: no interpolation, so every figure
/// reported is a residual that was actually measured somewhere.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    let last = sorted.len() - 1;
    let rank = (q * last as f64).round() as usize;
    sorted[rank.min(last)]
}

fn relative_stats(arcs: &[&ArcError], keep: impl Fn(&ArcError) -> bool) -> RelativeStats {
    let mut values: Vec<f64> = Vec::new();
    let mut undef = 0_usize;

    for a in arcs.iter().filter(|a| keep(a)) {
        for v in a.relative_error().iter() {
            if v.is_finite() {
                values.push(*v);
            } else {
                undef += 1;
            }
        }
    }

    if values.is_empty() {
        return RelativeStats {
            points: 0,
            undef,
            band: None,
        };
    }

    let mean = values.iter().sum::<f64>() / values.len() as f64;
    values.sort_by(f64::total_cmp);

    RelativeStats {
        points: values.len(),
        undef,
        band: Some(Band {
            min: values[0],
            p5: percentile(&values, 0.05),
            p50: percentile(&values, 0.50),
            p95: percentile(&values, 0.95),
            max: values[values.len() - 1],
            mean,
        }),
    }
}

/// Where the conversion's error is worst, written before any of the detail.
///
/// The per-arc statistics measure a residual against one magnitude, which cannot
/// say which output or which edge carries the error. This groups the relative
/// error three ways — by cell, then by output, then by direction — so the tables
/// below can be read starting from the worst group rather than in order.
fn write_relative_error_summary(
    sink: &mut dyn Write,
    arcs: &[&ArcError],
) -> Result<(), Box<dyn Error>> {
    if arcs.is_empty() {
        return Ok(());
    }

    writeln!(sink, "relative error summary")?;
    writeln!(
        sink,
        "100 * (reconstructed - original) / original, over every characterised point"
    )?;
    writeln!(
        sink,
        "p5 to p95 is the error away from the few points where the arc crosses zero\n"
    )?;

    let mut table = prettytable::Table::new();
    table.add_row(prettytable::row![
        "scope", "points", "undef", "min %", "p5 %", "p50 %", "p95 %", "max %", "avg %"
    ]);

    let add = |scope: String, stats: RelativeStats, t: &mut prettytable::Table| {
        // Nothing measurable is said as such, never as a zero that would read as a
        // perfect reconstruction.
        let cells = match stats.band {
            Some(b) => [b.min, b.p5, b.p50, b.p95, b.max, b.mean].map(g),
            None => std::array::from_fn(|_| "-".to_owned()),
        };
        t.add_row(prettytable::Row::new(
            [scope, stats.points.to_string(), stats.undef.to_string()]
                .into_iter()
                .chain(cells)
                .map(|c| prettytable::Cell::new(&c))
                .collect(),
        ));
    };

    // First appearance rather than sorted, so the grouping follows the order the
    // arcs were collected in and the rows can be compared between runs.
    let cells: Vec<&str> = arcs.iter().map(|a| a.cell.as_str()).unique().collect();
    for cell in cells {
        add(
            format!("cell {}", cell),
            relative_stats(arcs, |a| a.cell == cell),
            &mut table,
        );

        let outputs: Vec<&str> = arcs
            .iter()
            .filter(|a| a.cell == cell)
            .map(|a| a.output.as_str())
            .unique()
            .collect();
        for output in outputs {
            add(
                format!("  output {}", output),
                relative_stats(arcs, |a| a.cell == cell && a.output == output),
                &mut table,
            );

            let directions: Vec<&str> = arcs
                .iter()
                .filter(|a| a.cell == cell && a.output == output)
                .map(|a| a.edge)
                .unique()
                .collect();
            for direction in directions {
                add(
                    format!("    {}", direction),
                    relative_stats(arcs, |a| {
                        a.cell == cell && a.output == output && a.edge == direction
                    }),
                    &mut table,
                );
            }
        }
    }
    add("ALL".to_owned(), relative_stats(arcs, |_| true), &mut table);

    writeln!(sink, "{}", table)?;
    Ok(())
}

pub(crate) fn write_report(
    sink: &mut dyn Write,
    reports: &[CellReport],
    refusals: &[Refusal],
    mode: ReferenceMode,
    anchor: Anchor,
    summary_only: bool,
) -> Result<(), Box<dyn Error>> {
    writeln!(sink, "reference mode: {:?}", mode)?;
    writeln!(sink, "anchor: {:?}", anchor)?;
    if let Some(r) = reports.first() {
        writeln!(sink, "when-arc merge: {:?}", r.when_merge)?;
    }
    writeln!(
        sink,
        "residual of reconstructing each arc as setup + clock-to-output delay\n"
    )?;

    // The opening summary: which cell, which output and which edge the error sits
    // on, so the reader knows where to look before meeting any of the tables. Under
    // `--report-summary-only` too -- it is the summary that flag keeps.
    let all: Vec<&ArcError> = reports.iter().flat_map(|r| r.arcs.iter()).collect();
    write_relative_error_summary(sink, &all)?;

    // Before the tables, and under `--report-summary-only` as well. That flag limits
    // the tables; a refusal is not a table. A run with skips exits 0, so the report
    // and standard error are the only signals it has -- suppressing refusals here
    // would make the flag a mode in which skips vanish from the machine-readable
    // artefact altogether.
    if !refusals.is_empty() {
        writeln!(sink, "refusals")?;
        for r in refusals {
            match &r.output {
                Some(output) => writeln!(
                    sink,
                    "refused: cell {} of library {}, output {}: {}",
                    r.cell, r.library, output, r.reason
                )?,
                None => writeln!(
                    sink,
                    "refused: cell {} of library {}: {}",
                    r.cell, r.library, r.reason
                )?,
            }
        }
        writeln!(sink)?;
    }

    if !summary_only {
        // The reduction is upstream of the split, so it is reported first.
        for r in reports {
            dump_reduction(sink, r)?;
        }
        for r in reports {
            dump_cell(sink, r)?;
        }
    }

    let arcs: Vec<&ArcError> = reports.iter().flat_map(|r| r.arcs.iter()).collect();
    write_summary(sink, &arcs)
}

#[cfg(test)]
mod tests {
    //! Behaviour of the report renderers: formatting, statistics and section layout.

    use super::*;
    use crate::arcs::{EdgeRef, RefArc, ReferenceMode, TimingSense, WhenMerge};
    use crate::conditions::{collision_classes, Condition}; // Test-only; the fixture's class ids are minted by the real classifier rather than written down.
    use crate::report::{CheckClass, ConstraintKey, StateClass};
    use std::collections::BTreeMap;
    use std::io::{self, Write};

    /// Every renderer takes a `&mut dyn Write`, so a report can be produced into
    /// memory: none of these tests touch the filesystem or the binary.
    fn rendered(render: impl FnOnce(&mut dyn Write) -> Result<(), Box<dyn Error>>) -> String {
        let mut sink: Vec<u8> = Vec::new();
        render(&mut sink).expect("a Vec accepts every byte, so rendering into memory cannot fail");
        String::from_utf8(sink).expect("the renderers emit utf-8")
    }

    /// The cells of every prettytable row in `out`, whitespace trimmed. Table rows
    /// are the only lines drawn with `|`.
    fn table_rows(out: &str) -> Vec<Vec<&str>> {
        out.lines()
            .filter(|l| l.starts_with('|'))
            .map(|l| l.trim_matches('|').split('|').map(str::trim).collect())
            .collect()
    }

    /// Everything that is not part of a drawn table: the labels and stat lines.
    fn prose_lines(out: &str) -> Vec<&str> {
        out.lines()
            .filter(|l| !l.is_empty() && !l.starts_with('|') && !l.starts_with('+'))
            .collect()
    }

    /// The leading name of each rollup line, in the order they were written.
    fn rollup_names(out: &str) -> Vec<&str> {
        out.lines()
            .filter(|l| !l.starts_with('|') && l.contains(" arcs "))
            .filter_map(|l| l.split_whitespace().next())
            .collect()
    }

    /// The statistics of [`arc_error`], computed by hand from its tables:
    /// error [-7, 1] over an original of magnitude 2. bias (-7+1)/2 = -3;
    /// sd sqrt((16+16)/2) = 4; rms sqrt((49+1)/2) = 5; |max| 7; rms/scale 5/2.
    const ARC_ERROR_STATS: &str =
        "stats: n 2  scale 2  bias -3  sd 4  rms 5  min -7  max 1  |max| 7  rms/scale 250.00%";

    /// A residual whose every statistic is known in advance -- see
    /// [`ARC_ERROR_STATS`].
    fn arc_error(cell: &str) -> ArcError {
        let original = Array2::from_shape_vec((1, 2), vec![2.0, 2.0]).unwrap();
        let error = Array2::from_shape_vec((1, 2), vec![-7.0, 1.0]).unwrap();
        ArcError {
            cell: cell.to_owned(),
            source: "D".to_owned(),
            output: "Q".to_owned(),
            edge: "rise",
            when: Some("(C0)".to_owned()),
            reconstructed: &original + &error,
            original,
            error,
        }
    }

    fn ref_arc() -> RefArc {
        ref_arc_at(Anchor::Middle)
    }

    fn ref_arc_at(anchor: Anchor) -> RefArc {
        RefArc {
            col: 1,
            row: 0,
            related_pin: "G".to_owned(),
            lut_template: "T".to_owned(),
            anchor,
            rise: Some(EdgeRef {
                delay: Array1::from(vec![1.0, 2.0]),
                transition: Array1::from(vec![0.1, 0.2]),
                crossing: 2.0,
            }),
            fall: Some(EdgeRef {
                delay: Array1::from(vec![1.5, 2.5]),
                transition: Array1::from(vec![0.11, 0.21]),
                crossing: 2.5,
            }),
        }
    }

    /// A report for one pin pair characterised under several `when` conditions,
    /// each a two-point rise table, reduced to their elementwise mean -- what
    /// [`WhenMerge::Mean`] leaves the model holding. The conditions are the knob:
    /// their magnitudes set the spread the reduction has to stand in for.
    fn cell_report(conditions: &[[f64; 2]]) -> CellReport {
        let table = |v: [f64; 2]| Array2::from_shape_vec((1, 2), v.to_vec()).unwrap();
        let n = conditions.len() as f64;
        let mean = [
            conditions.iter().map(|c| c[0]).sum::<f64>() / n,
            conditions.iter().map(|c| c[1]).sum::<f64>() / n,
        ];

        // The class ids come from the real classifier over the fixture's own
        // conditions, rather than from numbers written here: `ClassId` is opaque,
        // and widening it so a test could build one would be widening visibility to
        // suit a test.
        let whens: Vec<String> = (0..conditions.len()).map(|i| format!("(C{})", i)).collect();
        let parsed: Vec<Condition> = whens
            .iter()
            .map(|w| Condition::parse(w).expect("a parenthesised pin name is a condition"))
            .collect();
        let ids = collision_classes(&parsed);

        let raw_arcs: Vec<ConditionedArc> = conditions
            .iter()
            .enumerate()
            .map(|(i, v)| ConditionedArc {
                source: "D".to_owned(),
                output: "Q".to_owned(),
                when: Some(whens[i].clone()),
                sense: TimingSense::Positive,
                // The classifier's own answer over these conditions. Which ids it
                // mints decides nothing here: every scope this fixture is rendered
                // at is `Scope::Whole`, which `in_scope` accepts without reading a
                // class at all.
                class_rise: Some(ids[i]),
                class_fall: None,
                // One check per pin at a whole-output scope, so no arc carries a
                // check class of its own.
                check_class: None,
                cell_rise: Some(table(*v)),
                cell_fall: None,
            })
            .collect();

        let classes: Vec<StateClass> = parsed
            .iter()
            .zip(whens.iter())
            .map(|(condition, when)| StateClass {
                output: "Q".to_owned(),
                edge: Transition::Rise,
                condition: Some(condition.liberty()),
                members: vec![("D".to_owned(), Some(when.clone()))],
            })
            .collect();

        CellReport {
            library: "testlib".to_owned(),
            cell: "DUT".to_owned(),
            when_merge: WhenMerge::Mean,
            raw_arcs,
            // Positive unate, so the rise tables were contributed by an input rise.
            constraint_arcs: BTreeMap::from([(
                ConstraintKey {
                    src: "D".to_owned(),
                    outpin: "Q".to_owned(),
                    delay: Scope::Whole,
                    check: Scope::Whole,
                    input_edge: Transition::Rise,
                    family: "cell_rise",
                },
                table(mean),
            )]),
            ref_arcs: BTreeMap::from([(("Q".to_owned(), Scope::Whole), ref_arc())]),
            mean_ref: ref_arc(),
            setup_input_rise: BTreeMap::from([(
                ("D".to_owned(), Scope::Whole),
                Array1::from(vec![0.5, 0.6]),
            )]),
            setup_input_fall: BTreeMap::new(),
            hold_input_rise: BTreeMap::from([(
                ("D".to_owned(), Scope::Whole),
                Array1::from(vec![-0.5, -0.6]),
            )]),
            hold_input_fall: BTreeMap::new(),
            slews: None,
            loads: None,
            arcs: vec![arc_error("DUT")],
            classes,
            // One reference for the whole output, so no state is named and no check
            // is grouped under a condition.
            class_conditions: BTreeMap::new(),
            check_classes: Vec::new(),
            check_conditions: BTreeMap::new(),
        }
    }

    /// The same report drawn per state: one output characterised in two states, each
    /// with its own reference, its own merged arc and its own constraint.
    ///
    /// The two states are an order of magnitude apart, so a section captioned with
    /// the wrong state, or a figure measured against the wrong state's members, is
    /// visible rather than plausible.
    fn per_state_report() -> CellReport {
        let table = |v: f64| Array2::from_shape_vec((1, 2), vec![v, v]).unwrap();

        // Real class ids from the classifier over the fixture's own conditions:
        // `ClassId` is opaque, and widening it so a test could write one down would
        // be widening visibility to suit a test. The two are the two values of one
        // pin, so they cannot hold at once and are two states -- conditions that
        // could hold at once would collide into one, which is a fixture with one
        // state in it and nothing per-state to render.
        let whens = ["(C0)", "(!C0)"];
        let parsed: Vec<Condition> = whens
            .iter()
            .map(|w| Condition::parse(w).expect("a parenthesised literal is a condition"))
            .collect();
        let ids = collision_classes(&parsed);

        let refarc = |delay: f64| RefArc {
            col: 1,
            row: 0,
            related_pin: "G".to_owned(),
            lut_template: "T".to_owned(),
            anchor: Anchor::Middle,
            // Each state was characterised on the rise edge alone, which is the
            // ordinary shape a conditioned arc comes in.
            rise: Some(EdgeRef {
                delay: Array1::from(vec![delay, delay + 1.0]),
                transition: Array1::from(vec![0.1, 0.2]),
                crossing: delay + 1.0,
            }),
            fall: None,
        };

        let mut report = cell_report(&[[10.0, 10.0]]);
        report.raw_arcs = whens
            .iter()
            .zip(ids.iter())
            .zip([10.0, 30.0])
            .map(|((when, class), value)| ConditionedArc {
                source: "D".to_owned(),
                output: "Q".to_owned(),
                when: Some((*when).to_owned()),
                sense: TimingSense::Positive,
                class_rise: Some(*class),
                class_fall: None,
                // The two partitions have the same shape on a cell of one pin and one
                // output -- two conditions that cannot hold at once are two states and
                // two check groups -- so they number their classes alike, which is
                // what the engine mints for a cell of this shape.
                check_class: Some(*class),
                cell_rise: Some(table(value)),
                cell_fall: None,
            })
            .collect();
        // One member per state, so each state's merged arc IS that member's table.
        let key = |class, value| {
            (
                ConstraintKey {
                    src: "D".to_owned(),
                    outpin: "Q".to_owned(),
                    delay: Scope::State(class),
                    check: Scope::State(class),
                    input_edge: Transition::Rise,
                    family: "cell_rise",
                },
                table(value),
            )
        };
        report.constraint_arcs = BTreeMap::from([key(ids[0], 10.0), key(ids[1], 30.0)]);
        report.ref_arcs = BTreeMap::from([
            (("Q".to_owned(), Scope::State(ids[0])), refarc(1.0)),
            (("Q".to_owned(), Scope::State(ids[1])), refarc(11.0)),
        ]);
        report.setup_input_rise = BTreeMap::from([
            (
                ("D".to_owned(), Scope::State(ids[0])),
                Array1::from(vec![0.5, 0.6]),
            ),
            (
                ("D".to_owned(), Scope::State(ids[1])),
                Array1::from(vec![0.7, 0.8]),
            ),
        ]);
        report.hold_input_rise = report
            .setup_input_rise
            .iter()
            .map(|(k, v)| (k.clone(), v.clone() * -1.0))
            .collect();
        // A state is the source `when` conjoined with the direction the input settled
        // in, and a check is stated under that `when` alone -- so the two partitions
        // are spelled apart here as the engine spells them apart, and a caption drawn
        // from the wrong one of them is visible rather than a coincidence of the
        // fixture. The arcs are all `D` rising, so every state conjoins `D`.
        report.class_conditions = ids
            .iter()
            .zip(parsed.iter())
            .map(|(class, condition)| (*class, format!("{} * D", condition.liberty())))
            .collect();
        report.check_classes = whens
            .iter()
            .map(|when| CheckClass {
                pin: "D".to_owned(),
                condition: Some((*when).to_owned()),
                members: vec!["Q".to_owned()],
            })
            .collect();
        report.check_conditions = ids
            .iter()
            .zip(parsed.iter())
            .map(|(class, condition)| (("D".to_owned(), *class), condition.liberty()))
            .collect();
        report
    }

    // --- g / dump ----------------------------------------------------------

    /// %g, not Rust's own float Display: six significant digits and no trailing
    /// zeros, which is what keeps the tables readable.
    ///
    /// Killed by: `g` formatted the f64 with Rust's `{}` instead of `GPoint`.
    #[test]
    fn g_renders_floats_in_printf_g_form() {
        assert_eq!(g(4.0), "4");
        assert_eq!(g(1.0 / 3.0), "0.333333");
    }

    /// Killed by: `dump` iterated `a.lanes(Axis(0))` instead of `a.rows()`, transposing every 2-D table.
    #[test]
    fn dump_writes_the_label_then_one_table_row_per_array_row() {
        let a = Array2::from_shape_vec((2, 2), vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        let out = rendered(|s| dump(s, "mean rise arc D -> Q:", &a, Labels::default()));

        assert_eq!(out.lines().next(), Some("mean rise arc D -> Q:"));
        assert_eq!(table_rows(&out), vec![vec!["1", "2"], vec!["3", "4"]]);
    }

    /// A 1-D arc -- a reference or a setup constraint -- is one row, not a column.
    ///
    /// Killed by: `dump` special-cased `a.ndim() == 1`, emitting one single-cell row per
    /// element instead of one row holding all of them. Observed to redden this test
    /// alone: `dump_writes_the_label_then_one_table_row_per_array_row` stays green under
    /// it, because the two-dimensional path is left untouched.

    #[test]
    fn dump_renders_a_one_dimensional_array_as_a_single_row() {
        let a = Array1::from(vec![1.0, 2.0, 3.0]);
        let out = rendered(|s| dump(s, "setup rise arc D:", &a, Labels::default()));

        assert_eq!(out.lines().next(), Some("setup rise arc D:"));
        assert_eq!(table_rows(&out), vec![vec!["1", "2", "3"]]);
    }

    // --- dump: a line that could not be stored is not a success ------------

    /// A sink that fails the first write carrying some nominated text and accepts
    /// every other byte.
    ///
    /// The write is named by what it contains rather than by a call index, because
    /// one `writeln!` becomes however many `write` calls the formatting machinery
    /// chooses -- an index would be a number read off a run rather than one derived
    /// from what the renderer emits.
    ///
    /// A sink that failed *every* write would redden both tests below at once, and
    /// so could not show that any particular site propagates its own error.
    struct FailOn {
        needle: &'static str,
        failed: bool,
    }

    impl FailOn {
        fn carrying(needle: &'static str) -> Self {
            FailOn {
                needle,
                failed: false,
            }
        }
    }

    impl Write for FailOn {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if !self.failed && String::from_utf8_lossy(buf).contains(self.needle) {
                self.failed = true;
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "no space left on device",
                ));
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The label carries no digit and the table carries nothing else, so each test
    /// below can name its own write without naming the other's.
    const LABEL: &str = "mean rise arc:";

    /// Killed by: `dump`'s label write restored to `let _ = writeln!(..)`, which
    /// leaves the table write to succeed and `dump` to report Ok. The table test
    /// below stays green under that, so this pins the label write alone.
    #[test]
    fn dump_reports_a_label_that_could_not_be_stored() {
        let a = Array2::from_shape_vec((2, 2), vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        let mut sink = FailOn::carrying(LABEL);

        let err = dump(&mut sink, LABEL, &a, Labels::default())
            .expect_err("a report line that could not be stored is not a line that was written");

        // The error is the sink's own, raised as it was, rather than some unrelated
        // failure that happens to also be an Err.
        assert_eq!(
            err.downcast_ref::<io::Error>().expect("io error").kind(),
            io::ErrorKind::StorageFull
        );
    }

    /// Killed by: `dump`'s table write restored to `let _ = writeln!(..)`, so the
    /// label wrote fine and `dump` reported Ok having lost the entire table. The
    /// label test above stays green under that, so this pins the table write alone.
    ///
    /// The remaining ten sites are carried by the signature change rather than by a
    /// test each: with these three renderers returning Result, a discarded write at any
    /// of them does not compile.
    #[test]
    fn dump_reports_a_table_that_could_not_be_stored() {
        let a = Array2::from_shape_vec((2, 2), vec![1.0, 2.0, 3.0, 4.0]).unwrap();
        // A table value. `LABEL` holds no digit, so this cannot match the label write.
        let mut sink = FailOn::carrying("3");

        let err = dump(&mut sink, LABEL, &a, Labels::default())
            .expect_err("a table that could not be stored is not a table that was written");

        assert_eq!(
            err.downcast_ref::<io::Error>().expect("io error").kind(),
            io::ErrorKind::StorageFull
        );
    }

    // --- stats_of / stat_line ----------------------------------------------

    /// err [3, -3, 3, -3] against a reference of magnitude 2: bias 0, sd 3,
    /// rms sqrt(36/4) = 3, min -3, max 3, rms/scale 3/2.
    ///
    /// Killed by: `stats_of` dropped the `.sqrt()` from `sd`.
    #[test]
    fn stats_of_reports_the_residuals_own_statistics() {
        let err = Array2::from_shape_vec((1, 4), vec![3.0, -3.0, 3.0, -3.0]).unwrap();
        let reference = Array2::from_shape_vec((1, 4), vec![2.0, 2.0, 2.0, 2.0]).unwrap();

        assert_eq!(
            stats_of(&err, &reference),
            "stats: n 4  scale 2  bias 0  sd 3  rms 3  min -3  max 3  rms/scale 150.00%"
        );
    }

    /// A residual against an all-zero reference has no scale to be a percentage
    /// of, and must read as 0% rather than as a division by zero.
    ///
    /// Killed by: `stats_of` divided by `scale` unconditionally, dropping the zero-scale guard.
    #[test]
    fn stats_of_reports_zero_percent_when_the_reference_has_no_scale() {
        let err = Array2::from_shape_vec((1, 2), vec![1.0, -1.0]).unwrap();
        let reference = Array2::from_shape_vec((1, 2), vec![0.0, 0.0]).unwrap();

        assert_eq!(
            stats_of(&err, &reference),
            "stats: n 2  scale 0  bias 0  sd 1  rms 1  min -1  max 1  rms/scale 0.00%"
        );
    }

    /// The per-arc line reports the arc's own scale, bias, rms, |max| and relative
    /// rms -- the quantities [`ArcError`] exposes.
    ///
    /// Killed by: `stat_line` printed `a.rms()` in the `|max|` column.
    #[test]
    fn stat_line_reports_the_arcs_statistics() {
        assert_eq!(stat_line(&arc_error("DUT")), ARC_ERROR_STATS);
    }

    // --- write_summary -----------------------------------------------------

    /// `write_summary` is handed a list of arcs and must not depend on their order, so a
    /// cell whose arcs are not adjacent is still rolled up once. Dropping only adjacent
    /// repeats would roll it up twice.
    ///
    /// The arcs are supplied directly here rather than produced by a run: the point is
    /// what this function does with an ordering, not which orderings the engine can
    /// produce. A Liberty file holds one library block, so today the engine emits one
    /// contiguous run per cell — this pins that the rollup does not rest on that.
    ///
    /// Killed by: `write_summary` used itertools' `.dedup()` instead of `.unique()` on the cell list.
    #[test]
    fn write_summary_rolls_up_each_cell_once_even_when_its_arcs_are_not_adjacent() {
        let first = arc_error("cellA");
        let other = arc_error("cellB");
        let again = arc_error("cellA");
        let arcs: Vec<&ArcError> = vec![&first, &other, &again];

        let out = rendered(|s| write_summary(s, &arcs));

        assert_eq!(rollup_names(&out), vec!["cellA", "cellB", "ALL"]);
        let cell_a = out
            .lines()
            .find(|l| l.starts_with("cellA"))
            .expect("a rollup line for cellA");
        assert!(
            cell_a.contains("arcs   2"),
            "both of cellA's arcs must be rolled up together: {}",
            cell_a
        );
    }

    /// The rollup order is first appearance, not alphabetical: sorting would
    /// reorder the report and read as a regression when two runs are compared.
    ///
    /// Killed by: `write_summary` sorted the cell list before `.unique()`.
    #[test]
    fn write_summary_keeps_the_cell_rollups_in_first_appearance_order() {
        let last_alphabetically = arc_error("zcell");
        let first_alphabetically = arc_error("acell");
        let arcs: Vec<&ArcError> = vec![&last_alphabetically, &first_alphabetically];

        let out = rendered(|s| write_summary(s, &arcs));

        assert_eq!(rollup_names(&out), vec!["zcell", "acell", "ALL"]);
    }

    // --- dump_cell / dump_reduction ----------------------------------------

    /// Every table the cell was reduced to gets a labelled section, and each
    /// measured arc gets its original, reconstruction, residual and statistics.
    ///
    /// The list below is what [`cell_report`] holds, section by section, not a
    /// transcript of a run: the cell heading; one mean arc label per entry of
    /// `constraint_arcs` (one, D -> Q, `cell_rise` under an input rise); a rise and
    /// a fall label per entry of `ref_arcs` (one output, Q, referenced from G at
    /// row 0); the mean reference pair; one label per entry of `setup_input_rise`
    /// (one source, D) and of `setup_input_fall` (none); then, per measured
    /// arc, its heading and the four tables it is compared through — the arc as
    /// characterised, its reconstruction, the residual and that residual point by
    /// point as a percentage — closed by the statistics line. A section that
    /// appeared for a map the report holds nothing in, or went missing for one it
    /// does, is what this catches.
    ///
    /// Killed by: `dump_cell` iterated `r.ref_arcs.iter().take(0)`, dropping the per-output reference sections.
    #[test]
    fn dump_cell_writes_a_labelled_section_for_every_table_it_holds() {
        let out = rendered(|s| dump_cell(s, &cell_report(&[[9.0, 3.0], [11.0, 17.0]])));

        assert_eq!(
            prose_lines(&out),
            vec![
                "cell DUT of library testlib",
                // Both keys of the merged table: the family, which names the
                // output's direction, and the input's own direction.
                "mean cell_rise arc D -> Q (input rise):",
                "ref rise arc G -> Q (row 0):",
                "ref fall arc G -> Q (row 0):",
                "mean ref rise arc (col 1, row 0):",
                "mean ref fall arc:",
                // The constraint is captioned by the constrained pin's transition,
                // which is what `rise_constraint` is keyed on.
                "setup arc D (input rise):",
                "rise arc D -> Q  when (C0)",
                "original:",
                "reconstructed:",
                "error:",
                "error % of original:",
                ARC_ERROR_STATS,
            ]
        );
    }

    /// A fall arc is compared exactly as a rise arc is.
    ///
    /// The conversion splits both edges and the report has to account for both, so
    /// nothing here may be reachable only through `cell_rise`. This renders a cell
    /// whose only residual is on the fall edge and requires the same four tables,
    /// labelled for that edge.
    ///
    /// The normalised table is checked against a quotient computed here rather than
    /// read off the renderer: an original of [40, 50] carrying a residual of
    /// [-2, 5] is -5% and +10% of the arc at those two points.
    ///
    /// Killed by: `dump_cell` iterated `r.arcs.iter().filter(|a| a.edge == "rise")`,
    /// which leaves the rise-only section-list test green.
    #[test]
    fn dump_cell_compares_a_fall_arc_the_same_way_as_a_rise_arc() {
        let original = Array2::from_shape_vec((1, 2), vec![40.0, 50.0]).unwrap();
        let error = Array2::from_shape_vec((1, 2), vec![-2.0, 5.0]).unwrap();
        let mut r = cell_report(&[[9.0, 3.0], [11.0, 17.0]]);
        r.arcs = vec![ArcError {
            cell: "DUT".to_owned(),
            source: "M".to_owned(),
            output: "Q".to_owned(),
            edge: "fall",
            when: None,
            reconstructed: &original + &error,
            original,
            error,
        }];

        let out = rendered(|s| dump_cell(s, &r));

        assert!(
            prose_lines(&out).contains(&"fall arc M -> Q  unconditioned"),
            "{}",
            out
        );
        for table in [
            "original:",
            "reconstructed:",
            "error:",
            "error % of original:",
        ] {
            assert!(
                prose_lines(&out).contains(&table),
                "{} missing:\n{}",
                table,
                out
            );
        }
        // -2/40 and 5/50 as percentages, the last table drawn.
        assert_eq!(
            table_rows(&out).last().expect("four tables were drawn"),
            &vec!["-5", "10"]
        );
    }

    /// A table is captioned with the slew and load it was measured at, and a
    /// mismatched axis is dropped rather than misaligned.
    ///
    /// The point of the caption is to say which regime a residual sits in, so it has to
    /// be the axis the table is actually indexed on. An axis of the wrong length would
    /// caption every value with a slew or load it was not measured at — a silently wrong
    /// number — so it is not used at all.
    ///
    /// Killed by: `fitted` compared the row axis against the column count, so a 2x3
    /// table accepted a 3-point slew axis and labelled its two rows with the first two
    /// of three slews.
    #[test]
    fn a_table_is_captioned_with_the_axes_it_is_indexed_on() {
        let a = Array2::from_shape_vec((2, 3), vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]).unwrap();
        let slews = vec![0.01, 0.1];
        let loads = vec![0.005, 0.05, 0.5];

        let out = rendered(|s| dump(s, "error:", &a, Labels::grid(Some(&slews), Some(&loads))));

        assert_eq!(
            table_rows(&out),
            vec![
                vec!["slew \\ load", "0.005", "0.05", "0.5"],
                vec!["0.01", "1", "2", "3"],
                vec!["0.1", "4", "5", "6"],
            ]
        );

        // A slew axis of the wrong length captions nothing; the load axis still does,
        // and no corner is drawn because there is no row label to name.
        let wrong = vec![0.01, 0.1, 1.0];
        let out = rendered(|s| dump(s, "error:", &a, Labels::grid(Some(&wrong), Some(&loads))));

        assert_eq!(
            table_rows(&out),
            vec![
                vec!["0.005", "0.05", "0.5"],
                vec!["1", "2", "3"],
                vec!["4", "5", "6"],
            ]
        );
    }

    /// An arc whose relative error is known point by point, for the summary tests.
    fn rel_arc(
        cell: &str,
        output: &str,
        edge: &'static str,
        original: Vec<f64>,
        error: Vec<f64>,
    ) -> ArcError {
        let n = original.len();
        let original = Array2::from_shape_vec((1, n), original).unwrap();
        let error = Array2::from_shape_vec((1, n), error).unwrap();
        ArcError {
            cell: cell.to_owned(),
            source: "D".to_owned(),
            output: output.to_owned(),
            edge,
            when: None,
            reconstructed: &original + &error,
            original,
            error,
        }
    }

    /// The opening summary groups the relative error by cell, then output, then
    /// direction, and each level covers exactly the points beneath it.
    ///
    /// Four arcs, every relative error computed here rather than read off the
    /// renderer. Cell A output Q1 rise: residual [1, -2] on an original of [10, 10],
    /// so [10, -20]. A/Q1 fall: [3] on [10], so [30]. A/Q2 rise: [-1] on [100], so
    /// [-1]. Cell B output Z rise: [5] on [50], so [10]. Hence A/Q1/rise spans -20
    /// to 10 averaging -5; A/Q1 adds the fall point for -20 to 30 averaging 20/3;
    /// cell A adds Q2 for -20 to 30 averaging 19/4; and ALL adds cell B for -20 to
    /// 30 averaging 29/5.
    ///
    /// The percentiles are nearest-rank over the same sorted values, so for cell A's
    /// four points sorted to [-20, -1, 10, 30] the ranks are round(0.05*3)=0, round(0.5*3)=2
    /// and round(0.95*3)=3, giving p5 -20, p50 10, p95 30.
    ///
    /// Killed by: the direction level dropped `&& a.edge == direction` from its
    /// predicate, so both direction rows repeated their output's figures.
    #[test]
    fn the_opening_summary_groups_relative_error_by_cell_then_output_then_direction() {
        let arcs = [
            rel_arc("A", "Q1", "rise", vec![10.0, 10.0], vec![1.0, -2.0]),
            rel_arc("A", "Q1", "fall", vec![10.0], vec![3.0]),
            rel_arc("A", "Q2", "rise", vec![100.0], vec![-1.0]),
            rel_arc("B", "Z", "rise", vec![50.0], vec![5.0]),
        ];
        let refs: Vec<&ArcError> = arcs.iter().collect();

        let out = rendered(|s| write_relative_error_summary(s, &refs));

        assert_eq!(
            table_rows(&out),
            vec![
                vec![
                    "scope", "points", "undef", "min %", "p5 %", "p50 %", "p95 %", "max %", "avg %"
                ],
                vec!["cell A", "4", "0", "-20", "-20", "10", "30", "30", "4.75"],
                vec![
                    "output Q1",
                    "3",
                    "0",
                    "-20",
                    "-20",
                    "10",
                    "30",
                    "30",
                    "6.66667"
                ],
                vec!["rise", "2", "0", "-20", "-20", "10", "10", "10", "-5"],
                vec!["fall", "1", "0", "30", "30", "30", "30", "30", "30"],
                vec!["output Q2", "1", "0", "-1", "-1", "-1", "-1", "-1", "-1"],
                vec!["rise", "1", "0", "-1", "-1", "-1", "-1", "-1", "-1"],
                vec!["cell B", "1", "0", "10", "10", "10", "10", "10", "10"],
                vec!["output Z", "1", "0", "10", "10", "10", "10", "10", "10"],
                vec!["rise", "1", "0", "10", "10", "10", "10", "10", "10"],
                vec!["ALL", "5", "0", "-20", "-20", "10", "30", "30", "5.8"],
            ]
        );
    }

    /// A point characterised as zero has no relative error, and must not be folded
    /// in as one.
    ///
    /// Dividing by it gives an infinity, and admitting that into the aggregate would
    /// carry every statistic of the group away with it. The original [0, 10] with a
    /// residual of [5, 1] therefore reports one measured point at 10% and one
    /// unmeasurable, not a minimum or maximum of infinity.
    ///
    /// Killed by: `relative_stats` counted every point, dropping the `is_finite`
    /// test, which put `inf` in the max and NaN in the average.
    #[test]
    fn a_point_characterised_as_zero_is_counted_apart_not_averaged_in() {
        let arcs = [rel_arc("A", "Q", "rise", vec![0.0, 10.0], vec![5.0, 1.0])];
        let refs: Vec<&ArcError> = arcs.iter().collect();

        let out = rendered(|s| write_relative_error_summary(s, &refs));

        assert_eq!(
            table_rows(&out).last().expect("the ALL row is drawn"),
            &vec!["ALL", "1", "1", "10", "10", "10", "10", "10", "10"]
        );
    }

    /// The reduction is measured against each condition it replaced, condition by
    /// condition. Conditions [9, 3] and [11, 17] have mean [10, 10], magnitudes 6
    /// and 14, and residuals [1, 7] and [-1, -7]: bias +-4, sd 3, rms 5, worst 7.
    ///
    /// Killed by: `dump_reduction` accumulated `worst` with `.min` instead of `.max`.
    #[test]
    fn dump_reduction_measures_the_mean_against_every_condition_it_replaced() {
        let out = rendered(|s| dump_reduction(s, &cell_report(&[[9.0, 3.0], [11.0, 17.0]])));

        assert!(
            out.starts_with("== when-reduction error, cell DUT of library testlib ==\n"),
            "{}",
            out
        );
        // The mean's own magnitude must equal the mean of the conditions'. The
        // heading names the family the values came from and the direction the input
        // was moving in, which under a positive-unate arc agree.
        assert!(
            out.contains(
                "cell_rise arc D -> Q (input rise): mean of 2 condition(s)  |  mean scale 10  mean-of-condition scales 10\n"
            ),
            "{}",
            out
        );

        let stats_for = |when: &str| {
            out.lines()
                .find(|l| l.trim_start().starts_with(when))
                .unwrap_or_else(|| panic!("no line for condition {}: {}", when, out))
                .to_owned()
        };
        assert!(
            stats_for("(C0)").ends_with(
                "stats: n 2  scale 6  bias 4  sd 3  rms 5  min 1  max 7  rms/scale 83.33%"
            ),
            "{}",
            stats_for("(C0)")
        );
        assert!(
            stats_for("(C1)").ends_with(
                "stats: n 2  scale 14  bias -4  sd 3  rms 5  min -7  max -1  rms/scale 35.71%"
            ),
            "{}",
            stats_for("(C1)")
        );

        assert!(out.contains("  worst |mean - condition|: 7\n"), "{}", out);
    }

    /// The states the cell's arcs describe are listed after the reduction, one
    /// line per class: which output and which of its edges, the condition in
    /// Liberty's spelling, how many arcs claim it and which.
    ///
    /// Killed by: `dump_collision_classes` iterated `r.classes.iter().take(0)`, which wrote the heading and no rows.
    #[test]
    fn dump_reduction_lists_one_line_per_collision_class() {
        let out = rendered(|s| dump_reduction(s, &cell_report(&[[9.0, 3.0], [11.0, 17.0]])));

        assert!(out.contains("collision classes\n"), "{}", out);
        // Two conditions naming two different pins, so two classes, each claimed by
        // the one arc that carries it. `(C0)` renders as `C0`; the member keeps the
        // library's own spelling.
        assert!(
            out.contains(
                "  Q rise  C0                                           1 arc(s): D((C0))\n"
            ),
            "{}",
            out
        );
        assert!(
            out.contains(
                "  Q rise  C1                                           1 arc(s): D((C1))\n"
            ),
            "{}",
            out
        );
    }

    /// An arc whose `when` this tool could not read is named, rather than passed
    /// over: it belongs to no class and to no catch-all, so nothing else in the
    /// block would mention it at all.
    ///
    /// Killed by: `dump_collision_classes` dropped the delay-family test from its filter, which then also named a `when`-conditioned arc that simply carried no delay table -- an arc there was never anything to classify about.
    #[test]
    fn an_arc_whose_condition_could_not_be_read_is_named_in_the_class_block() {
        let mut report = cell_report(&[[9.0, 3.0]]);
        // What the engine leaves behind for an unreadable `when`: the arc keeps its
        // text and its tables, holds no class, and no class row mentions it.
        report.raw_arcs[0].when = Some("A'".to_owned());
        report.raw_arcs[0].class_rise = None;
        report.classes.clear();

        let out = rendered(|s| dump_reduction(s, &report));
        assert!(
            out.contains("  UNREADABLE CONDITION: arc D -> Q when A'\n"),
            "{}",
            out
        );

        // An arc that carries no delay table was never a candidate for a class, so
        // it is not reported as unreadable either.
        let mut tableless = cell_report(&[[9.0, 3.0]]);
        tableless.raw_arcs[0].class_rise = None;
        tableless.raw_arcs[0].cell_rise = None;
        tableless.classes.clear();
        assert!(
            !rendered(|s| dump_reduction(s, &tableless)).contains("UNREADABLE CONDITION"),
            "an arc with nothing to classify must not be reported as unreadable"
        );
    }

    /// Every per-state table is captioned with the condition it was filed under, and
    /// the sections that only a per-state report has are the ones it gains.
    ///
    /// The list below is what [`per_state_report`] holds, section by section, not a
    /// transcript of a run: two merged arcs, one per state; two references, each
    /// carrying the rise edge alone and its own crossing; the cell-wide mean pair;
    /// two setup constraints and their two negations; the conditions the checks were
    /// grouped under; then the measured arc and its four tables. A caption naming the
    /// wrong state -- or naming none, which is what a report with one reference per
    /// output reads like -- is what this catches.
    ///
    /// Each pin is named with the condition its OWN tables are filed under: an output
    /// with the post-settled state its delay was drawn at, `C0 * D`, and an input
    /// with the raw `when` its checks are grouped by, `C0`. The two are different
    /// partitions, so a caption drawn from the other one names a condition the table
    /// was never confined to.
    ///
    /// Killed by: `is_per_state` answered `false`, which dropped the crossing lines and the whole hold section -- the two sections a per-state report gains. Observed to redden this test alone; `dump_cell_writes_a_labelled_section_for_every_table_it_holds` stays green under it, because a report with one reference per output has neither section either way. (Making `scope_caption` say nothing for `Scope::State` reddens this and the reduction test beside it, since both read the state captions.)
    #[test]
    fn dump_cell_names_the_state_each_per_state_table_was_filed_under() {
        let out = rendered(|s| dump_cell(s, &per_state_report()));

        // `(C0)` renders as `C0`; the check block keeps the library's own spelling.
        let check_row = |when: &str| format!("  D {:<44} 1 arc(s): Q", when);
        assert_eq!(
            prose_lines(&out),
            vec![
                "cell DUT of library testlib",
                "mean cell_rise arc D [C0] -> Q [C0 * D] (input rise):",
                "mean cell_rise arc D [!C0] -> Q [!C0 * D] (input rise):",
                // Only the edge each state was characterised on, and the constant its
                // constraint half was offset by.
                "ref rise arc G -> Q [C0 * D] (row 0):",
                "crossing: 2",
                "ref rise arc G -> Q [!C0 * D] (row 0):",
                "crossing: 12",
                "mean ref rise arc (col 1, row 0):",
                "mean ref fall arc:",
                "setup arc D [C0] (input rise):",
                "setup arc D [!C0] (input rise):",
                // Hold is only worth printing once there is more than one of it.
                "hold arc D [C0] (input rise):",
                "hold arc D [!C0] (input rise):",
                "check conditions",
                "the source `when` each pin's setup and hold groups are stated under, classified per pin",
                &check_row("(C0)"),
                &check_row("(!C0)"),
                "rise arc D -> Q  when (C0)",
                "original:",
                "reconstructed:",
                "error:",
                "error % of original:",
                ARC_ERROR_STATS,
            ]
        );
    }

    /// Under a per-state reference each condition is measured against its OWN state's
    /// merged arc, not against every arc of the pin pair.
    ///
    /// The two states here were characterised at 10 and 30, and each was merged
    /// alone, so each group stands for one condition and reproduces it exactly. A
    /// group that swept in the other state's condition would report two conditions,
    /// a mean-of-condition scale of 20 against a mean scale of 10, and a residual of
    /// 20 -- a reduction error that no reduction made.
    ///
    /// Killed by: `in_scope` answered `true` for `Scope::State` as it does for `Scope::Whole`, which put both conditions in both groups and gave `mean of 2 condition(s)  |  mean scale 10  mean-of-condition scales 20`. Observed to redden this test alone; the per-class `dump_cell` test beside it stays green, because the section list does not depend on which conditions each group was measured against.
    #[test]
    fn dump_reduction_measures_each_state_against_its_own_members() {
        let out = rendered(|s| dump_reduction(s, &per_state_report()));

        for (when, scale) in [("C0", 10), ("!C0", 30)] {
            assert!(
                out.contains(&format!(
                    "cell_rise arc D [{}] -> Q [{} * D] (input rise): mean of 1 condition(s)  |  mean scale {}  mean-of-condition scales {}\n",
                    when, when, scale, scale
                )),
                "state {}: {}",
                when,
                out
            );
        }
        // Each state's merged arc IS its one member's table, so nothing was lost.
        assert_eq!(
            out.matches("  worst |mean - condition|: 0\n").count(),
            2,
            "{}",
            out
        );
    }

    /// The marker fires on the ratio between the widest and narrowest condition,
    /// and only past 2x: at exactly 2x the conditions are still close enough that
    /// the mean stands for something.
    ///
    /// Killed by: `dump_reduction`'s spread marker fired at `>= 2.0` instead of `> 2.0`.
    #[test]
    fn dump_reduction_flags_a_wide_spread_only_past_two_times() {
        let at_the_threshold =
            rendered(|s| dump_reduction(s, &cell_report(&[[1.0, 1.0], [2.0, 2.0]])));
        assert!(
            !at_the_threshold.contains("WIDE SPREAD"),
            "2x is not past 2x: {}",
            at_the_threshold
        );

        let past_it = rendered(|s| dump_reduction(s, &cell_report(&[[1.0, 1.0], [4.0, 4.0]])));
        assert!(
            past_it.contains("WIDE SPREAD: conditions range 1 .. 4 (4.0x)"),
            "{}",
            past_it
        );
    }

    // --- write_report ------------------------------------------------------

    // --- refusals ----------------------------------------------------------

    /// Every refusal reaches the report, `--report-summary-only` included.
    ///
    /// A run with skips exits 0, so the report and the standard-error warnings are the
    /// only signals it has. That flag limits the tables, and a refusal is not a table:
    /// suppressing it there would make the flag a mode in which skips vanish from the
    /// machine-readable artefact altogether, which is the defect the refusal section
    /// exists to close.
    ///
    /// Killed by: the refusals section gated on `!summary_only`, which reddens the
    /// summary-only pass of the loop below and leaves the full-report pass green.
    #[test]
    fn refusals_reach_the_report_under_summary_only_as_well() {
        let reports = vec![cell_report(&[[1.0, 1.0], [4.0, 4.0]])];
        let refusals = vec![
            Refusal {
                library: "testlib".to_owned(),
                cell: "DUT".to_owned(),
                output: Some("QN".to_owned()),
                reason: "no non-reset source supplies a complete reference".to_owned(),
            },
            Refusal {
                library: "testlib".to_owned(),
                cell: "OTHER".to_owned(),
                output: None,
                reason: "no output supplies a complete reference".to_owned(),
            },
        ];

        for summary_only in [true, false] {
            let out = rendered(|s| {
                write_report(
                    s,
                    &reports,
                    &refusals,
                    ReferenceMode::PerOutput,
                    Anchor::Middle,
                    summary_only,
                )
            });

            // An output-scope refusal names the output. A cell-scope one does not,
            // because the whole cell was left verbatim rather than one output of it.
            assert!(
                out.contains("refused: cell DUT of library testlib, output QN: no non-reset source supplies a complete reference\n"),
                "summary_only = {}: {}",
                summary_only,
                out
            );
            assert!(
                out.contains("refused: cell OTHER of library testlib: no output supplies a complete reference\n"),
                "summary_only = {}: {}",
                summary_only,
                out
            );
        }
    }

    // --- write_report: summary against full --------------------------------

    /// The summary is what the report always carries; the per-arc and per-cell
    /// tables are what `--report-summary-only` leaves out.
    ///
    /// Killed by: `write_report` guarded the per-cell sections with `if true` instead of `if !summary_only`.
    #[test]
    fn write_report_with_summary_only_omits_the_per_arc_sections() {
        let reports = vec![cell_report(&[[1.0, 1.0], [4.0, 4.0]])];
        let out = rendered(|s| {
            write_report(
                s,
                &reports,
                &[],
                ReferenceMode::PerOutput,
                Anchor::Middle,
                true,
            )
        });

        // The three knobs that decide what the numbers below mean, before any of
        // them: a report read without knowing how the reference was drawn, where
        // it was anchored and how its when-arcs were merged says nothing.
        assert!(
            out.starts_with("reference mode: PerOutput\nanchor: Middle\nwhen-arc merge: Mean\n"),
            "{}",
            out
        );
        assert_eq!(rollup_names(&out), vec!["DUT", "ALL"]);
        assert!(
            !out.contains("== when-reduction error"),
            "the reduction section is not part of the summary: {}",
            out
        );
        assert!(
            !out.contains("cell DUT of library testlib"),
            "the per-cell tables are not part of the summary: {}",
            out
        );
    }

    /// Killed by: `write_report` guarded the per-cell sections with `if false` instead of `if !summary_only`.
    #[test]
    fn write_report_in_full_adds_the_reduction_and_per_cell_sections() {
        let reports = vec![cell_report(&[[1.0, 1.0], [4.0, 4.0]])];
        let out = rendered(|s| {
            write_report(
                s,
                &reports,
                &[],
                ReferenceMode::PerOutput,
                Anchor::Middle,
                false,
            )
        });

        assert!(
            out.contains("== when-reduction error, cell DUT of library testlib ==\n"),
            "{}",
            out
        );
        assert!(out.contains("\ncell DUT of library testlib\n"), "{}", out);
    }
}
