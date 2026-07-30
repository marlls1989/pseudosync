//! Behaviour of the `arcs` module: arc averaging, restoration, reference
//! selection and `when` merging.

use super::*;

// --- mean_timingtable --------------------------------------------------

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
        rise_trans: Array1::from(vec![0.1, 0.2, 0.3]) * scale,
        fall_trans: Array1::from(vec![0.15, 0.25, 0.35]) * scale,
        cell_rise: Array1::from(vec![1.0, 2.0, 3.0]) * scale,
        cell_fall: Array1::from(vec![1.5, 2.5, 3.5]) * scale,
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
    close(&mean.rise_trans, [0.15, 0.3, 0.45], "rise_trans");
    close(&mean.fall_trans, [0.225, 0.375, 0.525], "fall_trans");
    close(&mean.cell_rise, [1.5, 3.0, 4.5], "cell_rise");
    close(&mean.cell_fall, [2.25, 3.75, 5.25], "cell_fall");
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

#[test]
fn lut_template_falls_back_to_fall_transition_when_only_that_is_present() {
    let g = timing_group(vec![table_group("fall_transition", "FT_TPL")]);
    let tt = extract_timing_tables_from_arc(&g).expect("tables present");
    assert_eq!(tt.lut_template, "FT_TPL");
}

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
