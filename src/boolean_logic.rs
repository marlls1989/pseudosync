//! Boolean logic and statetable parsing

use espresso_logic::{Anonymous, BoolExpr, Cover, CoverType, Cube, CubeType, Minimizable, Symbols};
use liberty_parse::liberty::Group;
use std::collections::BTreeMap;
use std::sync::Arc;

#[cfg(test)]
use espresso_logic::expr;

/// Parse a boolean expression from Liberty format (+ for OR, * for AND, ! for NOT)
/// espresso-logic already provides parsing capabilities
#[allow(dead_code)]
pub fn parse_boolean_expr(expr_str: &str) -> Option<BoolExpr> {
    BoolExpr::parse(expr_str).ok()
}

/// Format a boolean expression back to Liberty format
/// espresso-logic's BoolExpr already implements Display
pub fn format_boolean_expr(expr: &BoolExpr) -> String {
    expr.to_string()
}

/// Parse a statetable and extract the characteristic functions for all outputs
/// Returns a map from output node name to its characteristic expression
pub fn parse_statetable(statetable: &Group) -> Option<BTreeMap<String, BoolExpr>> {
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

    // Create a single FR cover with ALL outputs
    // For sequential/feedback logic, outputs appear as both inputs and outputs with the same name
    // Input variables: primary inputs + ALL outputs (for state feedback)
    let mut input_var_names: Vec<Arc<str>> = inputs.iter().map(|s| Arc::from(*s)).collect();
    input_var_names.extend(outputs.iter().map(|s| Arc::from(*s)));

    // Output variables: all outputs
    let output_var_names: Vec<Arc<str>> = outputs.iter().map(|s| Arc::from(*s)).collect();

    // Use FR (Function-Return) type: explicitly encodes both ON-set and OFF-set.
    // Undefined input combinations become implicit don't-cares for optimization.
    // Cubes are pushed positionally onto an anonymous cover, then the variable names
    // are attached via relabel before minimisation (espresso-logic 4.x only pushes
    // cubes onto anonymous covers).
    let mut cover: Cover<Anonymous, Anonymous> = Cover::anonymous(CoverType::FR);

    // Parse table rows and add cubes to the cover
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

        // Parse input values
        let input_chars: Vec<char> = input_values
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();

        if input_chars.len() != inputs.len() {
            continue;
        }

        // Parse current states
        let current_state_chars: Vec<char> = current_states
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();

        if current_state_chars.len() != outputs.len() {
            continue;
        }

        // Parse next states
        let next_state_chars: Vec<char> =
            next_states.chars().filter(|c| !c.is_whitespace()).collect();

        if next_state_chars.len() != outputs.len() {
            continue;
        }

        // Build cubes for this row
        // We need to expand any "hold" (N/-) states into explicit cubes
        // Collect the possible output combinations we need to generate
        let mut output_expansions: Vec<Vec<(Option<bool>, Option<bool>)>> = Vec::new();

        for (output_idx, &next_state) in next_state_chars.iter().enumerate() {
            let current_state = current_state_chars[output_idx];

            // Determine what output values to generate for this output
            // Format: Vec<(current_output_value, next_output_value)>
            let output_values: Vec<(Option<bool>, Option<bool>)> = match next_state {
                'H' | '1' => {
                    // Output goes to 1
                    let current_val = match current_state {
                        'H' | '1' => Some(true),
                        'L' | '0' => Some(false),
                        _ => None, // Don't care
                    };
                    vec![(current_val, Some(true))]
                }
                'L' | '0' => {
                    // Output goes to 0
                    let current_val = match current_state {
                        'H' | '1' => Some(true),
                        'L' | '0' => Some(false),
                        _ => None, // Don't care
                    };
                    vec![(current_val, Some(false))]
                }
                'N' | '-' => {
                    // Hold current state - need TWO options if current is don't care
                    match current_state {
                        'H' | '1' => vec![(Some(true), Some(true))], // Currently 1, stays 1
                        'L' | '0' => vec![(Some(false), Some(false))], // Currently 0, stays 0
                        '-' | 'N' | 'X' => {
                            // Don't care current state, so add both possibilities
                            vec![(Some(true), Some(true)), (Some(false), Some(false))]
                        }
                        _ => vec![],
                    }
                }
                _ => vec![],
            };

            output_expansions.push(output_values);
        }

        // Generate all combinations of output values (Cartesian product)
        fn cartesian_product(
            expansions: &[Vec<(Option<bool>, Option<bool>)>],
        ) -> Vec<Vec<(Option<bool>, Option<bool>)>> {
            if expansions.is_empty() {
                return vec![vec![]];
            }

            let mut result = Vec::new();
            let rest = cartesian_product(&expansions[1..]);

            for &item in &expansions[0] {
                for combo in &rest {
                    let mut new_combo = vec![item];
                    new_combo.extend(combo);
                    result.push(new_combo);
                }
            }

            result
        }

        let all_output_combos = cartesian_product(&output_expansions);

        // Create a cube for each output combination
        for output_combo in all_output_combos {
            // Build the input literals: [primary_inputs..., current_outputs...]
            let mut input_literals = Vec::new();

            // Add primary input literals
            for &val in input_chars.iter() {
                let lit = match val {
                    'H' | '1' => Some(true),
                    'L' | '0' => Some(false),
                    '-' | 'X' => None, // Don't care
                    _ => None,
                };
                input_literals.push(lit);
            }

            // Add current state literals for ALL outputs
            for (output_idx, &(current_val, _next_val)) in output_combo.iter().enumerate() {
                // Check if this output's current state should use the computed value
                // or the value from the original current_state_chars for other outputs
                let lit = if current_state_chars[output_idx] == '-'
                    || current_state_chars[output_idx] == 'N'
                    || current_state_chars[output_idx] == 'X'
                {
                    // Use the expanded value
                    current_val
                } else {
                    // Use the explicit value
                    match current_state_chars[output_idx] {
                        'H' | '1' => Some(true),
                        'L' | '0' => Some(false),
                        _ => current_val,
                    }
                };
                input_literals.push(lit);
            }

            // espresso-logic 4.x encodes ON/OFF-set membership as the cube's CubeType
            // rather than a tri-state output, so split this row's outputs into an F-cube
            // (outputs going to 1) and an R-cube (outputs going to 0), both sharing these
            // inputs.
            let on_membership: Vec<bool> = output_combo
                .iter()
                .map(|&(_current, next)| next == Some(true))
                .collect();
            let off_membership: Vec<bool> = output_combo
                .iter()
                .map(|&(_current, next)| next == Some(false))
                .collect();

            if on_membership.iter().any(|&b| b) {
                cover.push(Cube::anonymous(&input_literals, &on_membership, CubeType::F));
            }
            if off_membership.iter().any(|&b| b) {
                cover.push(Cube::anonymous(&input_literals, &off_membership, CubeType::R));
            }
        }
    }

    // Attach the variable names (position-for-position), then minimize once for all outputs
    let cover = cover
        .relabel(
            Symbols::new(input_var_names.iter().cloned().collect()),
            Symbols::new(output_var_names.iter().cloned().collect()),
        )
        .ok()?;

    let minimized = cover.minimize().ok()?;

    // Extract all output expressions using the iterator
    let result: BTreeMap<String, BoolExpr> = minimized
        .to_exprs()
        .map(|(name, expr)| (name.to_string(), expr))
        .collect();

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

    /// Check logical equivalence using BoolExpr's equivalent_to method
    fn assert_equivalent(result: &BoolExpr, expected: &BoolExpr) {
        assert!(
            result.equivalent_to(expected),
            "Expressions are not logically equivalent:\nGot: {}\nExpected: {}",
            format_boolean_expr(result),
            format_boolean_expr(expected)
        );
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
        let expected = expr!("A" * "B");
        assert_equivalent(q_expr, &expected);
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
        let expected = expr!("A" + "B");
        assert_equivalent(q_expr, &expected);
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
        let expected = expr!("A" * !"B" + !"A" * "B");
        assert_equivalent(q_expr, &expected);
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

        // Should be Q*A + Q*B + A*B (BDD-simplified form of A*B + Q*(A+B))
        let expected = expr!("A" * "B" + "Q" * "A" + "Q" * "B");
        assert_equivalent(q_expr, &expected);
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

        let expected_q = BoolExpr::variable("A");
        let expected_qn = expr!(!"A");
        assert_equivalent(q_expr, &expected_q);
        assert_equivalent(qn_expr, &expected_qn);
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

        // Q1 = A + B*Q1 (when A=1: Q1=1; when A=0,B=1: hold; when A=0,B=0: Q1=0)
        // Q2 = B + A*Q2 (when B=1: Q2=1; when B=0,A=1: hold; when B=0,A=0: Q2=0)
        let expected_q1 = expr!("A" + "B" * "Q1");
        let expected_q2 = expr!("B" + "A" * "Q2");
        assert_equivalent(q1_expr, &expected_q1);
        assert_equivalent(q2_expr, &expected_q2);
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
        let expected = expr!("D" * "E" + "Q" * !"E");
        assert_equivalent(q_expr, &expected);
    }

    #[test]
    fn test_sr_latch() {
        use std::collections::HashMap;
        use std::sync::Arc;

        // SR latch (NOR-based): Q = S*!R + Q*!R*!S
        // State table doesn't define S=1,R=1 (invalid state)
        let statetable = create_statetable(
            "S R",
            "Q",
            "H L : - : H, \
             L L : - : N, \
             L H : - : L",
        );

        let result = parse_statetable(&statetable).expect("Failed to parse");
        let q_expr = result.get("Q").expect("Q not found");

        // With FR cover type, undefined input combinations become don't-cares,
        // so the minimizer may produce different but valid forms.
        // Verify the expression satisfies all defined state table rows:

        // S=1, R=0: Q' = 1 (regardless of current Q)
        let mut assignment = HashMap::new();
        assignment.insert(Arc::from("S"), true);
        assignment.insert(Arc::from("R"), false);
        assignment.insert(Arc::from("Q"), false);
        assert!(
            q_expr.evaluate(&assignment),
            "S=1, R=0, Q=0 should give Q'=1"
        );
        assignment.insert(Arc::from("Q"), true);
        assert!(
            q_expr.evaluate(&assignment),
            "S=1, R=0, Q=1 should give Q'=1"
        );

        // S=0, R=0: Q' = Q (hold current state)
        assignment.insert(Arc::from("S"), false);
        assignment.insert(Arc::from("R"), false);
        assignment.insert(Arc::from("Q"), false);
        assert!(
            !q_expr.evaluate(&assignment),
            "S=0, R=0, Q=0 should give Q'=0"
        );
        assignment.insert(Arc::from("Q"), true);
        assert!(
            q_expr.evaluate(&assignment),
            "S=0, R=0, Q=1 should give Q'=1"
        );

        // S=0, R=1: Q' = 0 (regardless of current Q)
        assignment.insert(Arc::from("S"), false);
        assignment.insert(Arc::from("R"), true);
        assignment.insert(Arc::from("Q"), false);
        assert!(
            !q_expr.evaluate(&assignment),
            "S=0, R=1, Q=0 should give Q'=0"
        );
        assignment.insert(Arc::from("Q"), true);
        assert!(
            !q_expr.evaluate(&assignment),
            "S=0, R=1, Q=1 should give Q'=0"
        );
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

        // Should only depend on A (B and C are don't-care)
        let expected = BoolExpr::variable("A");
        assert_equivalent(q_expr, &expected);
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
        let expected_q = expr!("D" * "E" + "Q" * !"E");
        let expected_qn = expr!(!"D" * "E" + "QN" * !"E");
        assert_equivalent(q_expr, &expected_q);
        assert_equivalent(qn_expr, &expected_qn);
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
        let expected = expr!("D" * "E" + "Q" * !"E");
        assert_equivalent(q_expr, &expected);
    }
}
