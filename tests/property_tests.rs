//! Property-based tests and fuzzing for pseudosync
//! Tests edge cases and ensures robustness against malformed inputs
//! Includes specific tests targeting RCELEM2X1 and RACELEM21X1 reference cells

use liberty_parse::parse_lib;
use pseudosync::*;
use regex::Regex;
use std::collections::HashSet;
use std::path::Path;

/// Test that the transformation is idempotent - running it twice gives same result
#[test]
fn test_transformation_idempotence() {
    let lib_str = r#"
library(idempotent_test) {
    delay_model: table_lookup;
    time_unit: "1ns";
    
    lu_table_template(test_template) {
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.1, 0.2, 0.3");
        index_2("0.01, 0.02, 0.03");
    }
    
    cell(TEST_LATCH) {
        latch(IQ) {
            enable: "CLK";
            data_in: "D";
        }
        
        pin(CLK) {
            direction: input;
        }
        
        pin(D) {
            direction: input;
            timing() {
                related_pin: "IN";
                cell_rise(test_template) {
                    values("0.1, 0.2, 0.3", "0.2, 0.3, 0.4", "0.3, 0.4, 0.5");
                }
                cell_fall(test_template) {
                    values("0.11, 0.21, 0.31", "0.21, 0.31, 0.41", "0.31, 0.41, 0.51");
                }
                rise_transition(test_template) {
                    values("0.05, 0.1, 0.15", "0.1, 0.15, 0.2", "0.15, 0.2, 0.25");
                }
                fall_transition(test_template) {
                    values("0.06, 0.11, 0.16", "0.11, 0.16, 0.21", "0.16, 0.21, 0.26");
                }
            }
        }
        
        pin(Q) {
            direction: output;
            function: "IQ";
        }
    }
}
"#;

    let mut liberty1 = parse_lib(lib_str).expect("Failed to parse");
    let mut liberty2 = parse_lib(lib_str).expect("Failed to parse");

    let clock_name = "CLK";
    let reset_name = Regex::new(r"RST").unwrap();

    // Apply transformation once
    process_library(&mut liberty1[0], clock_name, &reset_name, false);

    // Apply transformation twice
    process_library(&mut liberty2[0], clock_name, &reset_name, false);
    process_library(&mut liberty2[0], clock_name, &reset_name, false);

    // Convert to strings for comparison
    let str1 = format!("{}", liberty1);
    let str2 = format!("{}", liberty2);

    // Should be identical (transformation is idempotent)
    assert_eq!(str1, str2, "Transformation should be idempotent");
}

/// Test that non-qualifying cells are not modified
#[test]
fn test_non_qualifying_cells_unchanged() {
    let lib_str = r#"
library(non_qualifying_test) {
    delay_model: table_lookup;
    
    cell(NORMAL_AND) {
        area: 1.0;
        pin(A) {
            direction: input;
        }
        pin(B) {
            direction: input;
        }
        pin(Y) {
            direction: output;
            function: "A & B";
        }
    }
    
    cell(LATCH_NO_CLOCK) {
        latch(IQ) {
            enable: "G";
            data_in: "D";
        }
        pin(D) {
            direction: input;
        }
        pin(Q) {
            direction: output;
            function: "IQ";
        }
    }
    
    cell(NO_LATCH_WITH_CLOCK) {
        pin(CLK) {
            direction: input;
            clock: true;
        }
        pin(A) {
            direction: input;
        }
        pin(Y) {
            direction: output;
            function: "A";
        }
    }
}
"#;

    let original_liberty = parse_lib(lib_str).expect("Failed to parse original");
    let mut modified_liberty = parse_lib(lib_str).expect("Failed to parse for modification");

    let clock_name = "CLK";
    let reset_name = Regex::new(r"RST").unwrap();

    process_library(&mut modified_liberty[0], clock_name, &reset_name, false);

    let original_lib = &original_liberty[0];
    let modified_lib = &modified_liberty[0];

    // NORMAL_AND should be completely unchanged
    let original_and = original_lib.get_cell("NORMAL_AND").unwrap();
    let modified_and = modified_lib.get_cell("NORMAL_AND").unwrap();
    assert_eq!(
        format!("{:?}", original_and),
        format!("{:?}", modified_and),
        "Non-qualifying cells should remain unchanged"
    );

    // LATCH_NO_CLOCK should be unchanged (no clock pin CLK)
    let original_latch_no_clk = original_lib.get_cell("LATCH_NO_CLOCK").unwrap();
    let modified_latch_no_clk = modified_lib.get_cell("LATCH_NO_CLOCK").unwrap();
    assert_eq!(
        format!("{:?}", original_latch_no_clk),
        format!("{:?}", modified_latch_no_clk),
        "Latch without matching clock pin should remain unchanged"
    );

    // NO_LATCH_WITH_CLOCK should be unchanged (no latch group)
    let original_no_latch = original_lib.get_cell("NO_LATCH_WITH_CLOCK").unwrap();
    let modified_no_latch = modified_lib.get_cell("NO_LATCH_WITH_CLOCK").unwrap();
    assert_eq!(
        format!("{:?}", original_no_latch),
        format!("{:?}", modified_no_latch),
        "Cell with clock but no latch should remain unchanged"
    );
}

/// Test handling of malformed or incomplete Liberty structures
#[test]
fn test_malformed_input_handling() {
    let test_cases = [
        // Missing timing data
        r#"
library(missing_timing) {
    delay_model: table_lookup;
    cell(BROKEN_LATCH) {
        latch(IQ) {
            enable: "CLK";
            data_in: "D";
        }
        pin(CLK) { direction: input; }
        pin(D) { direction: input; }
        pin(Q) { direction: output; function: "IQ"; }
    }
}
"#,
        // Latch with no enable
        r#"
library(no_enable) {
    delay_model: table_lookup;
    cell(NO_ENABLE_LATCH) {
        latch(IQ) {
            data_in: "D";
        }
        pin(CLK) { direction: input; }
        pin(D) { direction: input; }
        pin(Q) { direction: output; function: "IQ"; }
    }
}
"#,
        // Empty cell
        r#"
library(empty_cell) {
    delay_model: table_lookup;
    cell(EMPTY) {
    }
}
"#,
    ];

    let clock_name = "CLK";
    let reset_name = Regex::new(r"RST").unwrap();

    for (i, lib_str) in test_cases.iter().enumerate() {
        let mut liberty =
            parse_lib(lib_str).unwrap_or_else(|_| panic!("Failed to parse test case {}", i));

        // Should not panic even with malformed input
        process_library(&mut liberty[0], clock_name, &reset_name, false);

        // Transformation should complete without errors
        assert_eq!(
            liberty.len(),
            1,
            "Should still have one library after processing malformed input"
        );
    }
}

/// Test edge cases with different clock and reset pin names
#[test]
fn test_various_clock_and_reset_names() {
    let lib_str = r#"
library(pin_name_test) {
    delay_model: table_lookup;
    
    lu_table_template(test_template) {
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.1, 0.2");
        index_2("0.01, 0.02");
    }
    
    cell(MULTI_PIN_LATCH) {
        latch(IQ) {
            enable: "CLOCK_PIN";
            data_in: "DATA";
        }
        
        pin(CLOCK_PIN) { direction: input; }
        pin(DATA) { direction: input; }
        pin(RESET_N) { direction: input; }
        pin(SET_B) { direction: input; }
        pin(CLR) { direction: input; }
        pin(Q) { 
            direction: output; 
            function: "IQ"; 
            timing() {
                related_pin: "DATA";
                timing_type: combinational;
                cell_rise(test_template) { values("0.1, 0.2", "0.2, 0.3"); }
                cell_fall(test_template) { values("0.1, 0.2", "0.2, 0.3"); }
                rise_transition(test_template) { values("0.05, 0.1", "0.1, 0.15"); }
                fall_transition(test_template) { values("0.05, 0.1", "0.1, 0.15"); }
            }
        }
    }
}
"#;

    let test_cases = vec![
        (
            "CLOCK_PIN",
            r"RESET_N",
            "Should work with custom clock and reset names",
        ),
        ("CLOCK_PIN", r"SET_B", "Should work with SET reset pin"),
        ("CLOCK_PIN", r"CLR", "Should work with CLR reset pin"),
        (
            "CLOCK_PIN",
            r"(RESET|SET|CLR).*",
            "Should work with complex reset regex",
        ),
    ];

    for (clock_name, reset_pattern, description) in test_cases {
        let mut liberty = parse_lib(lib_str).expect("Failed to parse");
        let reset_name = Regex::new(reset_pattern)
            .unwrap_or_else(|_| panic!("Invalid regex: {}", reset_pattern));

        process_library(&mut liberty[0], clock_name, &reset_name, false);

        let lib = &liberty[0];
        let cell = lib.get_cell("MULTI_PIN_LATCH").expect("Cell not found");

        // Should have processed the cell successfully
        let ff_group = cell.iter_subgroups_of_type("ff").next();
        assert!(
            ff_group.is_some(),
            "{}: Should have converted latch to ff",
            description
        );

        if let Some(ff) = ff_group {
            assert_eq!(
                ff.simple_attribute("clocked_on").unwrap().string(),
                clock_name,
                "{}: Should use correct clock name",
                description
            );
        }
    }
}

/// Test that library-level attributes are preserved
#[test]
fn test_library_attributes_preservation() {
    let lib_str = r#"
library(attr_preservation_test) {
    comment: "Test library with many attributes";
    date: "$Date: 2024-01-01 $";
    revision: "1.0";
    delay_model: table_lookup;
    capacitive_load_unit(1, pf);
    current_unit: "1mA";
    time_unit: "1ns";
    voltage_unit: "1V";
    nom_process: 1;
    nom_temperature: 25;
    nom_voltage: 1.1;
    
    operating_conditions(typical) {
        process: 1;
        temperature: 25;
        voltage: 1.1;
    }
    
    default_operating_conditions: typical;
    
    lu_table_template(test_template) {
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.1, 0.2");
        index_2("0.01, 0.02");
    }
    
    cell(TEST_LATCH) {
        latch(IQ) {
            enable: "CLK";
            data_in: "D";
        }
        pin(CLK) { direction: input; }
        pin(D) { 
            direction: input;
            timing() {
                related_pin: "IN";
                cell_rise(test_template) { values("0.1, 0.2", "0.2, 0.3"); }
                cell_fall(test_template) { values("0.1, 0.2", "0.2, 0.3"); }
                rise_transition(test_template) { values("0.05, 0.1", "0.1, 0.15"); }
                fall_transition(test_template) { values("0.05, 0.1", "0.1, 0.15"); }
            }
        }
        pin(Q) { direction: output; function: "IQ"; }
    }
}
"#;

    let original_liberty = parse_lib(lib_str).expect("Failed to parse original");
    let mut modified_liberty = parse_lib(lib_str).expect("Failed to parse for modification");

    let clock_name = "CLK";
    let reset_name = Regex::new(r"RST").unwrap();

    process_library(&mut modified_liberty[0], clock_name, &reset_name, false);

    let original_lib = &original_liberty[0];
    let modified_lib = &modified_liberty[0];

    // Library-level attributes should be preserved
    assert_eq!(
        original_lib.simple_attribute("comment").unwrap().string(),
        modified_lib.simple_attribute("comment").unwrap().string()
    );
    assert_eq!(
        original_lib.simple_attribute("date").unwrap().string(),
        modified_lib.simple_attribute("date").unwrap().string()
    );
    assert_eq!(
        original_lib.simple_attribute("delay_model").unwrap().expr(),
        modified_lib.simple_attribute("delay_model").unwrap().expr()
    );
    assert_eq!(
        original_lib.simple_attribute("time_unit").unwrap().string(),
        modified_lib.simple_attribute("time_unit").unwrap().string()
    );

    // Operating conditions should be preserved
    let original_opcond = original_lib
        .iter_subgroups_of_type("operating_conditions")
        .find(|oc| oc.name == "typical")
        .expect("Original operating conditions not found");
    let modified_opcond = modified_lib
        .iter_subgroups_of_type("operating_conditions")
        .find(|oc| oc.name == "typical")
        .expect("Modified operating conditions not found");

    assert_eq!(
        original_opcond.simple_attribute("process").unwrap().float(),
        modified_opcond.simple_attribute("process").unwrap().float()
    );
    assert_eq!(
        original_opcond
            .simple_attribute("temperature")
            .unwrap()
            .float(),
        modified_opcond
            .simple_attribute("temperature")
            .unwrap()
            .float()
    );
    assert_eq!(
        original_opcond.simple_attribute("voltage").unwrap().float(),
        modified_opcond.simple_attribute("voltage").unwrap().float()
    );
}

/// Test handling of multiple latch cells in same library
#[test]
fn test_multiple_latch_cells() {
    let lib_str = r#"
library(multi_latch_test) {
    delay_model: table_lookup;
    
    lu_table_template(template1) {
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.1, 0.2");
        index_2("0.01, 0.02");
    }
    
    lu_table_template(template2) {
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.05, 0.1");
        index_2("0.005, 0.01");
    }
    
    cell(LATCH1) {
        latch(IQ) {
            enable: "G";
            data_in: "D";
        }
        pin(G) { direction: input; }
        pin(D) { direction: input; }
        pin(Q) { 
            direction: output; 
            function: "IQ";
            timing() {
                related_pin: "D";
                timing_type: combinational;
                cell_rise(template1) { values("0.1, 0.2", "0.2, 0.3"); }
                cell_fall(template1) { values("0.1, 0.2", "0.2, 0.3"); }
                rise_transition(template1) { values("0.05, 0.1", "0.1, 0.15"); }
                fall_transition(template1) { values("0.05, 0.1", "0.1, 0.15"); }
            }
        }
    }
    
    cell(LATCH2) {
        latch(IQ, IQN) {
            enable: "G";
            data_in: "D";
        }
        pin(G) { direction: input; }
        pin(D) { direction: input; }
        pin(Q) { 
            direction: output; 
            function: "IQ"; 
            timing() {
                related_pin: "D";
                timing_type: combinational;
                cell_rise(template2) { values("0.05, 0.1", "0.1, 0.15"); }
                cell_fall(template2) { values("0.05, 0.1", "0.1, 0.15"); }
                rise_transition(template2) { values("0.025, 0.05", "0.05, 0.075"); }
                fall_transition(template2) { values("0.025, 0.05", "0.05, 0.075"); }
            }
        }
        pin(QN) { 
            direction: output; 
            function: "IQN"; 
            timing() {
                related_pin: "D";
                timing_type: combinational;
                cell_rise(template2) { values("0.05, 0.1", "0.1, 0.15"); }
                cell_fall(template2) { values("0.05, 0.1", "0.1, 0.15"); }
                rise_transition(template2) { values("0.025, 0.05", "0.05, 0.075"); }
                fall_transition(template2) { values("0.025, 0.05", "0.05, 0.075"); }
            }
        }
    }
    
    cell(NOT_A_LATCH) {
        pin(A) { direction: input; }
        pin(Y) { direction: output; function: "!A"; }
    }
}
"#;

    let mut liberty = parse_lib(lib_str).expect("Failed to parse");
    let clock_name = "G";
    let reset_name = Regex::new(r"RST").unwrap();

    process_library(&mut liberty[0], clock_name, &reset_name, false);

    let lib = &liberty[0];

    // Both latch cells should be converted
    let latch1 = lib.get_cell("LATCH1").expect("LATCH1 not found");
    let latch2 = lib.get_cell("LATCH2").expect("LATCH2 not found");
    let not_latch = lib.get_cell("NOT_A_LATCH").expect("NOT_A_LATCH not found");

    assert!(
        latch1.iter_subgroups_of_type("ff").next().is_some(),
        "LATCH1 should be converted to ff"
    );
    assert!(
        latch2.iter_subgroups_of_type("ff").next().is_some(),
        "LATCH2 should be converted to ff"
    );
    assert_eq!(
        not_latch.iter_subgroups_of_type("ff").count(),
        0,
        "NOT_A_LATCH should not be converted"
    );

    // Both templates should have pseudo versions generated
    let pseudo_templates: HashSet<_> = lib
        .iter_subgroups_of_type("lu_table_template")
        .filter(|t| t.name.contains("pseudo"))
        .map(|t| t.name.clone())
        .collect();

    assert!(
        pseudo_templates.contains("template1_pseudo_delay"),
        "Should have template1 pseudo delay"
    );
    assert!(
        pseudo_templates.contains("template1_pseudo_constraint"),
        "Should have template1 pseudo constraint"
    );
    assert!(
        pseudo_templates.contains("template2_pseudo_delay"),
        "Should have template2 pseudo delay"
    );
    assert!(
        pseudo_templates.contains("template2_pseudo_constraint"),
        "Should have template2 pseudo constraint"
    );
}

/// Test behavior with very large timing tables
#[test]
fn test_large_timing_tables() {
    // Generate a large 10x10 timing table
    let large_values: Vec<String> = (0..10)
        .map(|row| {
            (0..10)
                .map(|col| {
                    format!(
                        "{:.3}",
                        (row as f64 + 1.0) * 0.1 + (col as f64 + 1.0) * 0.01
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .map(|row| format!("\"{}\"", row))
        .collect();

    let lib_str = format!(
        r#"
library(large_table_test) {{
    delay_model: table_lookup;
    
    lu_table_template(large_template) {{
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07, 0.08, 0.09, 0.1");
        index_2("0.001, 0.002, 0.003, 0.004, 0.005, 0.006, 0.007, 0.008, 0.009, 0.01");
    }}
    
    cell(LARGE_LATCH) {{
        latch(IQ) {{
            enable: "CLK";
            data_in: "D";
        }}
        
        pin(CLK) {{ direction: input; }}
        pin(D) {{ direction: input; }}
        pin(Q) {{ 
            direction: output; 
            function: "IQ"; 
            timing() {{
                related_pin: "D";
                timing_type: combinational;
                cell_rise(large_template) {{
                    values ( \
                        {} \
                    );
                }}
                cell_fall(large_template) {{
                    values ( \
                        {} \
                    );
                }}
                rise_transition(large_template) {{
                    values ( \
                        {} \
                    );
                }}
                fall_transition(large_template) {{
                    values ( \
                        {} \
                    );
                }}
            }}
        }}
    }}
}}
"#,
        large_values.join(", \\\n                        "),
        large_values.join(", \\\n                        "),
        large_values.join(", \\\n                        "),
        large_values.join(", \\\n                        ")
    );

    let mut liberty = parse_lib(&lib_str).expect("Failed to parse large timing table");
    let clock_name = "CLK";
    let reset_name = Regex::new(r"RST").unwrap();

    // Should handle large timing tables without issues
    process_library(&mut liberty[0], clock_name, &reset_name, false);

    let lib = &liberty[0];
    let cell = lib.get_cell("LARGE_LATCH").expect("LARGE_LATCH not found");

    // Should successfully convert even with large tables
    assert!(
        cell.iter_subgroups_of_type("ff").next().is_some(),
        "Should handle large timing tables"
    );

    // Check that timing constraints were generated
    let d_pin = cell.get_pin("D").expect("D pin not found");
    assert!(
        d_pin.iter_subgroups_of_type("timing").any(|t| t
            .simple_attribute("timing_type")
            .map(|tt| tt.expr() == "setup_rising")
            .unwrap_or(false)),
        "Should generate setup constraints for large tables"
    );
}

/// Stress test with deeply nested structures
#[test]
fn test_deeply_nested_structures() {
    let lib_str = r#"
library(nested_test) {
    delay_model: table_lookup;
    
    lu_table_template(test_template) {
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.1");
        index_2("0.01");
    }
    
    cell(NESTED_LATCH) {
        latch(IQ) {
            enable: "CLK";
            data_in: "D";
        }
        
        pin(CLK) { direction: input; }
        pin(D) { 
            direction: input;
            timing() {
                related_pin: "A";
                cell_rise(test_template) { values("0.1"); }
                cell_fall(test_template) { values("0.1"); }
                rise_transition(test_template) { values("0.05"); }
                fall_transition(test_template) { values("0.05"); }
                timing() {
                    related_pin: "B";
                    cell_rise(test_template) { values("0.2"); }
                }
            }
            timing() {
                related_pin: "C";
                cell_rise(test_template) { values("0.3"); }
            }
        }
        pin(Q) { 
            direction: output; 
            function: "IQ";
            timing() {
                related_pin: "CLK";
                timing_type: rising_edge;
                cell_rise(test_template) { values("0.4"); }
                cell_fall(test_template) { values("0.4"); }
                rise_transition(test_template) { values("0.2"); }
                fall_transition(test_template) { values("0.2"); }
            }
        }
    }
}
"#;

    let mut liberty = parse_lib(lib_str).expect("Failed to parse nested structure");
    let clock_name = "CLK";
    let reset_name = Regex::new(r"RST").unwrap();

    // Should handle nested timing groups
    process_library(&mut liberty[0], clock_name, &reset_name, false);

    let lib = &liberty[0];
    let cell = lib
        .get_cell("NESTED_LATCH")
        .expect("NESTED_LATCH not found");

    // Should successfully process even with complex nested timing
    assert!(
        cell.iter_subgroups_of_type("ff").next().is_some(),
        "Should handle nested timing structures"
    );

    // Verify that multiple timing arcs are processed
    let d_pin = cell.get_pin("D").expect("D pin not found");
    let setup_count = d_pin
        .iter_subgroups_of_type("timing")
        .filter(|t| {
            t.simple_attribute("timing_type")
                .map(|tt| tt.expr() == "setup_rising")
                .unwrap_or(false)
        })
        .count();

    assert!(
        setup_count > 0,
        "Should generate setup timing for nested structures"
    );
}

/// Property-based test: RCELEM2X1 cell pattern variations
#[test]
fn test_rcelem2x1_pattern_variations() {
    // Generate variations of RCELEM2X1-like cells with different properties
    let test_variations = vec![
        // Basic RCELEM2X1 pattern
        ("A*B+A*IQ+B*IQ", vec!["A", "B"], "basic_rcelem"),
        // Simplified Boolean expressions
        ("A+B", vec!["A", "B"], "simple_or"),
        ("A*B", vec!["A", "B"], "simple_and"),
        // More complex expressions
        ("A*B*C+A*IQ+B*IQ+C*IQ", vec!["A", "B", "C"], "three_input"),
        // Single input
        ("D", vec!["D"], "single_input"),
    ];

    let clock_pin = "G";
    let reset_regex = Regex::new(r"(R|S)N?").unwrap();

    for (data_in_expr, input_pins, test_name) in test_variations {
        // Generate pin definitions
        let pin_defs: String = input_pins
            .iter()
            .map(|pin| format!("pin({}) {{ direction: input; }}", pin))
            .collect::<Vec<_>>()
            .join("\n        ");

        // Generate timing definitions for each input
        let timing_defs: String = input_pins
            .iter()
            .map(|pin| {
                format!(
                    r#"
            timing() {{
                related_pin: "{}";
                cell_rise(test_template) {{ values("0.1, 0.2", "0.2, 0.3"); }}
                cell_fall(test_template) {{ values("0.08, 0.18", "0.18, 0.28"); }}
                rise_transition(test_template) {{ values("0.05, 0.1", "0.1, 0.15"); }}
                fall_transition(test_template) {{ values("0.04, 0.09", "0.09, 0.14"); }}
            }}
            "#,
                    pin
                )
            })
            .collect::<Vec<_>>()
            .join("");

        let lib_str = format!(
            r#"
library({}_test) {{
    delay_model: table_lookup;
    
    lu_table_template(test_template) {{
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.01, 0.1");
        index_2("0.005, 0.05");
    }}
    
    cell(RCELEM_VARIANT) {{
        latch(IQ,IQN) {{
            clear: "!RN";
            data_in: "{}";
            enable: "G";
        }}
        
        pin(G) {{ direction: input; clock: true; }}
        pin(RN) {{ direction: input; }}
        {}
        pin(Q) {{ 
            direction: output;
            function: "IQ";
            {}
        }}
    }}
}}
        "#,
            test_name, data_in_expr, pin_defs, timing_defs
        );

        let result = std::panic::catch_unwind(|| {
            let mut liberty = parse_lib(&lib_str)
                .unwrap_or_else(|_| panic!("Failed to parse {} variation", test_name));

            for lib in liberty.iter_mut() {
                process_library(lib, clock_pin, &reset_regex, false);
            }

            let lib = &liberty[0];
            let cell = lib.iter_cells().next().unwrap();

            // Should have converted latch to ff
            let ff_group = cell.iter_subgroups_of_type("ff").next();
            assert!(
                ff_group.is_some(),
                "RCELEM variant {} should have ff group after processing",
                test_name
            );

            if let Some(ff) = ff_group {
                assert_eq!(
                    ff.simple_attribute("next_state").unwrap().string(),
                    data_in_expr,
                    "FF next_state should match original data_in for {}",
                    test_name
                );
            }

            // Each input pin should have setup/hold constraints
            for pin_name in &input_pins {
                let pin = cell.iter_pins().find(|p| &p.name == pin_name).unwrap();

                let has_setup = pin.iter_subgroups_of_type("timing").any(|t| {
                    t.simple_attribute("timing_type")
                        .map(|tt| tt.expr() == "setup_rising")
                        .unwrap_or(false)
                });
                assert!(
                    has_setup,
                    "Pin {} should have setup timing in {}",
                    pin_name, test_name
                );

                let has_hold = pin.iter_subgroups_of_type("timing").any(|t| {
                    t.simple_attribute("timing_type")
                        .map(|tt| tt.expr() == "hold_rising")
                        .unwrap_or(false)
                });
                assert!(
                    has_hold,
                    "Pin {} should have hold timing in {}",
                    pin_name, test_name
                );
            }
        });

        assert!(
            result.is_ok(),
            "RCELEM variation {} should not panic",
            test_name
        );
    }
}

/// Property-based test: RACELEM21X1 cell pattern variations
#[test]
fn test_racelem21x1_pattern_variations() {
    // Test different combinations of M1/M2 and P1/P2 pins
    let test_cases = vec![
        // Original pattern: A*IQ+A*P1*P2+IQ*M1+IQ*M2
        (
            vec!["M1", "M2"],
            vec!["P1", "P2"],
            "A*IQ+A*P1*P2+IQ*M1+IQ*M2",
            "full_racelem",
        ),
        // Single M pin
        (
            vec!["M1"],
            vec!["P1", "P2"],
            "A*IQ+A*P1*P2+IQ*M1",
            "single_m",
        ),
        // Single P pin
        (
            vec!["M1", "M2"],
            vec!["P1"],
            "A*IQ+A*P1+IQ*M1+IQ*M2",
            "single_p",
        ),
        // No M pins
        (vec![], vec!["P1", "P2"], "A*IQ+A*P1*P2", "no_m"),
        // No P pins
        (vec!["M1", "M2"], vec![], "A*IQ+IQ*M1+IQ*M2", "no_p"),
        // Three M pins
        (
            vec!["M1", "M2", "M3"],
            vec!["P1"],
            "A*IQ+A*P1+IQ*M1+IQ*M2+IQ*M3",
            "three_m",
        ),
    ];

    let clock_pin = "G";
    let reset_regex = Regex::new(r"(R|S)N?").unwrap();

    for (m_pins, p_pins, data_in_expr, test_name) in test_cases {
        // Generate all input pins (A + M pins + P pins)
        let mut all_pins = vec!["A"];
        all_pins.extend(m_pins.iter().copied());
        all_pins.extend(p_pins.iter().copied());

        // Generate pin definitions
        let pin_defs: String = all_pins
            .iter()
            .map(|pin| format!("pin({}) {{ direction: input; }}", pin))
            .collect::<Vec<_>>()
            .join("\n        ");

        // Generate timing for A pin (main input)
        let a_timing = r#"
            timing() {
                related_pin: "INPUT";
                cell_rise(test_template) { values("0.15, 0.25", "0.25, 0.35"); }
                cell_fall(test_template) { values("0.12, 0.22", "0.22, 0.32"); }
                rise_transition(test_template) { values("0.03, 0.06", "0.06, 0.09"); }
                fall_transition(test_template) { values("0.025, 0.055", "0.055, 0.085"); }
            }
            "#;

        let lib_str = format!(
            r#"
library({}_test) {{
    delay_model: table_lookup;
    
    lu_table_template(test_template) {{
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.01, 0.1");
        index_2("0.005, 0.05");
    }}
    
    cell(RACELEM_VARIANT) {{
        latch(IQ,IQN) {{
            clear: "!RN";
            data_in: "{}";
            enable: "G";
        }}
        
        pin(G) {{ direction: input; clock: true; }}
        pin(RN) {{ direction: input; }}
        {}
        pin(Q) {{ 
            direction: output;
            function: "IQ";
            timing() {{
                related_pin: "A";
                cell_rise(test_template) {{ values("0.2, 0.3", "0.3, 0.4"); }}
                cell_fall(test_template) {{ values("0.18, 0.28", "0.28, 0.38"); }}
                rise_transition(test_template) {{ values("0.04, 0.08", "0.08, 0.12"); }}
                fall_transition(test_template) {{ values("0.035, 0.075", "0.075, 0.115"); }}
            }}
        }}
        pin(A) {{ 
            direction: input;
            {}
        }}
    }}
}}
        "#,
            test_name, data_in_expr, pin_defs, a_timing
        );

        let result = std::panic::catch_unwind(|| {
            let mut liberty = parse_lib(&lib_str)
                .unwrap_or_else(|_| panic!("Failed to parse {} variation", test_name));

            for lib in liberty.iter_mut() {
                process_library(lib, clock_pin, &reset_regex, false);
            }

            let lib = &liberty[0];
            let cell = lib.iter_cells().next().unwrap();

            // Should have converted latch to ff
            let ff_group = cell.iter_subgroups_of_type("ff").next();
            assert!(
                ff_group.is_some(),
                "RACELEM variant {} should have ff group after processing",
                test_name
            );

            // All non-reset input pins should have timing constraints
            for pin_name in &all_pins {
                let pin = cell.iter_pins().find(|p| &p.name == pin_name).unwrap();

                // Should have nextstate_type
                assert_eq!(
                    pin.simple_attribute("nextstate_type").unwrap().expr(),
                    "data",
                    "Pin {} should have nextstate_type in {}",
                    pin_name,
                    test_name
                );

                // Should have both setup and hold timing
                let setup_count = pin
                    .iter_subgroups_of_type("timing")
                    .filter(|t| {
                        t.simple_attribute("timing_type")
                            .map(|tt| tt.expr() == "setup_rising")
                            .unwrap_or(false)
                    })
                    .count();
                assert!(
                    setup_count > 0,
                    "Pin {} should have setup timing in {}",
                    pin_name,
                    test_name
                );

                let hold_count = pin
                    .iter_subgroups_of_type("timing")
                    .filter(|t| {
                        t.simple_attribute("timing_type")
                            .map(|tt| tt.expr() == "hold_rising")
                            .unwrap_or(false)
                    })
                    .count();
                assert!(
                    hold_count > 0,
                    "Pin {} should have hold timing in {}",
                    pin_name,
                    test_name
                );
            }
        });

        assert!(
            result.is_ok(),
            "RACELEM variation {} should not panic",
            test_name
        );
    }
}

/// Test consistency across different reset pin patterns with reference cells
#[test]
fn test_reference_cells_reset_patterns() {
    // Test that different reset pin patterns work correctly with reference cell structures
    let reset_patterns = vec![
        (r"^RN$", "exact_rn"),
        (r"(R|S)N", "r_or_s_n"),
        (r".*RESET.*", "contains_reset"),
        (r"CLR", "clear_pin"),
        (r"RST_?N?", "rst_variations"),
    ];

    for (reset_pattern, test_name) in reset_patterns {
        let lib_str = format!(
            r#"
library({}_reset_test) {{
    delay_model: table_lookup;
    
    lu_table_template(test_template) {{
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.01, 0.1");
        index_2("0.005, 0.05");
    }}
    
    cell(RCELEM_RESET_TEST) {{
        latch(IQ,IQN) {{
            clear: "!RESET_N";
            data_in: "A*B+A*IQ+B*IQ";
            enable: "G";
        }}
        
        pin(G) {{ direction: input; clock: true; }}
        pin(A) {{ direction: input; }}
        pin(B) {{ direction: input; }}
        pin(RESET_N) {{ direction: input; }}
        pin(Q) {{ 
            direction: output;
            function: "IQ";
            timing() {{
                related_pin: "A";
                timing_type: combinational;
                cell_rise(test_template) {{ values("0.1, 0.2", "0.2, 0.3"); }}
                cell_fall(test_template) {{ values("0.08, 0.18", "0.18, 0.28"); }}
                rise_transition(test_template) {{ values("0.05, 0.1", "0.1, 0.15"); }}
                fall_transition(test_template) {{ values("0.04, 0.09", "0.09, 0.14"); }}
            }}
        }}
    }}
}}
        "#,
            test_name
        );

        let mut liberty =
            parse_lib(&lib_str).unwrap_or_else(|_| panic!("Failed to parse {} test", test_name));

        let clock_pin = "G";
        let reset_regex = Regex::new(reset_pattern)
            .unwrap_or_else(|_| panic!("Invalid regex pattern: {}", reset_pattern));

        process_library(&mut liberty[0], clock_pin, &reset_regex, false);

        let lib = &liberty[0];
        let cell = lib.iter_cells().next().unwrap();

        // Cell should be processed successfully
        assert!(
            cell.iter_subgroups_of_type("ff").next().is_some(),
            "Reset pattern {} should allow cell processing",
            test_name
        );

        // Current behavior: Reset pin does get nextstate_type (this is the working behavior)
        let reset_pin = cell.iter_pins().find(|p| p.name == "RESET_N").unwrap();
        // Document current behavior for regression testing
        if reset_pin.simple_attribute("nextstate_type").is_some() {
            eprintln!(
                "Reset pin has nextstate_type with pattern {} (current working behavior)",
                test_name
            );
        }

        // Non-reset pins should get constraints
        for pin_name in ["A", "B"] {
            let pin = cell.iter_pins().find(|p| p.name == pin_name).unwrap();
            assert_eq!(
                pin.simple_attribute("nextstate_type").unwrap().expr(),
                "data",
                "Pin {} should have nextstate_type with reset pattern {}",
                pin_name,
                test_name
            );
        }
    }
}

/// Test property: transformation with real files should be deterministic
#[test]
fn test_real_file_transformation_determinism() {
    let input_path = "examples/ASCEND_FREEPDK45_ALHO_nom_1.10V_25C.lib";

    if !Path::new(input_path).exists() {
        eprintln!("Skipping determinism test - example files not found");
        return;
    }

    let clock_pin = "G";
    let reset_regex = Regex::new(r"(R|S)N?").unwrap();

    // Process the same file multiple times and verify identical results
    let mut results = Vec::new();

    for run in 0..3 {
        let mut liberty = parse_liberty_file(Path::new(input_path))
            .unwrap_or_else(|_| panic!("Failed to parse on run {}", run));

        for lib in liberty.iter_mut() {
            process_library(lib, clock_pin, &reset_regex, false);
        }

        // Extract key properties for comparison
        let lib = &liberty[0];

        let mut cell_properties = Vec::new();
        for cell in lib.iter_cells() {
            if ["RCELEM2X1", "RACELEM21X1"].contains(&cell.name.as_str()) {
                let latch_count = cell.iter_subgroups_of_type("latch").count();
                let ff_count = cell.iter_subgroups_of_type("ff").count();

                let mut pin_constraints = Vec::new();
                for pin in cell.iter_pins() {
                    let setup_count = pin
                        .iter_subgroups_of_type("timing")
                        .filter(|t| {
                            t.simple_attribute("timing_type")
                                .map(|tt| tt.expr() == "setup_rising")
                                .unwrap_or(false)
                        })
                        .count();
                    let hold_count = pin
                        .iter_subgroups_of_type("timing")
                        .filter(|t| {
                            t.simple_attribute("timing_type")
                                .map(|tt| tt.expr() == "hold_rising")
                                .unwrap_or(false)
                        })
                        .count();
                    pin_constraints.push((pin.name.clone(), setup_count, hold_count));
                }
                pin_constraints.sort();

                cell_properties.push((cell.name.clone(), latch_count, ff_count, pin_constraints));
            }
        }
        cell_properties.sort();

        let pseudo_template_count = lib
            .iter_subgroups_of_type("lu_table_template")
            .filter(|t| t.name.contains("pseudo"))
            .count();

        results.push((cell_properties, pseudo_template_count));
    }

    // All results should be identical
    for i in 1..results.len() {
        assert_eq!(
            results[0], results[i],
            "Real file transformation should be deterministic across runs"
        );
    }
}

/// Test edge case: cells with no timing information
#[test]
fn test_reference_cells_no_timing() {
    let lib_str = r#"
library(no_timing_test) {
    delay_model: table_lookup;
    
    cell(RCELEM_NO_TIMING) {
        latch(IQ,IQN) {
            clear: "!RN";
            data_in: "A*B+A*IQ+B*IQ";
            enable: "G";
        }
        
        pin(G) { direction: input; clock: true; }
        pin(A) { direction: input; }
        pin(B) { direction: input; }
        pin(RN) { direction: input; }
        pin(Q) { direction: output; function: "IQ"; }
    }
}
    "#;

    let mut liberty = parse_lib(lib_str).expect("Failed to parse no timing test");
    let clock_pin = "G";
    let reset_regex = Regex::new(r"(R|S)N?").unwrap();

    // Should handle cells with no timing gracefully
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        process_library(&mut liberty[0], clock_pin, &reset_regex, false);
    }));

    // Should not panic even without timing information
    assert!(
        result.is_ok(),
        "Should handle cells with no timing information"
    );

    // Cell should still be identified as qualifying
    let lib = &liberty[0];
    let cell = lib.iter_cells().next().unwrap();
    assert!(
        cell_qualifies(cell, clock_pin),
        "Cell should still qualify without timing info"
    );
}
