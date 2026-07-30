//! Behaviour of the `engine` module: constraint calculation and bundle traversal.

use super::*;

// --- ReferenceMode::from_str --------------------------------------------

#[test]
fn reference_mode_from_str_maps_each_spelling() {
    assert_eq!("pooled".parse::<ReferenceMode>(), Ok(ReferenceMode::Pooled));
    assert_eq!(
        "per-output".parse::<ReferenceMode>(),
        Ok(ReferenceMode::PerOutput)
    );

    let err = "bogus".parse::<ReferenceMode>().unwrap_err();
    assert!(
        err.contains("unknown reference mode"),
        "error message was {:?}",
        err
    );
}

// --- calculate_setup / hold constraints --------------------------------

#[test]
fn setup_constraint_is_input_arc_minus_reference_delay() {
    // One input->output rise arc for source pin "D"; column `col` is what
    // the reference samples. ref.cell_rise[col] is subtracted off.
    let col = 1usize;
    // 3x3 arc whose column 1 is [25, 35, 45]
    let arc = Array2::from_shape_vec((3, 3), vec![0.0, 25.0, 0.0, 0.0, 35.0, 0.0, 0.0, 45.0, 0.0])
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
    let ref_arcs: BTreeMap<String, RefArc> = BTreeMap::from([("Q".to_owned(), ref_arc.clone())]);

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

/// Delegating to the members is not the same as constraining all of them: a
/// member takes constraints only if the library characterised it.
///
/// `D1` drives both outputs, so its name is harvested from the arcs and it has a
/// constraint to take. `D2` is declared, is a real input and is a member of the
/// same bundle, but no arc ever names it -- there is no delay to subtract a
/// reference from, so it must come through with nothing at all. An empty setup
/// pair on it would be timing no tool can use, and the marker alone would claim
/// a data input the library never characterised.
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
    process_library(&mut lib[0], "G", &Regex::new("(R|S)N?").unwrap(), false);
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
    process_library(&mut lib[0], "G", &Regex::new("(R|S)N?").unwrap(), latch);
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
#[test]
fn latch_mode_appends_the_pseudo_arc_to_the_original_ones() {
    let lib = converted_latch_cell(true);
    let cell = lib[0].get_cell("LATCH_CELL").expect("LATCH_CELL");
    let q = cell.get_pin("Q").expect("Q");

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
#[test]
fn ff_mode_strips_every_output_arc_the_reset_does_not_own() {
    let lib = converted_latch_cell(false);
    let cell = lib[0].get_cell("LATCH_CELL").expect("LATCH_CELL");
    let q = cell.get_pin("Q").expect("Q");

    // The data arc A -> Q is gone; the reset arc stays, because a flip-flop
    // still needs its asynchronous clear characterised.
    assert_eq!(
        arc_census(q),
        census(&[("RN", "clear", 1), ("G", "rising_edge", 1)])
    );
}

/// Flip-flop mode renames the latch's control attributes to their flip-flop
/// spellings, keeping the expressions themselves.
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

    process_library(&mut lib[0], "G", &Regex::new("(R|S)N?").unwrap(), false);
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

// --- constraint arithmetic at full characterisation size ----------------

/// The emitted setup constraint on a full-size 10x10 characterisation is the
/// arc sampled at the reference column, minus the clock-to-output delay the
/// pseudo-flop model charges for that path; hold is its negation.
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

    process_library(&mut liberty[0], "CLK", &Regex::new("RST").unwrap(), false);

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

        process_library(&mut liberty[0], "G", &reset_name, false);

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
// These replace the golden-file comparisons the old suite ran against the
// committed `_pseudoflop.lib` and `_pseudolatch.lib` outputs. A golden file
// either passes vacuously or fails on every intended change, so the coverage
// is restated as semantic assertions and the golden files are not read.
//
// The paths go through `CARGO_MANIFEST_DIR` rather than being relative,
// because `engine_does_not_leak_pseudosync_txt_in_cwd` above changes the
// process working directory and shares this test binary.

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
    assert_eq!(
        arc_census(cell.get_pin("Q").expect("Q")),
        racelem21x1_original_q()
    );

    let reports = process_library_with_reference(
        &mut liberty[0],
        ASCEND_CLOCK,
        &reset_name,
        false,
        ReferenceMode::PerOutput,
        WhenMerge::Mean,
    );
    let report = reports
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
    // arc in place of the 28 data arcs it dropped.
    let cell = liberty[0].get_cell("RACELEM21X1").expect("RACELEM21X1");
    assert_eq!(
        arc_census(cell.get_pin("Q").expect("Q")),
        census(&[
            ("G", "rising_edge", 1),
            ("RN", "clear", 29),
            ("RN", "preset", 5),
        ])
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
    assert_eq!(
        arc_census(rn),
        census(&[("RN", "min_pulse_width", 24)]),
        "the reset pin's own arcs must be left alone"
    );
}

/// The whole pseudo-flop conversion of the real ASCEND library.
///
/// Replaces `test_ascend_freepdk45_comprehensive_comparison`, which diffed the
/// output against a committed 121k-line golden file. The counts below were
/// derived from the fixture itself (see the census helpers above) and pin the
/// present behaviour.
#[test]
fn ascend_ff_mode_emits_the_pseudo_flop_model_for_every_qualifying_cell() {
    let mut liberty =
        parse_liberty_file(Path::new(ASCEND_FF_1V25)).expect("parse the ASCEND ff library");
    assert_eq!(liberty.len(), 1, "the fixture holds one library");
    let reset_name = Regex::new(ASCEND_RESET).unwrap();

    let qualifying = qualifying_cells(&liberty[0]);
    assert_eq!(
        qualifying.len(),
        26,
        "cells qualifying for conversion: {:?}",
        qualifying
    );

    // Q's arc population before the run, so what is retained afterwards can be
    // counted against what there was to keep.
    assert_eq!(
        arc_census(output_pin(&liberty[0], "RACELEM21X1", "Q")),
        racelem21x1_original_q()
    );
    assert_eq!(
        arc_census(output_pin(&liberty[0], "LBTIEX1", "Q")),
        lbtiex1_original_q()
    );

    process_library_with_reference(
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
    assert_eq!(
        arc_census(output_pin(lib, "RACELEM21X1", "Q")),
        census(&[
            ("G", "rising_edge", 1),
            ("RN", "clear", 29),
            ("RN", "preset", 5),
        ])
    );
    assert_eq!(
        arc_census(output_pin(lib, "LBTIEX1", "Q")),
        census(&[
            ("G", "rising_edge", 1),
            ("RN", "clear", 1),
            ("RN", "preset", 1)
        ])
    );

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
    // nothing; the reset keeps only the arcs it already had.
    for (cell_name, reset_pin_arcs) in [("RACELEM21X1", 24usize), ("LBTIEX1", 0usize)] {
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

    process_library_with_reference(
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

    // The same totals, spelled out: 62 characterised arcs plus one, and 3 plus
    // one.
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
