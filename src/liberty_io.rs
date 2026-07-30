//! Reading a Liberty file in and writing one back out.

use liberty_parser::{ast::LibertyAst, liberty::Liberty};
use simple_error::simple_error;
use std::{
    error::Error,
    fs::File,
    io::{stdin, stdout, BufWriter, Read, Write},
    path::Path,
};

/// Parse a Liberty file from the given path
pub(crate) fn parse_liberty_file(path: &Path) -> Result<Liberty, Box<dyn Error>> {
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
pub(crate) fn write_liberty_file(
    path: Option<&Path>,
    liberty: &LibertyAst,
) -> Result<(), Box<dyn Error>> {
    let mut output_stream = {
        let output: Box<dyn Write> = if let Some(path) = path {
            Box::new(File::create(path)?)
        } else {
            Box::new(stdout())
        };
        BufWriter::new(output)
    };

    write_liberty(&mut output_stream, liberty)
}

/// Write a Liberty AST to an already-open sink.
///
/// The flush is not optional. A buffered sink writes nothing to its backing store
/// until the buffer fills, and the flush it performs when it drops throws its error
/// away -- so without this, a library small enough to fit the buffer would be
/// reported as written when the disk had refused every byte of it.
fn write_liberty(sink: &mut dyn Write, liberty: &LibertyAst) -> Result<(), Box<dyn Error>> {
    writeln!(sink, "{}", liberty)?;
    sink.flush()?;

    Ok(())
}

#[cfg(test)]
mod tests {
    //! Behaviour of the Liberty file readers and writers: reading a library from a
    //! path, what a write-then-reparse round trip has to preserve, and that a
    //! library which could not be stored is reported as a failure.

    use super::*;
    use liberty_parser::liberty::Group;
    use std::io;
    use tempfile::TempDir;

    // --- parse_liberty_file ------------------------------------------------

    /// Killed by: `parse_liberty_file` inverted its stdin test to `path.as_os_str() != "-"`, reading stdin for a real path.
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

    /// Killed by: `write_liberty` prefixed the emitted library with `@@@ `, so what it wrote no longer reparsed.
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

    // --- write_liberty: a library that was not stored is not a success ------

    /// A sink that accepts every byte and then fails to flush, standing in for a
    /// buffered writer whose backing store is full: the bytes reach the buffer
    /// happily, and only the flush ever discovers they cannot be stored.
    #[derive(Default)]
    struct FullDisk {
        accepted: usize,
    }

    impl Write for FullDisk {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.accepted += buf.len();
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::StorageFull,
                "no space left on device",
            ))
        }
    }

    /// Killed by: `write_liberty` swallowed the flush error -- `let _ = sink.flush();`.
    #[test]
    fn a_sink_that_cannot_be_flushed_makes_the_write_fail() {
        let liberty = liberty_parser::parse_lib(ROUND_TRIP_LIB).expect("parse fixture");
        let mut sink = FullDisk::default();

        let err = write_liberty(&mut sink, &liberty.to_ast())
            .expect_err("a library that could not be stored must not be reported as written");

        // The error has to be the sink's own, raised as it was, rather than some
        // unrelated failure that happens to also be an Err.
        assert_eq!(
            err.downcast_ref::<io::Error>().expect("io error").kind(),
            io::ErrorKind::StorageFull
        );
        // Every byte was accepted, so the write itself succeeded: the failure this
        // pins is the flush, which is the only place the loss could have surfaced.
        assert!(sink.accepted > 0, "the fixture should produce output");
    }

    // --- write_liberty: trailing newline ------------------------------------

    /// Killed by: `write_liberty` used `write!` instead of `writeln!`.
    #[test]
    fn write_liberty_ends_the_output_with_a_newline() {
        // Liberty is a line-oriented text format, and POSIX defines a text file's
        // lines as each ending in a newline -- a file that stops mid-line reads as
        // truncated to a line-oriented parser or to `diff`/`cat`, whether or not the
        // bytes before that point are complete. The last byte the writer emits must
        // therefore be b'\n', independent of what precedes it.
        let liberty = liberty_parser::parse_lib(ROUND_TRIP_LIB).expect("parse fixture");
        let mut buf: Vec<u8> = Vec::new();

        write_liberty(&mut buf, &liberty.to_ast()).expect("write to buffer");

        assert_eq!(
            buf.last().copied(),
            Some(b'\n'),
            "emitted library must end with a trailing newline"
        );
    }
}
