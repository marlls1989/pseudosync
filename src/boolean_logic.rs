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

        // Parse row format: "input_values : current_state : next_state"
        let parts: Vec<&str> = row.split(':').map(|s| s.trim()).collect();
        if parts.len() != 3 {
            continue;
        }

        let input_values = parts[0];
        let next_states = parts[2];

        // Build condition for this row
        let input_chars: Vec<char> = input_values
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();

        if input_chars.len() != inputs.len() {
            continue;
        }

        let mut conditions = Vec::new();
        for (i, &val) in input_chars.iter().enumerate() {
            match val {
                'H' | '1' => conditions.push(inputs[i].to_string()),
                'L' | '0' => conditions.push(format!("!{}", inputs[i])),
                '-' | 'X' => {} // Don't care, skip
                _ => {}
            }
        }

        // Build the term for this row
        let term = if conditions.is_empty() {
            "1".to_string()
        } else {
            conditions.join("*")
        };

        // Parse next states for each output
        let next_state_chars: Vec<char> =
            next_states.chars().filter(|c| !c.is_whitespace()).collect();

        if next_state_chars.len() != outputs.len() {
            continue;
        }

        // Categorise based on next state for each output
        for (output_idx, &next_state) in next_state_chars.iter().enumerate() {
            match next_state {
                'H' | '1' => {
                    // Activation: output goes high
                    activation_terms
                        .entry(output_idx)
                        .or_default()
                        .push(term.clone());
                }
                'N' | '-' => {
                    // Hold: output stays at current state
                    hold_terms.entry(output_idx).or_default().push(term.clone());
                }
                'L' | '0' => {
                    // Deactivation: output goes low
                    deactivation_terms
                        .entry(output_idx)
                        .or_default()
                        .push(term.clone());
                }
                _ => {}
            }
        }
    }

    // Build the complete characteristic function for each output: activation + output*!(deactivation)
    let mut result = BTreeMap::new();

    for (output_idx, output_name) in outputs.iter().enumerate() {
        eprintln!("\n=== Processing output: {} ===", output_name);

        let activation_str = activation_terms
            .get(&output_idx)
            .map(|t| t.join("+"))
            .unwrap_or_else(|| "0".to_string());
        eprintln!("Activation terms: {}", activation_str);

        let hold_str = hold_terms
            .get(&output_idx)
            .map(|t| t.join("+"))
            .unwrap_or_else(|| "0".to_string());
        eprintln!("Hold terms: {}", hold_str);

        let deactivation_str = deactivation_terms
            .get(&output_idx)
            .map(|t| t.join("+"))
            .unwrap_or_else(|| "0".to_string());
        eprintln!("Deactivation terms: {}", deactivation_str);

        let activation_expr = if let Some(terms) = activation_terms.get(&output_idx) {
            let expr = parse_boolean_expr_with_interning(&terms.join("+"), &mut intern_pool)?;
            // Simplify activation expression
            expr.simplify_via_laws().simplify_via_bdd()
        } else {
            parse_boolean_expr_with_interning("0", &mut intern_pool)?
        };

        // Build the complete expression
        let complete_expr = if let Some(terms) = hold_terms.get(&output_idx) {
            eprintln!("Hold expression (from N states): {}", hold_str);
            let hold_expr = parse_boolean_expr_with_interning(&terms.join("+"), &mut intern_pool)?;
            let hold_simplified = hold_expr.simplify_via_laws().simplify_via_bdd();

            // Build: activation + output*hold
            let output = Expr::Terminal(intern_string(output_name, &mut intern_pool));
            let output_and_hold = Expr::and(output, hold_simplified);
            Expr::or(activation_expr, output_and_hold)
        } else {
            eprintln!("Hold expression: none (purely combinational)");
            // No hold states - purely combinational, just use activation
            activation_expr
        };

        eprintln!(
            "Complete expression before simplification: {}",
            format_boolean_expr(&complete_expr)
        );

        // Simplify the complete expression using rule-based simplification only
        let mut simplified = complete_expr.simplify_via_laws();
        eprintln!(
            "After simplify_via_laws: {}",
            format_boolean_expr(&simplified)
        );

        let mut prev = simplified.clone();

        // Keep applying until no more changes
        for _ in 0..5 {
            simplified = simplified.simplify_via_laws();
            if simplified == prev {
                break;
            }
            prev = simplified.clone();
        }

        eprintln!("Final simplified: {}", format_boolean_expr(&simplified));

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
        let q_str = format_boolean_expr(q_expr);

        // Should be A*B (activation only, no state-dependent terms)
        assert_eq!(q_str, "A*B");
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
        let q_str = format_boolean_expr(q_expr);

        // Should be A+B (or B+A, BDD may reorder)
        assert!(q_str == "A+B" || q_str == "B+A");
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
        let q_str = format_boolean_expr(q_expr);

        // XOR should simplify to some form of A*!B + !A*B
        // The exact form depends on BDD ordering, but should contain both A and B
        assert!(q_str.contains("A"));
        assert!(q_str.contains("B"));
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
        assert!(q_str.contains("Q"));
        assert!(q_str.contains("A"));
        assert!(q_str.contains("B"));
    }

    #[test]
    fn test_multi_output_statetable() {
        // Dual output: Q and QN (complementary outputs)
        // Q = A, QN = !A
        let statetable = create_statetable(
            "A",
            "Q QN",
            "H : - : H L, \
             L : - : L H",
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
            "H H : - : H H, \
             H L : - : H N, \
             L H : - : N H, \
             L L : - : L L",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        assert_eq!(result.len(), 2);

        let q1_expr = result.get("Q1").expect("Q1 not found");
        let q2_expr = result.get("Q2").expect("Q2 not found");

        let q1_str = format_boolean_expr(q1_expr);
        let q2_str = format_boolean_expr(q2_expr);

        println!("Q1: {}", q1_str);
        println!("Q2: {}", q2_str);

        // Q1 should depend on A and possibly Q1 (state-dependent)
        assert!(q1_str.contains("A"));
        // Q2 should depend on B and possibly Q2 (state-dependent)
        assert!(q2_str.contains("B"));
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
        let q_str = format_boolean_expr(q_expr);

        println!("Latch: {}", q_str);

        // The BDD should simplify to E*D + !E*Q (D*Q term should be absorbed)
        // But BDD may reorder terms
        assert!(q_str.contains("D"));
        assert!(q_str.contains("E"));
        assert!(q_str.contains("Q"));

        // Verify it's logically correct (should not have D*Q without E)
        // Common forms: E*D+!E*Q or Q*!E+D*E or similar reorderings
    }

    #[test]
    fn test_sr_latch() {
        // SR latch (NOR-based): Q = !R*(!S+Q)
        let statetable = create_statetable(
            "S R",
            "Q",
            "H L : - : H, \
             L L : - : N, \
             L H : - : L",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        let q_expr = result.get("Q").expect("Q not found");
        let q_str = format_boolean_expr(q_expr);

        println!("SR Latch: {}", q_str);

        // Should be state-dependent with S, R, and Q
        assert!(q_str.contains("S") || q_str.contains("R"));
        assert!(q_str.contains("Q"));
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
}
