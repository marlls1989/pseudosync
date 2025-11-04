//! Boolean logic and statetable parsing

use boolean_expression::Expr;
use liberty_parse::liberty::Group;

/// Parse a simple boolean expression from Liberty format (+ for OR, * for AND, ! for NOT)
pub fn parse_boolean_expr(expr_str: &str) -> Option<Expr<String>> {
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
                Expr::not(Expr::Terminal(var_name.to_string()))
            } else {
                Expr::Terminal(lit.to_string())
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

/// Format a boolean expression back to Liberty format with proper parentheses
pub fn format_boolean_expr(expr: &Expr<String>) -> String {
    match expr {
        Expr::Terminal(s) => s.clone(),
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

/// Parse a statetable and extract the characteristic function
/// Returns the complete characteristic expression
pub fn parse_statetable(statetable: &Group) -> Option<String> {
    // Extract input variables and output from the statetable name
    // Format: "inputs", "output" e.g., "A P M", "Q"
    let name_parts: Vec<&str> = statetable.name.split('"').collect();
    if name_parts.len() < 4 {
        return None;
    }
    
    let inputs_str = name_parts[1];
    let output_str = name_parts[3];
    let inputs: Vec<&str> = inputs_str.split_whitespace().collect();

    // Extract the table attribute
    let table_attr = statetable.simple_attribute("table")?;
    let table_str = table_attr.string();

    // Remove backslash continuation characters
    let table_str = table_str.replace('\\', "");

    // Parse table rows
    let mut activation_terms = Vec::new();
    let mut deactivation_terms = Vec::new();

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
        let next_state = parts[2];

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

        // Categorise based on next state
        match next_state {
            "H" | "1" => {
                // Activation: output goes high
                activation_terms.push(term);
            }
            "N" | "-" => {
                // Hold: output stays at current state (skip)
            }
            "L" | "0" => {
                // Deactivation: output goes low
                deactivation_terms.push(term);
            }
            _ => {}
        }
    }

    // Build the complete characteristic function: activation + output*!(deactivation)
    let activation_expr = if activation_terms.is_empty() {
        parse_boolean_expr("0")?
    } else {
        parse_boolean_expr(&activation_terms.join("+"))?
    };
    
    let hold_expr = if deactivation_terms.is_empty() {
        parse_boolean_expr("1")? // No deactivation means always hold
    } else {
        let deact = parse_boolean_expr(&deactivation_terms.join("+"))?;
        Expr::not(deact)
    };
    
    // Build: activation + output*hold
    let output = Expr::Terminal(output_str.to_string());
    let output_and_hold = Expr::and(output, hold_expr);
    let complete_expr = Expr::or(activation_expr, output_and_hold);

    // Simplify the complete expression
    let simplified = complete_expr.simplify_via_bdd();
    
    // Return the complete simplified characteristic function
    Some(format_boolean_expr(&simplified))
}

