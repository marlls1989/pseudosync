//! Boolean logic and statetable parsing

use boolean_expression::Expr;
use liberty_parse::liberty::Group;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

/// Parse a boolean expression from Liberty format (+ for OR, * for AND, ! for NOT)
/// Uses Arc<str> for variable names with deduplication for memory efficiency
#[allow(dead_code)]
pub fn parse_boolean_expr(expr_str: &str) -> Option<Expr<Arc<str>>> {
    parse_boolean_expr_with_interning(expr_str, &mut HashSet::new())
}

/// Parse a boolean expression with variable name interning
fn parse_boolean_expr_with_interning(
    expr_str: &str,
    intern_pool: &mut HashSet<Arc<str>>,
) -> Option<Expr<Arc<str>>> {
    // Parse sum of products: term1+term2+...
    let terms: Vec<&str> = expr_str.split('+').collect();

    if terms.is_empty() {
        return None;
    }

    let mut result_expr = None;

    for term in terms {
        let term = term.trim();
        let literals: Vec<&str> = term.split('*').collect();

        let mut term_expr = None;
        for lit in literals {
            let lit = lit.trim();
            let var_expr = if let Some(var_name) = lit.strip_prefix('!') {
                let interned = intern_string(var_name, intern_pool);
                Expr::not(Expr::Terminal(interned))
            } else {
                let interned = intern_string(lit, intern_pool);
                Expr::Terminal(interned)
            };

            term_expr = Some(match term_expr {
                None => var_expr,
                Some(e) => Expr::and(e, var_expr),
            });
        }

        if let Some(te) = term_expr {
            result_expr = Some(match result_expr {
                None => te,
                Some(e) => Expr::or(e, te),
            });
        }
    }

    result_expr
}

/// Intern a string into the pool, returning existing Arc if present
fn intern_string(s: &str, pool: &mut HashSet<Arc<str>>) -> Arc<str> {
    if let Some(existing) = pool.get(s) {
        Arc::clone(existing)
    } else {
        let arc: Arc<str> = Arc::from(s);
        pool.insert(Arc::clone(&arc));
        arc
    }
}

/// Format a boolean expression back to Liberty format with proper parentheses
pub fn format_boolean_expr(expr: &Expr<Arc<str>>) -> String {
    match expr {
        Expr::Terminal(s) => s.to_string(),
        Expr::Not(inner) => {
            // Add parentheses if inner is not a terminal
            match **inner {
                Expr::Terminal(_) => format!("!{}", format_boolean_expr(inner)),
                _ => format!("!({})", format_boolean_expr(inner)),
            }
        }
        Expr::And(e1, e2) => {
            // Add parentheses around OR expressions in AND
            let left = match **e1 {
                Expr::Or(_, _) => format!("({})", format_boolean_expr(e1)),
                _ => format_boolean_expr(e1),
            };
            let right = match **e2 {
                Expr::Or(_, _) => format!("({})", format_boolean_expr(e2)),
                _ => format_boolean_expr(e2),
            };
            format!("{}*{}", left, right)
        }
        Expr::Or(e1, e2) => {
            format!("{}+{}", format_boolean_expr(e1), format_boolean_expr(e2))
        }
        _ => "0".to_string(),
    }
}

/// Parse a statetable and extract the characteristic functions for all outputs
/// Returns a map from output node name to its characteristic expression
pub fn parse_statetable(statetable: &Group) -> Option<BTreeMap<String, Expr<Arc<str>>>> {
    // Extract input variables and outputs from the statetable name
    // Format: "inputs", "outputs" e.g., "A P M", "Q" or "A B", "Q1 Q2"
    let name_parts: Vec<&str> = statetable.name.split('"').collect();
    if name_parts.len() < 4 {
        return None;
    }

    let inputs_str = name_parts[1];
    let outputs_str = name_parts[3];
    let inputs: Vec<&str> = inputs_str.split_whitespace().collect();
    let outputs: Vec<&str> = outputs_str.split_whitespace().collect();

    // Extract the table attribute
    let table_attr = statetable.simple_attribute("table")?;
    let table_str = table_attr.string();

    // Remove backslash continuation characters
    let table_str = table_str.replace('\\', "");

    // Create intern pool for variable names
    let mut intern_pool = HashSet::new();

    // Parse table rows - collect activation, hold, and deactivation terms for each output
    let mut activation_terms: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut hold_terms: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    let mut deactivation_terms: BTreeMap<usize, Vec<String>> = BTreeMap::new();

    for row in table_str.split(',') {
        let row = row.trim();
        if row.is_empty() {
            continue;
        }

        // Parse row format: "input_values : current_states : next_states"
        let parts: Vec<&str> = row.split(':').map(|s| s.trim()).collect();
        if parts.len() != 3 {
            continue;
        }

        let input_values = parts[0];
        let current_states = parts[1];
        let next_states = parts[2];

        // Verify current states match number of outputs
        let current_state_chars: Vec<char> = current_states
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();

        if current_state_chars.len() != outputs.len() {
            continue;
        }

        // Build base condition from inputs only
        let input_chars: Vec<char> = input_values
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();

        if input_chars.len() != inputs.len() {
            continue;
        }

        let mut base_conditions = Vec::new();
        for (i, &val) in input_chars.iter().enumerate() {
            match val {
                'H' | '1' => base_conditions.push(inputs[i].to_string()),
                'L' | '0' => base_conditions.push(format!("!{}", inputs[i])),
                '-' | 'X' => {} // Don't care, skip
                _ => {}
            }
        }

        // Parse next states for each output
        let next_state_chars: Vec<char> =
            next_states.chars().filter(|c| !c.is_whitespace()).collect();

        if next_state_chars.len() != outputs.len() {
            continue;
        }

        // Categorise based on next state for each output
        for (output_idx, &next_state) in next_state_chars.iter().enumerate() {
            let current_state = current_state_chars[output_idx];

            // Build condition including current state of this output
            let mut conditions = base_conditions.clone();
            match current_state {
                'H' | '1' => conditions.push(outputs[output_idx].to_string()),
                'L' | '0' => conditions.push(format!("!{}", outputs[output_idx])),
                '-' | 'N' | 'X' => {} // Don't care, skip
                _ => {}
            }

            let term = if conditions.is_empty() {
                "1".to_string()
            } else {
                conditions.join("*")
            };

            match next_state {
                'H' | '1' => {
                    // Activation: output goes high
                    activation_terms.entry(output_idx).or_default().push(term);
                }
                'N' | '-' => {
                    // Hold: output stays at current state
                    hold_terms.entry(output_idx).or_default().push(term);
                }
                'L' | '0' => {
                    // Deactivation: output goes low
                    deactivation_terms.entry(output_idx).or_default().push(term);
                }
                _ => {}
            }
        }
    }

    // Build the complete characteristic function for each output
    let mut result = BTreeMap::new();

    for (output_idx, output_name) in outputs.iter().enumerate() {
        let activation_expr = if let Some(terms) = activation_terms.get(&output_idx) {
            let expr = parse_boolean_expr_with_interning(&terms.join("+"), &mut intern_pool)?;
            // Simplify activation expression
            expr.simplify_via_bdd()
        } else {
            parse_boolean_expr_with_interning("0", &mut intern_pool)?
        };

        // Build hold expression from N states
        let hold_from_n = if let Some(terms) = hold_terms.get(&output_idx) {
            let hold_str = terms.join("+");
            Some(parse_boolean_expr_with_interning(
                &hold_str,
                &mut intern_pool,
            )?)
        } else {
            None
        };

        // Build base expression: activation + output*hold
        let base_expr = if let Some(terms) = hold_terms.get(&output_idx) {
            let hold_str = terms.join("+");
            let hold_expr = parse_boolean_expr_with_interning(&hold_str, &mut intern_pool)?;
            let hold_simplified = hold_expr.simplify_via_bdd();
            let output = Expr::Terminal(intern_string(output_name, &mut intern_pool));
            let output_and_hold = Expr::and(output, hold_simplified);
            Expr::or(activation_expr, output_and_hold)
        } else {
            activation_expr
        };

        // Apply deactivation constraint: (activation + output*hold) * !(deactivation)
        let complete_expr = if let Some(terms) = deactivation_terms.get(&output_idx) {
            let deact_str = terms.join("+");
            let deact_expr = parse_boolean_expr_with_interning(&deact_str, &mut intern_pool)?;
            let not_deact = Expr::not(deact_expr).simplify_via_bdd();
            Expr::and(base_expr, not_deact)
        } else {
            base_expr
        };

        // Final simplification
        let simplified = complete_expr.simplify_via_laws().simplify_via_bdd();

        // Store the characteristic expression for this output
        result.insert(output_name.to_string(), simplified);
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use indexmap::IndexMap;
    use liberty_parse::ast::Value;
    use liberty_parse::liberty::Attribute;

    /// Helper to create a statetable group for testing
    fn create_statetable(inputs: &str, outputs: &str, table: &str) -> Group {
        Group {
            type_: "statetable".to_string(),
            name: format!("\"{}\", \"{}\"", inputs, outputs),
            attributes: IndexMap::from([(
                "table".to_string(),
                vec![Attribute::Simple(Value::String(table.to_string()))],
            )]),
            subgroups: vec![],
        }
    }

    /// Collect all terminal variables from an expression
    fn collect_variables(expr: &Expr<Arc<str>>, vars: &mut HashSet<Arc<str>>) {
        match expr {
            Expr::Terminal(s) => {
                vars.insert(Arc::clone(s));
            }
            Expr::Not(inner) => collect_variables(inner, vars),
            Expr::And(e1, e2) | Expr::Or(e1, e2) => {
                collect_variables(e1, vars);
                collect_variables(e2, vars);
            }
            _ => {}
        }
    }

    /// Evaluate an expression given variable assignments
    fn evaluate_expr(
        expr: &Expr<Arc<str>>,
        assignment: &std::collections::HashMap<Arc<str>, bool>,
    ) -> bool {
        match expr {
            Expr::Terminal(s) => *assignment.get(s).unwrap_or(&false),
            Expr::Not(inner) => !evaluate_expr(inner, assignment),
            Expr::And(e1, e2) => evaluate_expr(e1, assignment) && evaluate_expr(e2, assignment),
            Expr::Or(e1, e2) => evaluate_expr(e1, assignment) || evaluate_expr(e2, assignment),
            Expr::Const(b) => *b,
        }
    }

    /// Check logical equivalence by evaluating both expressions against truth table
    fn assert_logically_equivalent(expr: &Expr<Arc<str>>, expected: &str) {
        let expected_expr = parse_boolean_expr(expected)
            .unwrap_or_else(|| panic!("Failed to parse expected expression: {}", expected));

        // Collect all variables from both expressions
        let mut vars = HashSet::new();
        collect_variables(expr, &mut vars);
        collect_variables(&expected_expr, &mut vars);

        let vars: Vec<&str> = vars.iter().map(|s| s.as_ref()).collect();

        // Generate all possible truth assignments
        let num_combinations = 1 << vars.len();

        for i in 0..num_combinations {
            let mut assignment = std::collections::HashMap::new();
            for (j, &var) in vars.iter().enumerate() {
                assignment.insert(Arc::from(var), (i >> j) & 1 == 1);
            }

            let result1 = evaluate_expr(expr, &assignment);
            let result2 = evaluate_expr(&expected_expr, &assignment);

            assert_eq!(
                result1,
                result2,
                "Expressions differ at assignment {:?}\nGot: {}\nExpected: {}",
                assignment,
                format_boolean_expr(expr),
                expected
            );
        }
    }

    #[test]
    fn test_simple_and_gate() {
        // AND gate: Q = A*B
        let statetable = create_statetable(
            "A B",
            "Q",
            "H H : - : H, \
             H L : - : L, \
             L H : - : L, \
             L L : - : L",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        assert_eq!(result.len(), 1);

        let q_expr = result.get("Q").expect("Q not found");

        // Verify logical equivalence to A*B
        assert_logically_equivalent(q_expr, "A*B");
    }

    #[test]
    fn test_simple_or_gate() {
        // OR gate: Q = A+B
        let statetable = create_statetable(
            "A B",
            "Q",
            "H H : - : H, \
             H L : - : H, \
             L H : - : H, \
             L L : - : L",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        let q_expr = result.get("Q").expect("Q not found");

        // Verify logical equivalence to A+B
        assert_logically_equivalent(q_expr, "A+B");
    }

    #[test]
    fn test_simple_xor_gate() {
        // XOR gate: Q = A*!B + !A*B
        let statetable = create_statetable(
            "A B",
            "Q",
            "H H : - : L, \
             H L : - : H, \
             L H : - : H, \
             L L : - : L",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        let q_expr = result.get("Q").expect("Q not found");

        // Verify logical equivalence to XOR
        assert_logically_equivalent(q_expr, "A*!B+!A*B");
    }

    #[test]
    fn test_c_element() {
        // C-element (Muller C-element): hysteretic function
        // Q = A*B + Q*(A+B)
        // When both inputs high: output high
        // When both inputs low: output low
        // When inputs differ: hold current state
        let statetable = create_statetable(
            "A B",
            "Q",
            "H H : - : H, \
             H L : - : N, \
             L H : - : N, \
             L L : - : L",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        let q_expr = result.get("Q").expect("Q not found");
        let q_str = format_boolean_expr(q_expr);

        // Should be Q*A + Q*B + A*B (BDD-simplified form of A*B + Q*(A+B))
        println!("C-element: {}", q_str);

        // Verify it contains all three product terms (order may vary)
        let has_ab = q_str.contains("A*B") || q_str.contains("B*A");
        let has_qa = q_str.contains("Q*A") || q_str.contains("A*Q");
        let has_qb = q_str.contains("Q*B") || q_str.contains("B*Q");
        assert!(
            has_ab && has_qa && has_qb,
            "Expected A*B + Q*A + Q*B in some order, got: {}",
            q_str
        );
    }

    #[test]
    fn test_multi_output_statetable() {
        // Dual output: Q and QN (complementary outputs)
        // Q = A, QN = !A
        let statetable = create_statetable(
            "A",
            "Q QN",
            "H : - - : H L, \
             L : - - : L H",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        assert_eq!(result.len(), 2);

        let q_expr = result.get("Q").expect("Q not found");
        let qn_expr = result.get("QN").expect("QN not found");

        let q_str = format_boolean_expr(q_expr);
        let qn_str = format_boolean_expr(qn_expr);

        println!("Q: {}", q_str);
        println!("QN: {}", qn_str);

        assert_eq!(q_str, "A");
        assert_eq!(qn_str, "!A");
    }

    #[test]
    fn test_multi_output_with_state() {
        // Two outputs with state-dependent behaviour
        let statetable = create_statetable(
            "A B",
            "Q1 Q2",
            "H H : - - : H H, \
             H L : - - : H N, \
             L H : - - : N H, \
             L L : - - : L L",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        assert_eq!(result.len(), 2);

        let q1_expr = result.get("Q1").expect("Q1 not found");
        let q2_expr = result.get("Q2").expect("Q2 not found");

        let q1_str = format_boolean_expr(q1_expr);
        let q2_str = format_boolean_expr(q2_expr);

        // Q1 = A*B + Q1*!B = A + Q1*!B (simplified)
        // Q2 = A*B + Q2*!A = B + Q2*!A (simplified)
        // Exact forms depend on BDD ordering
        assert!(q1_str.contains("A"));
        assert!(q1_str.contains("B") || q1_str.contains("Q1"));

        assert!(q2_str.contains("B"));
        assert!(q2_str.contains("A") || q2_str.contains("Q2"));
    }

    #[test]
    fn test_latch_with_enable() {
        // Simple D-latch: Q = E*D + !E*Q
        let statetable = create_statetable(
            "D E",
            "Q",
            "H H : - : H, \
             L H : - : L, \
             - L : - : N",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        let q_expr = result.get("Q").expect("Q not found");

        // Verify logical equivalence to D-latch characteristic function
        assert_logically_equivalent(q_expr, "D*E+Q*!E");
    }

    #[test]
    fn test_sr_latch() {
        // SR latch (NOR-based): Q = S*!R + Q*!R*!S
        let statetable = create_statetable(
            "S R",
            "Q",
            "H L : - : H, \
             L L : - : N, \
             L H : - : L",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        let q_expr = result.get("Q").expect("Q not found");

        // Verify logical equivalence to SR-latch characteristic function
        assert_logically_equivalent(q_expr, "S*!R+Q*!S*!R");
    }

    #[test]
    fn test_dont_care_conditions() {
        // Test that don't-care conditions are properly handled
        let statetable = create_statetable(
            "A B C",
            "Q",
            "H - - : - : H, \
             L - - : - : L",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        let q_expr = result.get("Q").expect("Q not found");
        let q_str = format_boolean_expr(q_expr);

        // Should only depend on A (B and C are don't-care)
        assert_eq!(q_str, "A");
    }

    #[test]
    fn test_dual_output_d_latch() {
        // D-latch with complementary outputs Q and QN
        // When E=H: Q=D, QN=!D (transparent)
        // When E=L: Q and QN hold (latched)
        let statetable = create_statetable(
            "D E",
            "Q QN",
            "H H : - - : H L, \
             L H : - - : L H, \
             - L : - - : N N",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        assert_eq!(result.len(), 2);

        let q_expr = result.get("Q").expect("Q not found");
        let qn_expr = result.get("QN").expect("QN not found");

        // Verify logical equivalence
        assert_logically_equivalent(q_expr, "D*E+Q*!E");
        assert_logically_equivalent(qn_expr, "!D*E+QN*!E");
    }

    #[test]
    fn test_explicit_current_state() {
        // D-latch using explicit current state instead of 'N'
        // When E=H: Q=D (transparent)
        // When E=L: Q holds current value (explicit L->L and H->H)
        let statetable = create_statetable(
            "D E",
            "Q",
            "H H : - : H, \
             L H : - : L, \
             - L : L : L, \
             - L : H : H",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        let q_expr = result.get("Q").expect("Q not found");

        // Should extract same hold condition as using 'N'
        // Both "- L : - : N" and "- L : L : L, - L : H : H" mean hold when E=L
        assert_logically_equivalent(q_expr, "D*E+Q*!E");
    }
}
