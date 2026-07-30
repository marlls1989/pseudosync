//! What the library declares about its lookup templates.
//!
//! A characterisation table names an `lu_table_template`, and the template declares
//! the axes the table is indexed on. The conversion needs both a slew axis and a load
//! axis, so this module is where "can that arc be re-expressed at all" is answered.

use crate::arcs::REFERENCE_FAMILIES;
use crate::pins::{cell_qualifies, is_output_pin, timing_leaves};
use liberty_parser::liberty::Group;
use std::collections::BTreeMap;

/// Which of the two axes a lookup template declares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Axes {
    pub(crate) slew: bool,
    pub(crate) load: bool,
}

/// The lookup templates a library declares, and the axes each one carries.
pub(crate) struct Templates(BTreeMap<String, Axes>);

impl Templates {
    /// Index every `lu_table_template` the library declares.
    ///
    /// The group-type test is the same one `generate_pseudo_lut_templates` uses to
    /// decide what it can derive a pseudo template pair from, so a template declared
    /// under any other group type counts as undeclared here too. That is the truth
    /// that matters: the conversion could not build its derived pair from such a
    /// template either.
    pub(crate) fn of_library(lib: &Group) -> Self {
        Templates(
            lib.iter_subgroups()
                .filter(|g| g.type_ == "lu_table_template")
                .map(|g| {
                    (
                        g.name.clone(),
                        Axes {
                            slew: g.attributes.contains_key("index_1"),
                            load: g.attributes.contains_key("index_2"),
                        },
                    )
                })
                .collect(),
        )
    }

    /// What stops this template carrying the conversion's two halves, phrased for a
    /// warning -- or `None` if nothing does.
    ///
    /// Both axes are needed because the setup constraint is indexed by slew and the
    /// clock-to-output delay by load, so a template declaring only one of them
    /// describes something the split has no way to be expressed on.
    ///
    /// A name the library never declared is answered here too, so a caller skipping an
    /// arc needs no separate not-declared branch. A library whose candidate cells name
    /// an undeclared template is refused outright before any conversion begins, which
    /// makes that case unreachable for a candidate -- but answering it rather than
    /// panicking is what keeps this a question about the input instead of an assertion
    /// against it.
    ///
    /// A diagnostic naming which axis is missing can be acted on; one saying only that
    /// something is missing cannot.
    pub(crate) fn missing_axis(&self, name: &str) -> Option<&'static str> {
        match self.0.get(name) {
            None => Some("is not declared by this library"),
            Some(Axes {
                slew: false,
                load: false,
            }) => Some("declares neither a slew nor a load axis"),
            Some(Axes { slew: false, .. }) => {
                Some("declares no slew axis, which the setup constraint is indexed on")
            }
            Some(Axes { load: false, .. }) => {
                Some("declares no load axis, which the clock-to-output delay is indexed on")
            }
            Some(_) => None,
        }
    }
}

/// Every characterisation table in a candidate cell that names a lookup template the
/// library never declares.
///
/// Such a library is broken. The conversion derives its `_pseudo_constraint` and
/// `_pseudo_delay` pair from the declared template, so for a name that is not there it
/// has nothing to derive from -- and would emit a converted cell referencing templates
/// the library does not contain. That file reads and parses without complaint, so
/// nothing else would catch it.
///
/// The scope is deliberately narrow: candidate cells only, and only the four families a
/// reference is drawn from. That is a superset of the tables the conversion reads -- it
/// carries no reset filter, where the conversion skips reset arcs before extraction -- and
/// deliberately so: a reset arc is retained in the output, so one naming a template the
/// library does not declare would ship in the product. What it excludes is what matters:
/// a wider gate would reject input over tables nothing ever looks at, the power tables
/// among them, which in the shipped examples name the standard predefined `scalar`
/// template throughout.
pub(crate) fn undeclared_table_templates(lib: &Group, clock_name: &str) -> Vec<String> {
    let declared = Templates::of_library(lib);
    let mut found = Vec::new();

    for cell in lib.iter_cells().filter(|c| cell_qualifies(c, clock_name)) {
        for outpin in timing_leaves(cell, is_output_pin) {
            for timing in outpin.iter_subgroups_of_type("timing") {
                for table in timing
                    .iter_subgroups()
                    .filter(|t| REFERENCE_FAMILIES.contains(&t.type_.as_str()))
                {
                    if !declared.0.contains_key(&table.name) {
                        found.push(format!("{} pin {}: {}", cell.name, outpin.name, table.name));
                    }
                }
            }
        }
    }

    found
}

#[cfg(test)]
mod tests {
    //! What the axes of a declared template are read as, what a name the library
    //! never declared answers, and which undeclared references make a library broken.

    use super::*;

    /// One template declaring both axes and one declaring only a slew axis. A
    /// one-axis template is legal Liberty, and is the shape this tool's own derived
    /// templates take, so it is an ordinary input rather than a malformed one.
    const TEMPLATE_LIB: &str = r#"
library(template_test) {
  lu_table_template(BOTH) {
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }
  lu_table_template(SLEW_ONLY) {
    variable_1: constrained_pin_transition;
    index_1("0.01, 0.1");
  }
}
"#;

    /// Killed by: `of_library` read `index_1` for the load axis as well as the slew
    /// axis, so `SLEW_ONLY` reported both axes and `missing_axis` answered `None`.
    #[test]
    fn only_a_template_declaring_both_axes_can_carry_the_split() {
        let lib = liberty_parser::parse_lib(TEMPLATE_LIB).expect("parse fixture");
        let templates = Templates::of_library(&lib[0]);

        // The conversion indexes the constraint by slew and the delay by load, so
        // only a template declaring both can carry the two halves.
        assert_eq!(templates.missing_axis("BOTH"), None);
        assert_eq!(
            templates.missing_axis("SLEW_ONLY"),
            Some("declares no load axis, which the clock-to-output delay is indexed on")
        );

        // A name the library never declared is answered here too, so an arc-scope
        // caller needs no separate not-declared branch.
        assert_eq!(
            templates.missing_axis("NEVER_DECLARED"),
            Some("is not declared by this library")
        );
    }

    // --- undeclared_table_templates ----------------------------------------

    /// Two cells whose `cell_rise` names the same undeclared template. `CANDIDATE`
    /// declares a latch group and a `G` pin; `ORDINARY` declares neither, so nothing is
    /// asked of the conversion there and its tables are never read.
    const UNDECLARED_LIB: &str = r#"
library(undeclared_test) {
  lu_table_template(T) {
    variable_1: input_net_transition;
    variable_2: total_output_net_capacitance;
    index_1("0.01, 0.1");
    index_2("0.005, 0.05");
  }
  cell(CANDIDATE) {
    latch(IQ, IQN) { enable: "G"; data_in: "A"; }
    pin(G) { direction: input; clock: true; }
    pin(A) { direction: input; }
    pin(Q) {
      direction: output;
      function: "IQ";
      timing() {
        related_pin: "A";
        timing_type: combinational;
        cell_rise(MISSING) { values("1.0, 2.0", "3.0, 4.0"); }
      }
    }
  }
  cell(ORDINARY) {
    pin(A) { direction: input; }
    pin(Y) {
      direction: output;
      function: "A";
      timing() {
        related_pin: "A";
        timing_type: combinational;
        cell_rise(MISSING) { values("1.0, 2.0", "3.0, 4.0"); }
      }
    }
  }
}
"#;

    /// Only a candidate cell's undeclared template makes the library broken.
    ///
    /// The conversion reads the template of the four reference families, in candidate
    /// cells, and nowhere else. A table it never looks at naming a template the library
    /// never declares costs it nothing, so refusing the file over that would reject
    /// input the tool converts perfectly well.
    ///
    /// Killed by: the `cell_qualifies` filter was dropped, so `ORDINARY`'s
    /// identically-undeclared table was reported as well and the library was condemned
    /// over a table nothing reads.
    #[test]
    fn only_a_candidate_cells_undeclared_template_makes_a_library_broken() {
        let lib = liberty_parser::parse_lib(UNDECLARED_LIB).expect("parse fixture");

        assert_eq!(
            undeclared_table_templates(&lib[0], "G"),
            vec!["CANDIDATE pin Q: MISSING".to_owned()],
            "the candidate is reported, the ordinary cell is not"
        );

        // And a library whose candidate names only what it declares has nothing to
        // answer for -- otherwise the gate would refuse every library there is.
        let clean = liberty_parser::parse_lib(
            &UNDECLARED_LIB.replace("cell_rise(MISSING)", "cell_rise(T)"),
        )
        .expect("parse the declared-template variant");
        assert!(undeclared_table_templates(&clean[0], "G").is_empty());
    }
}
