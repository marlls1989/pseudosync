//! Behaviour of the Liberty file readers and writers: reading a library from a
//! path, and what a write-then-reparse round trip has to preserve.

use super::*;
use tempfile::TempDir;

// --- parse_liberty_file ------------------------------------------------

#[test]
fn parse_liberty_file_reads_the_library_at_the_given_path() {
    let dir = TempDir::new().expect("create temp dir");
    let path = dir.path().join("test.lib");
    std::fs::write(
        &path,
        r#"
library(test_parse) {
  delay_model: table_lookup;
  time_unit: "1ns";
  cell(TEST) {
    area: 1.0;
    pin(A) { direction: input; }
  }
}
"#,
    )
    .expect("write fixture");

    let liberty = parse_liberty_file(&path).expect("parse liberty file");

    // The whole file is one library, and its attributes are surfaced with the
    // types the rest of the crate reads them back as.
    assert_eq!(liberty.len(), 1);
    let lib = &liberty[0];
    assert_eq!(lib.name, "test_parse");
    assert_eq!(
        lib.simple_attribute("delay_model").unwrap().expr(),
        "table_lookup"
    );
    assert_eq!(lib.simple_attribute("time_unit").unwrap().string(), "1ns");

    let cell = lib.get_cell("TEST").expect("TEST cell");
    assert_eq!(cell.simple_attribute("area").unwrap().float(), 1.0);
    assert_eq!(cell.get_pin("A").expect("pin A").name, "A");
}

// --- write_liberty_file: round trip ------------------------------------

/// A library whose cells differ in how many pins they have and whose pins
/// differ in how many arcs they carry, so a writer that lost a whole family of
/// subgroups -- or emitted the same pin everywhere -- cannot go unnoticed.
const ROUND_TRIP_LIB: &str = r#"
library(io_test) {
  delay_model: table_lookup;
  time_unit: "1ns";
  lu_table_template(T) {
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }
  cell(LCELL) {
    area: 1.5;
    latch(IQ, IQN) { enable: "G"; data_in: "D"; }
    pin(D) { direction: input; capacitance: 0.01; }
    pin(G) { direction: input; clock: true; }
    pin(Q) {
      direction: output;
      function: "IQ";
      timing() {
        related_pin: "D";
        timing_type: combinational;
        cell_rise(T) { values("0.1, 0.2", "0.3, 0.4"); }
        cell_fall(T) { values("0.11, 0.21", "0.31, 0.41"); }
      }
      timing() {
        related_pin: "G";
        timing_type: rising_edge;
        cell_rise(T) { values("0.5, 0.6", "0.7, 0.8"); }
        cell_fall(T) { values("0.51, 0.61", "0.71, 0.81"); }
      }
    }
  }
  cell(COMB) {
    pin(A) { direction: input; }
    pin(Y) {
      direction: output;
      function: "A";
      timing() {
        related_pin: "A";
        timing_type: combinational;
        cell_rise(T) { values("1.0, 2.0", "3.0, 4.0"); }
      }
    }
  }
}
"#;

/// The shape of a library: every cell, and for each of its pins the number of
/// timing arcs hanging off it. This is what a file the tool wrote has to say
/// back when it is read again -- a count of libraries and a library name would
/// still match after every cell body had been dropped.
fn structure(lib: &Group) -> Vec<(String, Vec<(String, usize)>)> {
    lib.iter_cells()
        .map(|cell| {
            let pins = cell
                .iter_pins()
                .map(|pin| {
                    (
                        pin.name.clone(),
                        pin.iter_subgroups_of_type("timing").count(),
                    )
                })
                .collect();
            (cell.name.clone(), pins)
        })
        .collect()
}

/// The structure of [`ROUND_TRIP_LIB`], read off the fixture text by hand.
fn expected_structure() -> Vec<(String, Vec<(String, usize)>)> {
    [
        ("LCELL", vec![("D", 0), ("G", 0), ("Q", 2)]),
        ("COMB", vec![("A", 0), ("Y", 1)]),
    ]
    .into_iter()
    .map(|(cell, pins)| {
        (
            cell.to_owned(),
            pins.into_iter()
                .map(|(pin, arcs)| (pin.to_owned(), arcs))
                .collect(),
        )
    })
    .collect()
}

#[test]
fn write_then_reparse_preserves_the_library_structure() {
    let dir = TempDir::new().expect("create temp dir");
    let input_path = dir.path().join("input.lib");
    let output_path = dir.path().join("output.lib");
    std::fs::write(&input_path, ROUND_TRIP_LIB).expect("write fixture");

    let liberty = parse_liberty_file(&input_path).expect("parse fixture");
    assert_eq!(liberty.len(), 1);
    assert_eq!(liberty[0].name, "io_test");
    // The fixture is what it is read as, so a failure below is the round trip
    // and not a misread of the text above.
    assert_eq!(structure(&liberty[0]), expected_structure(), "fixture");

    write_liberty_file(Some(&output_path), &liberty.clone().to_ast()).expect("write output");
    assert!(output_path.exists(), "output file should be created");

    let reparsed = parse_liberty_file(&output_path).expect("reparse output");

    assert_eq!(reparsed.len(), 1);
    assert_eq!(reparsed[0].name, "io_test");
    assert_eq!(
        structure(&reparsed[0]),
        expected_structure(),
        "round trip lost structure"
    );
}
