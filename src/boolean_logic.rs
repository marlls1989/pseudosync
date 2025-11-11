//! Boolean logic and statetable parsing

use espresso_logic::{BoolExpr, Cover, CoverType, Minimizable};
use liberty_parse::liberty::Group;
use std::collections::BTreeMap;

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

    // Create a Cover for each output
    // For sequential/feedback logic, outputs appear as both inputs and outputs with the same name
    let mut output_covers: BTreeMap<usize, Cover> = BTreeMap::new();
    
    for idx in 0..outputs.len() {
        // Build input variable names: primary inputs + OTHER outputs (for state feedback)
        // The current output being computed appears in both inputs and outputs with the same name
        let mut input_var_names: Vec<String> = inputs.iter().map(|s| s.to_string()).collect();
        
        // Add ALL outputs as inputs for feedback (including the output itself)
        input_var_names.extend(outputs.iter().map(|s| s.to_string()));
        
        // Output name
        let output_names = vec![outputs[idx].to_string()];
        
        let cover = Cover::with_labels(
            CoverType::FD,
            &input_var_names,
            &output_names,
        );
        output_covers.insert(idx, cover);
    }

    // Parse table rows and add cubes to the appropriate covers
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

        // Process each output
        for (output_idx, &next_state) in next_state_chars.iter().enumerate() {
            let current_state = current_state_chars[output_idx];
            
            // Determine what cubes to add based on next state
            // Format: Vec<(current_output_value, next_output_value)>
            // None for current means don't care
            let cubes_to_add: Vec<(Option<bool>, bool)> = match next_state {
                'H' | '1' => {
                    // Output goes to 1
                    // Use current state if specified, otherwise don't care
                    let current_val = match current_state {
                        'H' | '1' => Some(true),
                        'L' | '0' => Some(false),
                        _ => None, // Don't care
                    };
                    vec![(current_val, true)]
                }
                'L' | '0' => {
                    // Output goes to 0
                    let current_val = match current_state {
                        'H' | '1' => Some(true),
                        'L' | '0' => Some(false),
                        _ => None, // Don't care
                    };
                    vec![(current_val, false)]
                }
                'N' | '-' => {
                    // Hold current state - need TWO cubes if current is don't care
                    match current_state {
                        'H' | '1' => vec![(Some(true), true)], // Currently 1, stays 1
                        'L' | '0' => vec![(Some(false), false)],  // Currently 0, stays 0
                        '-' | 'N' | 'X' => {
                            // Don't care current state, so add both possibilities
                            vec![(Some(true), true), (Some(false), false)]
                        }
                        _ => vec![],
                    }
                }
                _ => vec![],
            };

            if cubes_to_add.is_empty() {
                continue;
            }

            // Create a cube for each (current_output, next_output) pair
            for &(current_output_val, next_output_val) in &cubes_to_add {
                // Build the cube: combine input literals and current state literals
                // Order matters! Must match: [primary_inputs..., outputs...]
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

                // Add output state literals (current states) - ALL outputs in order
                for (i, _output) in outputs.iter().enumerate() {
                    let lit = if i == output_idx {
                        // For the current output being processed, use the specified current value
                        current_output_val
                    } else {
                        // Other outputs - current state as specified or don't care
                        let other_current_state = current_state_chars[i];
                        match other_current_state {
                            'H' | '1' => Some(true),
                            'L' | '0' => Some(false),
                            _ => None,
                        }
                    };
                    input_literals.push(lit);
                }

                // Output literal - the next state for this output
                let output_literals = vec![Some(next_output_val)];

                // Add cube to the cover
                if let Some(cover) = output_covers.get_mut(&output_idx) {
                    cover.add_cube(&input_literals, &output_literals);
                }
            }
        }
    }

    // Convert covers to BoolExpr for each output
    let mut result = BTreeMap::new();
    for (output_idx, output_name) in outputs.iter().enumerate() {
        if let Some(mut cover) = output_covers.remove(&output_idx) {
            // Minimize the cover
            if let Ok(minimized) = cover.minimize() {
                cover = minimized;
            }

            // Convert cover to BoolExpr
            let expr = cover.to_expr(output_name).ok()?;
            result.insert(output_name.to_string(), expr);
        }
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

    /// Check logical equivalence using BoolExpr's built-in equivalency checking
    fn assert_logically_equivalent(expr: &BoolExpr, expected: &str) {
        let expected_expr = parse_boolean_expr(expected)
            .unwrap_or_else(|| panic!("Failed to parse expected expression: {}", expected));

        // Minimize both expressions for comparison
        let expr_min = expr.minimize().unwrap_or_else(|_| expr.clone());
        let expected_min = expected_expr.minimize().unwrap_or(expected_expr);

        // Check if they're equivalent by comparing their minimized forms
        // Two expressions are equivalent if (A XOR B) minimizes to false
        let expr_str = expr_min.to_string();
        let expected_str = expected_min.to_string();
        
        // Create XOR: (expr AND NOT expected) OR (NOT expr AND expected)
        let xor_expr_str = format!("(({})*~({}))+(~({})*({})))", expr_str, expected_str, expr_str, expected_str);
        
        if let Ok(xor_expr) = BoolExpr::parse(&xor_expr_str) {
            if let Ok(minimized_xor) = xor_expr.minimize() {
                let result = minimized_xor.to_string();
                // If XOR minimizes to 0 or false, expressions are equivalent
                let is_equivalent = result == "0" || result == "false" || result.is_empty();
                
                assert!(
                    is_equivalent,
                    "Expressions are not logically equivalent:\nGot: {}\nExpected: {}",
                    format_boolean_expr(expr),
                    expected
                );
                return;
            }
        }
        
        // Fallback: direct string comparison of minimized forms
        assert_eq!(
            expr_str, expected_str,
            "Expressions differ:\nGot: {}\nExpected: {}",
            format_boolean_expr(expr),
            expected
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
        // espresso-logic uses spaces in output: "A * B" instead of "A*B"
        let normalized = q_str.replace(" ", "");
        let has_ab = normalized.contains("A*B") || normalized.contains("B*A");
        let has_qa = normalized.contains("Q*A") || normalized.contains("A*Q");
        let has_qb = normalized.contains("Q*B") || normalized.contains("B*Q");
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
        // espresso-logic uses ~ for NOT instead of !
        assert_eq!(qn_str, "~A");
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

        // With FD cover type, the minimizer produces S*!R + Q*!R = (S+Q)*!R
        // This is equivalent to the original S*!R + Q*!S*!R when S=1,R=1 is undefined
        assert_logically_equivalent(q_expr, "S*!R+Q*!R");
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
