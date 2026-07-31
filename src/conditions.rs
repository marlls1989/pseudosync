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
/// Two conditions share an id when they can hold at once, directly or through a
/// chain of conditions that can: a class is a connected component of the overlap
/// relation, not a set of spellings of one function.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ClassId(usize);

/// Group conditions into collision classes, in first-appearance order.
///
/// Two conditions collide when their conjunction is not a contradiction — when some
/// assignment satisfies both. So `A * B` and `A * B * C` collide, the first covering
/// the second; so do `A * B` and `B * C`, which both hold with all three pins high;
/// and so, as before, do `A * B` and `B * A`, two spellings of one function.
///
/// Overlap and not equality, because a `when` need not name every pin: conditions
/// are not all full assignments, and two of them over different pin subsets can
/// share satisfying assignments without denoting the same function. Liberty UG
/// p.7-49–50 requires the `when`s of state-dependent timing arcs to be mutually
/// exclusive — no more than one may be met at any time — so two conditions that can
/// both hold cannot be emitted as two states. They are one state.
///
/// A class is the *transitive closure* of that relation, because overlap is not
/// transitive on its own: `A * B` and `B * C` each overlap `B`, so all three are one
/// state even though the first two need the bridge to reach each other.
///
/// The BDD builder is minted locally and dies with the call: its brand is sealed
/// inside the macro's block, so no handle can escape into a struct and no two calls
/// can have their handles confused for each other.
///
/// Ids run in first-appearance order, which is this repository's convention for
/// anything numbered (see `src/render.rs`): it makes two runs over the same library
/// comparable line by line, where a sorted or hashed order would not. A class is
/// numbered by its least member index, so the first condition of a class fixes its
/// id whichever other conditions later join it.
///
/// A contradictory condition collides with nothing — not even with another
/// contradiction, a conjunction of two contradictions being a contradiction — and so
/// forms a class of its own. That is deliberate, and it is a change from the handle
/// equality this used to intern by, under which two contradictions were one class.
/// It is reachable only where a source `when` names the very pin whose opposite
/// literal the engine then conjoins, and it is left as it falls: whether the input
/// library's conditions behave is upstream of this tool.
///
/// Quadratic in the number of conditions, which is one cell's or one pin's — tens.
pub(crate) fn collision_classes(conditions: &[Condition]) -> Vec<ClassId> {
    let builder = bdd_builder!();
    let bdds: Vec<_> = conditions
        .iter()
        .map(|condition| builder.build(&condition.expr))
        .collect();

    let mut classes: Vec<ClassId> = vec![ClassId(0); bdds.len()];
    let mut assigned: Vec<bool> = vec![false; bdds.len()];
    let mut next = 0usize;

    // Seeds are taken in index order, so each class is opened by its least member and
    // the ids come out in first-appearance order.
    for seed in 0..bdds.len() {
        if assigned[seed] {
            continue;
        }
        let id = ClassId(next);
        next += 1;
        classes[seed] = id;
        assigned[seed] = true;

        // The seed's component, grown one collision at a time: whatever collides with
        // a member is a member, which is the closure.
        let mut frontier = vec![seed];
        while let Some(member) = frontier.pop() {
            for other in 0..bdds.len() {
                if !assigned[other] && !bdds[member].and(&bdds[other]).is_contradiction() {
                    classes[other] = id;
                    assigned[other] = true;
                    frontier.push(other);
                }
            }
        }
    }

    classes
}

/// The same over conditions already partitioned into groups: a class is drawn within
/// one group and never across two.
///
/// A group is a set whose members Liberty requires to exclude one another. UG
/// p.7-49–50's requirement is about the state-dependent timing arcs of one pin pair,
/// so two conditions characterised on different pin pairs were never required to be
/// mutually exclusive, and an overlap between them is not a state to resolve — it is
/// two states that happen to share an assignment, on paths that are never compared.
///
/// Ids run in first-appearance order over the groups as given and stay unique across
/// the whole call, because a class is filed under keys that do not all carry the
/// group it was drawn within: two groups' classes sharing a number would be read as
/// one state wherever they meet in such a key.
pub(crate) fn collision_classes_within(groups: &[Vec<Condition>]) -> Vec<Vec<ClassId>> {
    let mut issued = 0usize;
    groups
        .iter()
        .map(|group| {
            let ids = collision_classes(group);
            let shifted = ids.iter().map(|id| ClassId(id.0 + issued)).collect();
            // The ids of one call run `0..n`, so the greatest of them numbers the
            // classes this group opened.
            issued += ids.iter().map(|id| id.0 + 1).max().unwrap_or(0);
            shifted
        })
        .collect()
}

/// The one condition a whole collision class is stated under: the least restrictive
/// of them.
///
/// The minimised union of the class's own members and nothing further — no
/// generalisation against the other classes, no don't-care space, no off-set taken
/// from anywhere else. A class covers exactly what its members cover, and that is
/// what an arc merged over the class holds under.
///
/// Where the union equals one of the members, that member is returned whole, source
/// spelling and all. That is the least restrictive condition by definition — it
/// contains the rest — and it is the common case, covering every class but one shape:
/// a singleton is its own union, equal spellings union to either of them, and a
/// containing member such as `A * B * C` beside `A * B * C * D` unions to itself.
/// Only a class that overlaps without containment reaches Espresso.
///
/// A minimised label was written by no library, so it carries no source spelling and
/// is stated in Liberty's own rendering, its `sdf_cond` coming from the same
/// expression as for any condition this tool built. The minimised cover is turned
/// back into an expression by Espresso's own `Cover::to_expr_by_index` — by index and
/// not by name because the cover a BDD minimises to has an anonymous output, which
/// the crate's documentation names as the case for the indexed form. Its shape is the
/// crate's to choose and this tool does not second-guess it: it need not be a single
/// product term, and it need not be flat.
///
/// A failure to minimise, or to rebuild the expression from the cover, costs the
/// spelling and not the function: the plain disjunction of the members denotes the
/// same condition, unminimised.
pub(crate) fn merge_conditions(members: &[&Condition]) -> Condition {
    // Taken before any Boolean work, so a class of one keeps the very condition it
    // was built from rather than a rebuilt copy of it.
    if let [only] = members {
        return (*only).clone();
    }

    let builder = bdd_builder!();
    let bdds: Vec<_> = members
        .iter()
        .map(|member| builder.build(&member.expr))
        .collect();
    let union = bdds
        .iter()
        .fold(builder.constant(false), |union, bdd| union.or(bdd));

    if let Some(covering) = bdds.iter().position(|bdd| union.equivalent_to(bdd)) {
        return members[covering].clone();
    }

    let expr = union
        .minimize()
        .ok()
        .and_then(|cover| cover.to_expr_by_index(0).ok())
        .unwrap_or_else(|| {
            BoolExpr::build(|b| {
                members
                    .iter()
                    .map(|member| b.graft(&member.expr))
                    .reduce(|left, right| left | right)
                    .unwrap_or_else(|| b.constant(false))
            })
        });

    Condition {
        expr,
        written: None,
    }
}

#[cfg(test)]
mod tests {
    //! Behaviour of the `conditions` module: parsing, rendering and the grouping
    //! of conditions by overlap -- conditions that can hold at once, which is
    //! weaker than denoting the same function.

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

    /// Two spellings of one function are one class; a condition that cannot hold
    /// beside it is a different class; and the ids run in first-appearance order.
    ///
    /// Syntactic comparison is what this rules out: `A*B` and `B * A` are different
    /// token streams and the same Boolean function, so a class keyed on the text
    /// would split one operating state into two. Every other pair here is disjoint —
    /// each disagrees with the others on `A` or on `C` — so nothing but the two
    /// spellings shares a class, and the merged label of the class they share is the
    /// first member's text verbatim.
    ///
    /// Killed by: `collision_classes` compared `condition.liberty()` strings instead of BDD handles, which put `A*B` and `B * A` in different classes and gave `[0, 1, 2, 3, 1]`. Observed to redden this test alone.
    #[test]
    fn collision_classes_intern_by_function_in_first_appearance_order() {
        let conditions: Vec<Condition> = ["A*B", "!A * C", "B * A", "!A * !C", "!A * C"]
            .iter()
            .map(|t| Condition::parse(t).expect("parse"))
            .collect();

        // Class 0 is the first condition, class 1 the next one that cannot hold
        // beside it, and so on. `B * A` rejoins class 0 and the second `!A * C`
        // rejoins class 1.
        assert_eq!(
            collision_classes(&conditions),
            vec![ClassId(0), ClassId(1), ClassId(0), ClassId(2), ClassId(1)]
        );

        // The union of two spellings of one function is that function, which is each
        // member, so the first member is returned whole -- its own text and not the
        // rendering the second would also have produced.
        assert_eq!(
            merge_conditions(&[&conditions[0], &conditions[2]]).as_written(),
            "A*B"
        );
    }

    /// A condition covering another collides with it: `A * B` holds wherever
    /// `A * B * C` does, so the two describe one state and not two.
    ///
    /// This is the shape the emitted libraries violated. A source `when` need not
    /// name every pin, so two conditions over different pin subsets are not full
    /// assignments and can share satisfying assignments without being equal. Liberty
    /// UG p.7-49–50 forbids emitting both.
    ///
    /// Killed by: `collision_classes` reverted to interning on BDD-handle equality, under which the two distinct functions took separate ids. Observed to redden this test, its `B * C` sibling and the transitive-bridge test, while the disjoint test stays green -- equality is exactly the reading that keeps disjoint conditions apart and splits overlapping ones.
    #[test]
    fn a_condition_covering_another_is_one_class_with_it() {
        let conditions: Vec<Condition> = ["A*B", "A * B * C"]
            .iter()
            .map(|t| Condition::parse(t).expect("parse"))
            .collect();

        assert_eq!(collision_classes(&conditions), vec![ClassId(0), ClassId(0)]);
    }

    /// Two conditions that merely overlap collide: `A * B` and `B * C` both hold
    /// with all three pins high, so no more than one of them may be emitted as a
    /// state's condition.
    ///
    /// Neither contains the other, which is what separates this from the covering
    /// case above: the class's label cannot be either member.
    ///
    /// Killed by: `collision_classes` reverted to interning on BDD-handle equality, which gave the two distinct functions separate ids. Observed to redden this test, its covering sibling and the transitive-bridge test, while the disjoint test stays green.
    #[test]
    fn two_overlapping_conditions_are_one_class() {
        let conditions: Vec<Condition> = ["A * B", "B * C"]
            .iter()
            .map(|t| Condition::parse(t).expect("parse"))
            .collect();

        assert_eq!(collision_classes(&conditions), vec![ClassId(0), ClassId(0)]);
    }

    /// Conditions that cannot hold at once stay apart: `A * !B` and `!A * B`
    /// disagree on both pins, so no assignment satisfies both and they are two
    /// states.
    ///
    /// The negative half of the rule, which the two collision tests above cannot
    /// see: a criterion that collided everything would satisfy them and destroy the
    /// per-state model, collapsing every conditioned arc of a cell into one.
    ///
    /// Killed by: `collision_classes` treated every conjunction as satisfiable -- `is_contradiction()` replaced by `false` -- which merged the two disjoint conditions into one class. Observed to leave the two collision tests above green, which is the discrimination that matters here: they pin the positive half and cannot detect an over-eager collision. It also reddens the bridge, first-appearance and `conjoining_opposite_literals_yields_two_classes` tests, the module's other assertions that a disjoint pair stays apart; no mutation separates those, because they are one rule under four shapes.
    #[test]
    fn disjoint_conditions_are_two_classes() {
        let conditions: Vec<Condition> = ["A * !B", "!A * B"]
            .iter()
            .map(|t| Condition::parse(t).expect("parse"))
            .collect();

        assert_eq!(collision_classes(&conditions), vec![ClassId(0), ClassId(1)]);
    }

    /// Overlap is not transitive, so a class is its closure: two conditions that
    /// cannot hold at once are still one state when a third can hold beside either.
    ///
    /// `A * B` and `!A * C` are disjoint, and `B * C` overlaps both, so all three
    /// can be met by assignments the other two also meet in turn -- there is no way
    /// to state them as separate mutually exclusive conditions. `!B * !C` shares an
    /// assignment with none of them and is a class of its own.
    ///
    /// The ids are what shows the closure ran before the numbering: `!A * C` appears
    /// third and is class 0, while the singleton that appears second is class 1.
    ///
    /// Killed by: `collision_classes` paired each condition with the first earlier class it overlapped instead of closing the relation, which gave `[0, 1, 2, 0]` -- `!A * C` opening a class of its own before `B * C` arrived to join it to `A * B`. Observed to redden this test alone: it is the only fixture whose members need a bridge to reach each other.
    #[test]
    fn a_bridging_condition_closes_two_disjoint_ones_into_one_class() {
        let conditions: Vec<Condition> = ["A * B", "!B * !C", "!A * C", "B * C"]
            .iter()
            .map(|t| Condition::parse(t).expect("parse"))
            .collect();

        assert_eq!(
            collision_classes(&conditions),
            vec![ClassId(0), ClassId(1), ClassId(0), ClassId(0)]
        );
    }

    // --- merge_conditions ----------------------------------------------------

    /// A class whose members are contained in one of them is stated under that
    /// member, verbatim: the container is the least restrictive condition, and it is
    /// one the library actually wrote.
    ///
    /// `A*B` holds wherever `A * B * C` does, so their union is `A*B` itself. The
    /// source spelling is deliberately one the renderer would not produce, so this
    /// distinguishes returning the member from rebuilding its function.
    ///
    /// Killed by: `merge_conditions` dropped the equivalence short-circuit and always minimised, which spelled the class `A * B` -- the same function, the library's own text lost. Observed to redden this test alone; the two-cube test below never reaches the short-circuit, and the first-appearance test's class is two spellings of one function, which minimises back to the same text.
    #[test]
    fn a_contained_class_is_stated_under_its_containing_member() {
        let covering = Condition::parse("A*B").expect("parse");
        let covered = Condition::parse("A * B * C").expect("parse");

        assert_eq!(merge_conditions(&[&covering, &covered]).as_written(), "A*B");
    }

    /// A class that overlaps without containment is stated under the minimised union
    /// of its own members: that function, over exactly the pins its members name.
    ///
    /// Derived from the domain, not from the minimiser. The class's members are
    /// `A * B` and `B * C`; neither covers the other, so the least restrictive
    /// condition covering both is their union, `A * B + B * C`. How Espresso spells
    /// that function -- flat, or factored as `B * (A + C)` -- is the crate's to
    /// choose and no part of what this tool decides, so the function is pinned
    /// semantically and the text only for the pins it names. Those are `A`, `B` and
    /// `C`, whichever spelling comes back: the union depends on all three (at
    /// `B * !C` it is `A`, at `!A * B` it is `C`, at `A * !C` it is `B`) and its
    /// members name no fourth pin for it to depend on. A `when` naming a pin the cell
    /// does not have is malformed Liberty, and an equivalence check cannot see one --
    /// a spurious `D + !D` conjoined in would leave the function untouched.
    ///
    /// Killed by: `merge_conditions` returned its first member unconditionally, which stated the class as `A * B` -- a condition the second member can hold outside, so the merged arc would have been labelled with a state narrower than the one its values were computed over. Observed to redden this test and the two engine tests that merge overlapping conditions through the whole path, which is the same fault seen a layer up; the containment test beside this one stays green, so the discrimination within this module holds.
    #[test]
    fn an_overlapping_class_is_stated_under_the_minimised_union_of_its_members() {
        let left = Condition::parse("A * B").expect("parse");
        let right = Condition::parse("B * C").expect("parse");
        let merged = merge_conditions(&[&left, &right]);

        // The label denotes the union and nothing wider.
        let builder = bdd_builder!();
        assert!(builder
            .build(&merged.expr)
            .equivalent_to(&builder.parse("A * B + B * C").expect("parse")));

        // No library wrote it, so it is stated in the tool's own rendering.
        assert_eq!(merged.as_written(), merged.liberty());

        // The three pins its members name, and no other.
        let mut pins: Vec<String> = merged
            .expr
            .variables()
            .map(|pin| pin.as_str().to_owned())
            .collect();
        pins.sort();
        assert_eq!(pins, ["A", "B", "C"]);
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

    /// A group bounds the search: a condition in another group closes nothing, not
    /// even two members its own group holds apart, and the numbering runs on across
    /// the partition rather than restarting inside it.
    ///
    /// `A * !C` and `!A * C` disagree on both pins, so they are two classes wherever
    /// they are asked about together. `A + C` holds beside either of them, so the
    /// bridge test above would close all three into one -- but it sits in the second
    /// group, and a bridge is only a bridge between conditions that had to exclude
    /// each other in the first place. `!A * !C` is the complement of `A + C` and
    /// shares an assignment with neither of the first two, so the second group is two
    /// classes as well: `[[0, 1], [2, 3]]`.
    ///
    /// Ids continuing across the groups is what keeps a class identifiable where it is
    /// filed under a key that does not carry the group it was drawn in.
    ///
    /// Killed by: `collision_classes_within` classified the concatenation of the groups and sliced the answer back apart, which is the cell-wide reading this replaced: `A + C` closed the first group's two disjoint conditions into one and the answer was `[[0, 0], [1, 2]]`. Observed to redden this test alone; no other fixture in the module hands the classifier more than one group. (Dropping the `issued` shift so each group restarts at zero was applied separately, giving `[[0, 1], [0, 1]]` -- the other half of the rule, and it fails independently. That one also reddens the engine's `two_outputs_whose_conditions_overlap_are_not_one_state`, which two groups' classes sharing a number reaches through the report's class-to-condition map.)
    #[test]
    fn a_collision_is_sought_within_a_group_and_never_across_two() {
        let group = |texts: &[&str]| -> Vec<Condition> {
            texts
                .iter()
                .map(|t| Condition::parse(t).expect("parse"))
                .collect()
        };

        assert_eq!(
            collision_classes_within(&[group(&["A * !C", "!A * C"]), group(&["A + C", "!A * !C"])]),
            vec![vec![ClassId(0), ClassId(1)], vec![ClassId(2), ClassId(3)]]
        );
    }
}
