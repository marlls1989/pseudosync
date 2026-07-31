//! What the library declares about its lookup templates.
//!
//! A characterisation table names an `lu_table_template`, and the template declares
//! the axes the table is indexed on. The conversion needs both a slew axis and a load
//! axis, so this module is where "can that arc be re-expressed at all" is answered.

use crate::arcs::REFERENCE_FAMILIES;
use crate::pins::{cell_qualifies, is_output_pin, timing_leaves};
use liberty_parser::{
    ast::Value,
    liberty::{Attribute, Group},
};
use std::collections::BTreeMap;

/// The two axes a lookup template declares, and the points along each.
///
/// The values, not merely their presence: the report labels its tables with them, so
/// a residual can be read against the slew and load it occurs at rather than a row
/// and column number. `None` is an axis the template does not declare at all.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Axes {
    pub(crate) slew: Option<Vec<f64>>,
    pub(crate) load: Option<Vec<f64>>,
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
                            slew: axis_values(g, "index_1"),
                            load: axis_values(g, "index_2"),
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
                slew: None,
                load: None,
            }) => Some("declares neither a slew nor a load axis"),
            Some(Axes { slew: None, .. }) => {
                Some("declares no slew axis, which the setup constraint is indexed on")
            }
            Some(Axes { load: None, .. }) => {
                Some("declares no load axis, which the clock-to-output delay is indexed on")
            }
            Some(_) => None,
        }
    }

    /// The axes of a named template, for labelling a table indexed on it.
    pub(crate) fn axes(&self, name: &str) -> Option<&Axes> {
        self.0.get(name)
    }
}

/// The numbers an `index_1` / `index_2` attribute declares, however the parser split
/// them.
///
/// Liberty writes an axis either as one quoted, comma-separated string or as a list of
/// numbers, and a template may carry an axis attribute that is empty. An axis with no
/// points is treated as absent, since it can label nothing and satisfies no half of the
/// conversion.
pub(crate) fn axis_values(group: &Group, axis: &str) -> Option<Vec<f64>> {
    let attribute = group.attributes.get(axis)?;
    let mut values = Vec::new();

    // `index_1 ("0.01, 0.1")` is a complex attribute, `index_1 : 0.01` a simple one, and
    // both are legal. Reading only one of the two would report a declared axis as absent
    // and skip every arc indexed on it.
    for value in attribute.iter().flat_map(|a| match a {
        Attribute::Simple(v) => std::slice::from_ref(v),
        Attribute::Complex(v) => v.as_slice(),
    }) {
        match value {
            Value::Float(x) => values.push(*x),
            Value::FloatGroup(x) => values.extend(x.iter().copied()),
            Value::String(s) | Value::Expression(s) => values.extend(
                s.split(',')
                    .filter_map(|part| part.trim().parse::<f64>().ok()),
            ),
            // An axis is numeric; anything else is not a point on one.
            Value::Bool(_) => {}
        }
    }

    (!values.is_empty()).then_some(values)
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

    /// An axis is read as the points it declares, not merely as present.
    ///
    /// Liberty writes `index_1 ("0.01, 0.1")` as a *complex* attribute — a
    /// parenthesised list — and `index_1 : 0.01` as a simple one. Reading only the
    /// simple form reports a declared axis as absent, which then skips at arc scope
    /// every arc indexed on it: a library the conversion handles becomes one it
    /// refuses, with a warning blaming the input. The values themselves matter because
    /// the report captions its tables with them, and a caption is only worth having if
    /// it is the slew and load the table was measured at.
    ///
    /// Killed by: `axis_values` dropped the last point of every axis, so `BOTH` read as
    /// one slew and one load.
    ///
    /// A coarser mutation — matching `Attribute::Simple` alone, so the complex form is
    /// skipped and every declared axis reads as absent — reddens 26 tests across four
    /// modules, because arc-scope then refuses every arc in every fixture. That proves
    /// the function is load-bearing, not that this test pins the values, which is why
    /// the recorded mutation is the narrower one.
    #[test]
    fn an_axis_is_read_as_its_points_however_liberty_spells_the_attribute() {
        let lib = liberty_parser::parse_lib(TEMPLATE_LIB).expect("parse fixture");
        let templates = Templates::of_library(&lib[0]);

        let both = templates.axes("BOTH").expect("BOTH is declared");
        assert_eq!(both.slew.as_deref(), Some([0.01, 0.1].as_slice()));
        assert_eq!(both.load.as_deref(), Some([0.005, 0.05].as_slice()));

        // An axis the template does not declare stays absent rather than empty, which
        // is what `missing_axis` reads to refuse the arc.
        let slew_only = templates.axes("SLEW_ONLY").expect("SLEW_ONLY is declared");
        assert_eq!(slew_only.slew.as_deref(), Some([0.01, 0.1].as_slice()));
        assert_eq!(slew_only.load, None);

        assert!(templates.axes("NEVER_DECLARED").is_none());
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
        timing_sense : positive_unate;
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
        timing_sense : positive_unate;
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
