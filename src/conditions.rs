//! Boolean conditions: the `when` expressions a library states, and the state a
//! characterised arc leaves the cell in once its input has settled.
//!
//! This is the only module allowed to name an `espresso_logic` type. Everything
//! above it sees [`Condition`] and [`ClassId`] and nothing else, so the Boolean
//! engine can be replaced without touching the conversion.
//!
//! # The parse surface
//!
//! Established from espresso's own grammar (`src/expression/bool_expr.lalrpop` in
//! the crate), not from what happened to work:
//!
//! - operators `!` `~` (NOT), `*` `&` (AND), `^` (XOR), `+` `|` (OR), and `(` `)`,
//!   on the precedence ladder NOT > AND > XOR > OR, every binary one left-associative;
//! - constants `0`, `1`, `true`, `false`;
//! - identifiers matching `[a-zA-Z_][a-zA-Z0-9_]*`.
//!
//! Two consequences matter for Liberty input, and are pinned by tests below. The
//! postfix complement `A'` is not in that token set, so a library writing it is
//! refused rather than silently misread. A bus-indexed pin name such as `A[0]` does
//! not lex either, because `[` is not a token at all.

use espresso_logic::{bdd_builder, BoolExpr, ExprNode};

/// How one rendering spells the operators, and how it writes a pin held at a value.
///
/// The single fold below is parameterised by this rather than duplicated, so a
/// second rendering — the SDF spelling — cannot drift from the Liberty one: both
/// walk the same expression through the same precedence rules.
#[derive(Debug, Clone, Copy)]
struct Spelling {
    not: &'static str,
    and: &'static str,
    or: &'static str,
    xor: &'static str,
    one: &'static str,
    zero: &'static str,
    /// A pin at a stated value. Spelled here rather than composed from `not`,
    /// because a comparison rendering writes a low pin as a comparison against zero
    /// and not as the complement of a comparison against one.
    literal: fn(&str, bool) -> String,
}

/// Liberty's own spelling of a Boolean function.
const LIBERTY: Spelling = Spelling {
    not: "!",
    and: " * ",
    or: " + ",
    xor: " ^ ",
    one: "1",
    zero: "0",
    literal: |pin, value| {
        if value {
            pin.to_owned()
        } else {
            format!("!{}", pin)
        }
    },
};

/// SDF's spelling of the same function, as an `sdf_cond` states it.
///
/// An SDF condition is a Verilog expression over the ports, so a pin is written as
/// a comparison against a one-bit literal rather than as a bare name, and the
/// logical operators take their Verilog spellings. A low pin is `P == 1'B0` rather
/// than the complement of `P == 1'B1`: the two denote the same thing, and the
/// comparison is the form a timing-check condition is written in.
const SDF: Spelling = Spelling {
    not: "!",
    and: " && ",
    or: " || ",
    xor: " ^ ",
    one: "1'B1",
    zero: "1'B0",
    literal: |pin, value| format!("{} == 1'B{}", pin, if value { 1 } else { 0 }),
};

// Espresso's ladder, as tightness: an operand binding less tightly than the
// operator above it has to be parenthesised.
const PREC_OR: u8 = 0;
const PREC_XOR: u8 = 1;
const PREC_AND: u8 = 2;
const PREC_NOT: u8 = 3;
const PREC_ATOM: u8 = 4;

/// One rendered subexpression: its text, how tightly it binds, and the pin it names
/// where it is a bare variable.
///
/// The pin is carried because a spelling may write a low pin as something other than
/// the complement of a high one — SDF writes `P == 1'B0` — and only a bare variable
/// can be spelled that way. Recording it here is what lets the negation be decided
/// bottom-up, in the one fold, rather than by a second walk of the tree.
struct Rendered {
    text: String,
    tightness: u8,
    variable: Option<String>,
}

/// Parenthesise an operand that binds less tightly than the operator taking it.
///
/// Equal tightness needs no parentheses: every binary operator here is
/// left-associative, and each is associative as a function, so the re-parsed tree
/// denotes the same thing.
fn operand(rendered: Rendered, operator: u8) -> String {
    if rendered.tightness < operator {
        format!("({})", rendered.text)
    } else {
        rendered.text
    }
}

/// A rendering that names no pin, which is everything but a bare variable.
fn compound(text: String, tightness: u8) -> Rendered {
    Rendered {
        text,
        tightness,
        variable: None,
    }
}

/// Render an expression in `spelling`, adding exactly the parentheses the
/// precedence ladder requires.
fn render(expr: &BoolExpr, spelling: Spelling) -> String {
    expr.fold(|node: ExprNode<Rendered>| match node {
        ExprNode::Variable(name) => Rendered {
            text: (spelling.literal)(name, true),
            tightness: PREC_ATOM,
            variable: Some(name.to_owned()),
        },
        ExprNode::Constant(value) => compound(
            if value { spelling.one } else { spelling.zero }.to_owned(),
            PREC_ATOM,
        ),
        // A negated pin has a spelling of its own; anything else is complemented.
        ExprNode::Not(mut inner) => match inner.variable.take() {
            Some(pin) => compound((spelling.literal)(&pin, false), PREC_ATOM),
            None => compound(
                format!("{}{}", spelling.not, operand(inner, PREC_NOT)),
                PREC_NOT,
            ),
        },
        ExprNode::And(left, right) => compound(
            format!(
                "{}{}{}",
                operand(left, PREC_AND),
                spelling.and,
                operand(right, PREC_AND)
            ),
            PREC_AND,
        ),
        // Espresso normalises to disjunctive normal form, so no `Xor` node reaches
        // this fold; the arm exists to make the match exhaustive. It also settles a
        // divergence in the SDF spelling: this ladder places XOR below AND, whereas
        // Verilog binds `^` tighter than `&&`, so an XOR taking an AND operand would
        // be rendered without the parentheses it needs. That shape cannot occur.
        ExprNode::Xor(left, right) => compound(
            format!(
                "{}{}{}",
                operand(left, PREC_XOR),
                spelling.xor,
                operand(right, PREC_XOR)
            ),
            PREC_XOR,
        ),
        ExprNode::Or(left, right) => compound(
            format!(
                "{}{}{}",
                operand(left, PREC_OR),
                spelling.or,
                operand(right, PREC_OR)
            ),
            PREC_OR,
        ),
    })
    .text
}

/// A Boolean condition over pin names.
#[derive(Debug, Clone)]
pub(crate) struct Condition {
    expr: BoolExpr,
    /// The exact text the library wrote this condition as, where it came from one.
    ///
    /// This is the whole point of keeping it. An emitted `when` can be that text
    /// verbatim — byte for byte what the source library said — while its `sdf_cond`
    /// is rendered from the very expression that text parsed to. The two are then
    /// equivalent by construction rather than by a second, independent translation
    /// that could disagree with the first.
    ///
    /// `None` for a condition this tool built rather than read, which no library
    /// ever wrote and so has no source spelling to preserve.
    written: Option<String>,
}

impl Condition {
    /// Parse a condition as a library wrote it, remembering the text verbatim.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        match BoolExpr::parse(text) {
            Ok(expr) => Ok(Condition {
                expr,
                written: Some(text.to_owned()),
            }),
            // The failing text is named, because a message that does not say which
            // condition it could not read cannot be acted on in a library of
            // thousands.
            Err(e) => Err(format!("cannot read condition {:?}: {}", text, e)),
        }
    }

    /// A single pin at a stated value: `P` when high, `!P` when low.
    pub(crate) fn literal(pin: &str, value: bool) -> Self {
        let expr = BoolExpr::build(|b| {
            let var = b.var(pin);
            if value {
                var
            } else {
                !var
            }
        });
        Condition {
            expr,
            written: None,
        }
    }

    /// The conjunction of two conditions.
    pub(crate) fn and(&self, other: &Self) -> Self {
        Condition {
            expr: &self.expr & &other.expr,
            written: None,
        }
    }

    /// This condition in Liberty's spelling.
    pub(crate) fn liberty(&self) -> String {
        render(&self.expr, LIBERTY)
    }

    /// This condition in SDF's spelling, as an `sdf_cond` attribute states it.
    ///
    /// Rendered from the same expression the `when` beside it was parsed from, and
    /// through the same fold, so the pair states one condition by construction
    /// rather than by two translations that could disagree.
    pub(crate) fn sdf(&self) -> String {
        render(&self.expr, SDF)
    }

    /// The source library's own spelling where there was one, and the Liberty
    /// rendering otherwise.
    pub(crate) fn as_written(&self) -> String {
        self.written
            .clone()
            .unwrap_or_else(|| render(&self.expr, LIBERTY))
    }
}

/// Which collision class a condition belongs to, within one cell.
///
/// Two conditions share an id exactly when they denote the same Boolean function,
/// however they were spelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ClassId(usize);

/// Group conditions by the function they denote, in first-appearance order.
///
/// Equivalence is decided on a BDD, so `A * B` and `B * A` — and any other pair of
/// spellings of one function — land in the same class. Syntactic comparison would
/// call them different and split a state in two.
///
/// The BDD builder is minted locally and dies with the call: its brand is sealed
/// inside the macro's block, so no handle can escape into a struct and no two calls
/// can have their handles confused for each other.
///
/// Ids run in first-appearance order, which is this repository's convention for
/// anything numbered (see `src/render.rs`): it makes two runs over the same library
/// comparable line by line, where a sorted or hashed order would not.
pub(crate) fn collision_classes(conditions: &[Condition]) -> Vec<ClassId> {
    let builder = bdd_builder!();
    let mut seen = Vec::new();

    conditions
        .iter()
        .map(|condition| {
            let bdd = builder.build(&condition.expr);
            match seen.iter().position(|already| *already == bdd) {
                Some(class) => ClassId(class),
                None => {
                    seen.push(bdd);
                    ClassId(seen.len() - 1)
                }
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! Behaviour of the `conditions` module: parsing, rendering and the grouping
    //! of conditions by the function they denote.

    use super::*;

    // --- Condition::parse --------------------------------------------------

    /// The spellings espresso also accepts are read as the same functions and
    /// rendered back in Liberty's, so what a condition is written as upstream never
    /// reaches the report or the emitted library.
    ///
    /// Killed by: `LIBERTY.one` spelled the true constant `"true"`. Observed to redden this test alone -- it is the only one that asks for a constant.
    #[test]
    fn alternate_spellings_are_rendered_back_in_liberty_form() {
        // `~` and `&` are espresso's other spellings of NOT and AND.
        assert_eq!(
            Condition::parse("~A & B").expect("parse").liberty(),
            "!A * B"
        );
        // `true` and `false` are espresso's keyword constants; Liberty writes `1`
        // and `0`.
        assert_eq!(Condition::parse("true").expect("parse").liberty(), "1");
        assert_eq!(Condition::parse("false").expect("parse").liberty(), "0");
    }

    /// A message that does not say which condition it could not read cannot be
    /// acted on in a library of thousands of them.
    ///
    /// The assertion is on the opening this code contributes, not merely on the
    /// text appearing somewhere: espresso's own message ends with `Input: "..."`,
    /// so a `contains` test would pass with our naming of the text removed --
    /// checked, and it did.
    ///
    /// Killed by: `Condition::parse`'s error arm formatted `"cannot read condition: {}"`, dropping the text this code names.
    #[test]
    fn an_unreadable_condition_names_the_text_that_failed() {
        let err = Condition::parse("A *").expect_err("a truncated conjunction is not a condition");
        assert!(
            err.starts_with("cannot read condition \"A *\":"),
            "error message was {:?}",
            err
        );
    }

    /// Two spellings that are ordinary Liberty and outside espresso's grammar, so
    /// a library using either is refused rather than silently misread.
    ///
    /// Liberty's postfix complement `A'` is not in the token set -- the grammar
    /// lists `!` and `~` and nothing else for negation -- and `[` is not a token at
    /// all, so a bus-indexed pin name does not lex. Both are recorded because both
    /// are legal Liberty: this is a limit of the parse surface, not of the input.
    ///
    /// Killed by: `Condition::parse` fell back to `Ok(Condition::literal(text, true))` on a parse failure, which accepted every text. That also reddens the error-naming test above and the engine's unreadable-`when` test; no mutation separates the two spellings from each other, because which texts lex is espresso's grammar and not this code's.
    #[test]
    fn the_parse_surface_excludes_two_ordinary_liberty_spellings() {
        assert!(Condition::parse("A'").is_err());
        assert!(Condition::parse("A[0] * B").is_err());
    }

    // --- as_written ---------------------------------------------------------

    /// A library-sourced condition remembers the exact text the library wrote, so
    /// an emitted `when` can be that text verbatim while anything derived from it
    /// is rendered from the expression that text parsed to.
    ///
    /// The source spelling here is deliberately one the renderer would not
    /// produce: extra parentheses and no spaces around the operator.
    ///
    /// Killed by: `Condition::as_written` returned `render(&self.expr, LIBERTY)` unconditionally, which normalised the source spelling to `!A * B`. Observed to redden this test alone.
    #[test]
    fn as_written_returns_the_source_spelling_and_not_the_rendering() {
        let c = Condition::parse("(!A)*(B)").expect("a parenthesised conjunction");
        assert_eq!(c.as_written(), "(!A)*(B)");
        // ... while the rendering of the same expression is the tool's own.
        assert_eq!(c.liberty(), "!A * B");
    }

    /// A condition this tool built was written by no library, so there is no
    /// source spelling to preserve and the rendering is what it has.
    ///
    /// Killed by: `Condition::literal` set `written: Some(pin.to_owned())`, so a low literal reported itself as `P` rather than `!P`. Observed to redden this test alone.
    #[test]
    fn a_built_condition_has_no_source_spelling_and_falls_back_to_the_rendering() {
        assert_eq!(Condition::literal("P", false).as_written(), "!P");
        assert_eq!(Condition::literal("P", true).as_written(), "P");
    }

    // --- rendering ----------------------------------------------------------

    /// Parentheses appear exactly where the ladder NOT > AND > XOR > OR requires
    /// them, and nowhere else.
    ///
    /// Killed by: `PREC_XOR` raised to 3, above AND, so the XOR under an AND lost the parentheses that keep it a different function. Observed to redden this test alone -- it is the only one that asks for an XOR.
    #[test]
    fn the_liberty_rendering_parenthesises_by_precedence_and_no_more() {
        // AND binds tighter than OR, so the conjunction needs no parentheses.
        assert_eq!(
            Condition::parse("A * B + C").expect("parse").liberty(),
            "A * B + C"
        );
        // The other way round it does: an OR under an AND must be bracketed or the
        // rendering would denote a different function.
        assert_eq!(
            Condition::parse("A * (B + C)").expect("parse").liberty(),
            "A * (B + C)"
        );
        // XOR sits between them, so it brackets under AND and not under OR.
        assert_eq!(
            Condition::parse("A * (B ^ C) + D")
                .expect("parse")
                .liberty(),
            "A * (B ^ C) + D"
        );
        // NOT binds tightest, so it brackets anything compound and nothing atomic.
        assert_eq!(
            Condition::parse("!(A + B) * !C").expect("parse").liberty(),
            "!(A + B) * !C"
        );
    }

    /// SDF states a condition as a Verilog expression over the ports, so every pin
    /// is a comparison against a one-bit literal and a low pin is a comparison
    /// against zero rather than a complemented comparison against one.
    ///
    /// Derivation from the domain, not from the renderer. `!M * P` holds while M is
    /// low and P is high, which SDF writes `M == 1'B0 && P == 1'B1`. A comparison is
    /// an operand in its own right, so nothing brackets it; the conjunction of two
    /// of them therefore carries no parentheses at all.
    ///
    /// Killed by: `SDF.one` spelled the true constant `"1"`, Liberty's spelling rather than SDF's one-bit literal. Observed to redden this test alone -- it is the only one that asks the SDF rendering for a constant.
    #[test]
    fn the_sdf_rendering_states_each_pin_as_a_comparison() {
        let sdf = |text: &str| Condition::parse(text).expect("parse").sdf();

        // The product of literals a post-settled state is built as.
        assert_eq!(sdf("!M * P"), "M == 1'B0 && P == 1'B1");
        // The other two binary operators take their Verilog spellings.
        assert_eq!(sdf("A + B"), "A == 1'B1 || B == 1'B1");
        assert_eq!(sdf("A ^ B"), "A == 1'B1 ^ B == 1'B1");
        // A one-bit constant is written as one.
        assert_eq!(sdf("true"), "1'B1");
        assert_eq!(sdf("false"), "1'B0");
    }

    /// Only a bare pin has a low spelling of its own; a negated compound is
    /// complemented, and bracketed exactly where the ladder requires.
    ///
    /// `!(A + B)` is not a pin at a value, so there is no comparison to invert: it
    /// has to be written as the complement of the disjunction, and the disjunction
    /// has to keep its parentheses or the complement would apply to `A` alone.
    ///
    /// Killed by: `render`'s `Not` arm took the variable spelling for every inner rendering -- `inner.variable.take().or(Some(inner.text.clone()))` -- so a complemented disjunction was written as a comparison of one. That also reddens `the_liberty_rendering_parenthesises_by_precedence_and_no_more`, which negates a compound in the other spelling; the SDF sibling above stays green under it, because that one negates bare pins alone, and that is the pair the mutation separates.
    #[test]
    fn a_negated_compound_is_complemented_rather_than_compared() {
        assert_eq!(
            Condition::parse("!(A + B) * !C").expect("parse").sdf(),
            "!(A == 1'B1 || B == 1'B1) && C == 1'B0"
        );
    }

    // --- collision_classes --------------------------------------------------

    /// Two spellings of one function are one class; a different function is a
    /// different class; and the ids run in first-appearance order.
    ///
    /// Syntactic comparison is what this rules out: `A * B` and `B * A` are
    /// different token streams and the same Boolean function, so a class keyed on
    /// the text would split one operating state into two.
    ///
    /// Killed by: `collision_classes` compared `condition.liberty()` strings instead of BDD handles, which put `A * B` and `B * A` in different classes. Observed to redden this test alone.
    #[test]
    fn collision_classes_intern_by_function_in_first_appearance_order() {
        let conditions: Vec<Condition> = ["A * B", "C", "B * A", "!C", "C"]
            .iter()
            .map(|t| Condition::parse(t).expect("parse"))
            .collect();

        // Class 0 is the first function seen, class 1 the next new one, and so on.
        // `B * A` rejoins class 0 and the second `C` rejoins class 1.
        assert_eq!(
            collision_classes(&conditions),
            vec![ClassId(0), ClassId(1), ClassId(0), ClassId(2), ClassId(1)]
        );
    }

    /// The construction the engine performs, checked where it is decided: a
    /// conditioned arc's post-settled state is its `when` conjoined with the
    /// literal for the direction the INPUT settled in.
    ///
    /// Killed by: `Condition::and` dropped its second operand, so both states collapsed into one class. That also reddens the engine's negative-sense test, which asks the same question of the whole path; that test's own mutation leaves this one green, which is what separates the two.
    #[test]
    fn conjoining_opposite_literals_yields_two_classes() {
        let when = Condition::parse("A * B").expect("parse");
        let settled_high = when.and(&Condition::literal("P", true));
        let settled_low = when.and(&Condition::literal("P", false));

        assert_eq!(settled_high.liberty(), "A * B * P");
        assert_eq!(settled_low.liberty(), "A * B * !P");
        assert_eq!(
            collision_classes(&[settled_high, settled_low]),
            vec![ClassId(0), ClassId(1)]
        );
    }
}
