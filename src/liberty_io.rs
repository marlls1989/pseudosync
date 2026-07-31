//! Reading a Liberty file in and writing one back out.

use liberty_parser::{ast::LibertyAst, liberty::Liberty};
use simple_error::simple_error;
use std::{
    error::Error,
    fs::File,
    io::{stderr, stdin, stdout, BufWriter, Read, Write},
    path::{Path, PathBuf},
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

/// Where one of the two artefacts is to be written.
///
/// `-` names the standard stream belonging to the artefact it was given to: standard
/// input for the input file, standard output for `--output`, standard error for
/// `--report`. Resolving that on the way in, rather than at the moment of writing,
/// is what lets the library's destination and the report's be compared for collision
/// while both files are still whatever they were.
pub(crate) enum Destination {
    Stdout,
    Stderr,
    File(PathBuf),
}

impl Destination {
    /// Where the converted library goes. No `--output` at all, or `-`, means
    /// standard output; `-` never names a file called `-`.
    pub(crate) fn for_output(path: Option<&Path>) -> Self {
        match path {
            None => Destination::Stdout,
            Some(path) if path.as_os_str() == "-" => Destination::Stdout,
            Some(path) => Destination::File(path.to_path_buf()),
        }
    }

    /// Where the reconstruction report goes. `-` means standard error.
    pub(crate) fn for_report(path: &Path) -> Self {
        if path.as_os_str() == "-" {
            Destination::Stderr
        } else {
            Destination::File(path.to_path_buf())
        }
    }

    /// Open the destination for writing. A file is buffered, and truncated if it
    /// already exists -- appending across runs would make it impossible to tell
    /// which run a table came from.
    pub(crate) fn open(&self) -> Result<Box<dyn Write>, Box<dyn Error>> {
        Ok(match self {
            Destination::Stdout => Box::new(stdout()),
            Destination::Stderr => Box::new(stderr()),
            Destination::File(path) => Box::new(BufWriter::new(File::create(path)?)),
        })
    }

    /// Whether these two destinations are one destination, in which case whichever
    /// artefact is written second would destroy the first.
    ///
    /// Only two files can collide: the standard streams are distinct from each other
    /// and from every file. Sameness is decided on the resolved path -- the parent
    /// directory as the filesystem resolves it, plus the final component -- so
    /// `out.lib` and `./out.lib` are recognised as one destination. Paths that alias
    /// the same file by other means, a symbolic link among them, are not detected.
    pub(crate) fn collides_with(&self, other: &Destination) -> bool {
        match (self, other) {
            (Destination::File(ours), Destination::File(theirs)) => {
                resolved(ours) == resolved(theirs)
            }
            _ => false,
        }
    }
}

/// A path reduced to what the collision test compares. The parent is canonicalised
/// where it can be, and left as written where it cannot -- which is the ordinary case
/// for a directory that does not exist yet, and where two spellings of the same path
/// are then still caught by comparing them as given.
fn resolved(path: &Path) -> PathBuf {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let base = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());

    match path.file_name() {
        Some(name) => base.join(name),
        None => base,
    }
}

/// Write a Liberty AST to the given destination
pub(crate) fn write_liberty_file(
    destination: &Destination,
    liberty: &LibertyAst,
) -> Result<(), Box<dyn Error>> {
    let mut output_stream = destination.open()?;

    write_liberty(&mut *output_stream, liberty)
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

    // --- Destination -------------------------------------------------------

    /// Killed by: `for_output` lost its `-` arm, so `Some("-")` resolved to
    /// `File("-")` and the library was written to a file literally named `-`.
    #[test]
    fn a_dash_names_the_standard_stream_belonging_to_the_artefact() {
        // `-` is not a filename. Given to `--output` it means standard output, given
        // to `--report` it means standard error: in each case the standard stream
        // belonging to that artefact. Nothing may turn it into a file called `-`.
        assert!(matches!(Destination::for_output(None), Destination::Stdout));
        assert!(matches!(
            Destination::for_output(Some(Path::new("-"))),
            Destination::Stdout
        ));
        assert!(matches!(
            Destination::for_report(Path::new("-")),
            Destination::Stderr
        ));

        // Any other path is a file, for either artefact.
        assert!(matches!(
            Destination::for_output(Some(Path::new("/nonexistent/out.lib"))),
            Destination::File(_)
        ));
        assert!(matches!(
            Destination::for_report(Path::new("/nonexistent/run.rpt")),
            Destination::File(_)
        ));
    }

    /// Killed by: `collides_with` compared the two paths as written instead of
    /// resolving them, so the two spellings below read as different destinations.
    #[test]
    fn destinations_collide_exactly_when_they_resolve_to_one_file() {
        let dir = TempDir::new().expect("create temp dir");

        // Two spellings of one file. Whichever artefact were written second would
        // destroy the first, so they must be recognised as a single destination
        // however the caller happened to write the path.
        //
        // The spelling has to be one that Rust's own path equality does not already
        // equate, or this pins nothing: `Path` compares by components, which
        // normalises a `.` away but keeps `..`. So the pair below differs as plain
        // paths and only agrees once the parent is resolved against the filesystem.
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).expect("create sub directory");
        let direct = dir.path().join("both.out");
        let traversed = sub.join("..").join("both.out");
        assert_ne!(
            direct, traversed,
            "the two spellings must differ as plain paths, or the resolution is untested"
        );

        let plain = Destination::File(direct);
        let indirect = Destination::File(traversed);
        assert!(plain.collides_with(&indirect));
        assert!(indirect.collides_with(&plain));

        // Distinct files do not collide, or no run could produce two artefacts.
        let other = Destination::File(dir.path().join("other.out"));
        assert!(!plain.collides_with(&other));

        // The standard streams are distinct from each other and from every file:
        // writing the library to standard output and the report to standard error
        // is the default arrangement and must never be refused.
        assert!(!Destination::Stdout.collides_with(&Destination::Stderr));
        assert!(!Destination::Stdout.collides_with(&plain));
        assert!(!plain.collides_with(&Destination::Stdout));
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
        timing_sense : positive_unate;
        timing_type: combinational;
        cell_rise(T) { values("0.1, 0.2", "0.3, 0.4"); }
        cell_fall(T) { values("0.11, 0.21", "0.31, 0.41"); }
      }
      timing() {
        related_pin: "G";
        timing_sense : positive_unate;
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
        timing_sense : positive_unate;
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

        write_liberty_file(
            &Destination::File(output_path.clone()),
            &liberty.clone().to_ast(),
        )
        .expect("write output");
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
