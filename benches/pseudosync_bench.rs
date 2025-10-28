//! Performance benchmarks for pseudosync
//! Ensures that processing remains performant as codebase evolves
//! Includes benchmarks for RCELEM2X1 and RACELEM21X1 reference cells

use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use pseudosync::*;
use liberty_parse::parse_lib;
use regex::Regex;
use std::time::Duration;
use std::path::Path;

/// Generate a test library with varying numbers of latch cells
fn generate_test_library(num_cells: usize) -> String {
    let mut lib = String::from(r#"
library(benchmark_lib) {
    delay_model: table_lookup;
    time_unit: "1ns";
    
    lu_table_template(bench_template) {
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.01, 0.02, 0.03, 0.04, 0.05");
        index_2("0.001, 0.002, 0.003, 0.004, 0.005");
    }
"#);

    for i in 0..num_cells {
        let cell = format!(r#"
    cell(LATCH_CELL_{}) {{
        area: {};
        
        latch(IQ, IQN) {{
            enable: "CLK";
            data_in: "D";
            clear: "!RST";
        }}
        
        pin(CLK) {{
            direction: input;
            clock: true;
        }}
        
        pin(D) {{
            direction: input;
            timing() {{
                related_pin: "A";
                cell_rise(bench_template) {{
                    values ( \
                        "0.1, 0.12, 0.15, 0.18, 0.22", \
                        "0.11, 0.13, 0.16, 0.19, 0.23", \
                        "0.13, 0.15, 0.18, 0.21, 0.25", \
                        "0.16, 0.18, 0.21, 0.24, 0.28", \
                        "0.21, 0.23, 0.26, 0.29, 0.33" \
                    );
                }}
                cell_fall(bench_template) {{
                    values ( \
                        "0.08, 0.1, 0.13, 0.16, 0.2", \
                        "0.09, 0.11, 0.14, 0.17, 0.21", \
                        "0.11, 0.13, 0.16, 0.19, 0.23", \
                        "0.14, 0.16, 0.19, 0.22, 0.26", \
                        "0.19, 0.21, 0.24, 0.27, 0.31" \
                    );
                }}
                rise_transition(bench_template) {{
                    values ( \
                        "0.02, 0.025, 0.03, 0.035, 0.042", \
                        "0.022, 0.027, 0.032, 0.037, 0.044", \
                        "0.025, 0.03, 0.035, 0.04, 0.047", \
                        "0.03, 0.035, 0.04, 0.045, 0.052", \
                        "0.038, 0.043, 0.048, 0.053, 0.06" \
                    );
                }}
                fall_transition(bench_template) {{
                    values ( \
                        "0.018, 0.023, 0.028, 0.033, 0.04", \
                        "0.02, 0.025, 0.03, 0.035, 0.042", \
                        "0.023, 0.028, 0.033, 0.038, 0.045", \
                        "0.028, 0.033, 0.038, 0.043, 0.05", \
                        "0.036, 0.041, 0.046, 0.051, 0.058" \
                    );
                }}
            }}
            timing() {{
                related_pin: "B";
                cell_rise(bench_template) {{
                    values ( \
                        "0.12, 0.14, 0.17, 0.2, 0.24", \
                        "0.13, 0.15, 0.18, 0.21, 0.25", \
                        "0.15, 0.17, 0.2, 0.23, 0.27", \
                        "0.18, 0.2, 0.23, 0.26, 0.3", \
                        "0.23, 0.25, 0.28, 0.31, 0.35" \
                    );
                }}
                cell_fall(bench_template) {{
                    values ( \
                        "0.1, 0.12, 0.15, 0.18, 0.22", \
                        "0.11, 0.13, 0.16, 0.19, 0.23", \
                        "0.13, 0.15, 0.18, 0.21, 0.25", \
                        "0.16, 0.18, 0.21, 0.24, 0.28", \
                        "0.21, 0.23, 0.26, 0.29, 0.33" \
                    );
                }}
                rise_transition(bench_template) {{
                    values ( \
                        "0.022, 0.027, 0.032, 0.037, 0.044", \
                        "0.024, 0.029, 0.034, 0.039, 0.046", \
                        "0.027, 0.032, 0.037, 0.042, 0.049", \
                        "0.032, 0.037, 0.042, 0.047, 0.054", \
                        "0.04, 0.045, 0.05, 0.055, 0.062" \
                    );
                }}
                fall_transition(bench_template) {{
                    values ( \
                        "0.02, 0.025, 0.03, 0.035, 0.042", \
                        "0.022, 0.027, 0.032, 0.037, 0.044", \
                        "0.025, 0.03, 0.035, 0.04, 0.047", \
                        "0.03, 0.035, 0.04, 0.045, 0.052", \
                        "0.038, 0.043, 0.048, 0.053, 0.06" \
                    );
                }}
            }}
        }}
        
        pin(RST) {{
            direction: input;
        }}
        
        pin(Q) {{
            direction: output;
            function: "IQ";
            timing() {{
                related_pin: "CLK";
                timing_type: rising_edge;
                timing_sense: non_unate;
                cell_rise(bench_template) {{
                    values ( \
                        "0.15, 0.17, 0.2, 0.23, 0.27", \
                        "0.16, 0.18, 0.21, 0.24, 0.28", \
                        "0.18, 0.2, 0.23, 0.26, 0.3", \
                        "0.21, 0.23, 0.26, 0.29, 0.33", \
                        "0.26, 0.28, 0.31, 0.34, 0.38" \
                    );
                }}
                cell_fall(bench_template) {{
                    values ( \
                        "0.12, 0.14, 0.17, 0.2, 0.24", \
                        "0.13, 0.15, 0.18, 0.21, 0.25", \
                        "0.15, 0.17, 0.2, 0.23, 0.27", \
                        "0.18, 0.2, 0.23, 0.26, 0.3", \
                        "0.23, 0.25, 0.28, 0.31, 0.35" \
                    );
                }}
                rise_transition(bench_template) {{
                    values ( \
                        "0.025, 0.03, 0.035, 0.04, 0.047", \
                        "0.027, 0.032, 0.037, 0.042, 0.049", \
                        "0.03, 0.035, 0.04, 0.045, 0.052", \
                        "0.035, 0.04, 0.045, 0.05, 0.057", \
                        "0.043, 0.048, 0.053, 0.058, 0.065" \
                    );
                }}
                fall_transition(bench_template) {{
                    values ( \
                        "0.023, 0.028, 0.033, 0.038, 0.045", \
                        "0.025, 0.03, 0.035, 0.04, 0.047", \
                        "0.028, 0.033, 0.038, 0.043, 0.05", \
                        "0.033, 0.038, 0.043, 0.048, 0.055", \
                        "0.041, 0.046, 0.051, 0.056, 0.063" \
                    );
                }}
            }}
        }}
        
        pin(QN) {{
            direction: output;
            function: "IQN";
            timing() {{
                related_pin: "CLK";
                timing_type: rising_edge;
                timing_sense: non_unate;
                cell_rise(bench_template) {{
                    values ( \
                        "0.14, 0.16, 0.19, 0.22, 0.26", \
                        "0.15, 0.17, 0.2, 0.23, 0.27", \
                        "0.17, 0.19, 0.22, 0.25, 0.29", \
                        "0.2, 0.22, 0.25, 0.28, 0.32", \
                        "0.25, 0.27, 0.3, 0.33, 0.37" \
                    );
                }}
                cell_fall(bench_template) {{
                    values ( \
                        "0.11, 0.13, 0.16, 0.19, 0.23", \
                        "0.12, 0.14, 0.17, 0.2, 0.24", \
                        "0.14, 0.16, 0.19, 0.22, 0.26", \
                        "0.17, 0.19, 0.22, 0.25, 0.29", \
                        "0.22, 0.24, 0.27, 0.3, 0.34" \
                    );
                }}
                rise_transition(bench_template) {{
                    values ( \
                        "0.024, 0.029, 0.034, 0.039, 0.046", \
                        "0.026, 0.031, 0.036, 0.041, 0.048", \
                        "0.029, 0.034, 0.039, 0.044, 0.051", \
                        "0.034, 0.039, 0.044, 0.049, 0.056", \
                        "0.042, 0.047, 0.052, 0.057, 0.064" \
                    );
                }}
                fall_transition(bench_template) {{
                    values ( \
                        "0.022, 0.027, 0.032, 0.037, 0.044", \
                        "0.024, 0.029, 0.034, 0.039, 0.046", \
                        "0.027, 0.032, 0.037, 0.042, 0.049", \
                        "0.032, 0.037, 0.042, 0.047, 0.054", \
                        "0.04, 0.045, 0.05, 0.055, 0.062" \
                    );
                }}
            }}
        }}
    }}
"#, i, i as f64 * 1.5);
        lib.push_str(&cell);
    }
    
    lib.push_str("}\n");
    lib
}

/// Benchmark parsing Liberty files of different sizes
fn bench_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("parsing");
    
    for size in [1, 5, 10, 20, 50].iter() {
        let lib_str = generate_test_library(*size);
        
        group.bench_with_input(
            BenchmarkId::new("parse_liberty", size),
            &lib_str,
            |b, lib_str| {
                b.iter(|| {
                    black_box(parse_lib(lib_str).expect("Failed to parse"))
                });
            },
        );
    }
    
    group.finish();
}

/// Benchmark the main process_library function
fn bench_processing(c: &mut Criterion) {
    let mut group = c.benchmark_group("processing");
    group.measurement_time(Duration::from_secs(10));
    
    for size in [1, 5, 10, 20, 50].iter() {
        let lib_str = generate_test_library(*size);
        let liberty = parse_lib(&lib_str).expect("Failed to parse for benchmark");
        
        group.bench_with_input(
            BenchmarkId::new("process_ff_mode", size),
            &liberty,
            |b, liberty| {
                let clock_name = "CLK";
                let reset_name = Regex::new(r"RST").unwrap();
                
                b.iter(|| {
                    let mut lib_copy = black_box(liberty.clone());
                    black_box(process_library(&mut lib_copy[0], clock_name, &reset_name, false));
                });
            },
        );
        
        group.bench_with_input(
            BenchmarkId::new("process_latch_mode", size),
            &liberty,
            |b, liberty| {
                let clock_name = "CLK";
                let reset_name = Regex::new(r"RST").unwrap();
                
                b.iter(|| {
                    let mut lib_copy = black_box(liberty.clone());
                    black_box(process_library(&mut lib_copy[0], clock_name, &reset_name, true));
                });
            },
        );
    }
    
    group.finish();
}

/// Benchmark timing table processing
fn bench_timing_calculations(c: &mut Criterion) {
    use pseudosync::{mean_timingtable, mean_reference_arc, RefArc};
    use ndarray::Array1;
    use liberty_parse::liberty::Group;
    use liberty_parse::ast::Value;
    use liberty_parse::liberty::Attribute;
    
    let mut group = c.benchmark_group("timing_calculations");
    
    // Create test timing groups with various sizes
    for size in [3, 5, 10, 20].iter() {
        // Generate timing values
        let values: Vec<Value> = (0..*size)
            .map(|i| Value::FloatGroup((0..*size).map(|j| (i as f64 + 1.0) * 0.1 + (j as f64 + 1.0) * 0.01).collect()))
            .collect();
        
        let mut timing_group = Group::new("cell_rise", "test_template");
        timing_group.attributes.insert(
            "values".to_string(),
            vec![Attribute::Complex(values)]
        );
        
        let timing_groups = vec![&timing_group; 10]; // Multiple identical groups
        
        group.bench_with_input(
            BenchmarkId::new("mean_timing_table", format!("{}x{}", size, size)),
            &timing_groups,
            |b, groups| {
                b.iter(|| {
                    black_box(mean_timingtable(black_box(groups.iter().cloned())));
                });
            },
        );
    }
    
    // Benchmark RefArc calculations
    let ref_arcs: Vec<RefArc> = (0..10)
        .map(|i| RefArc {
            col: 2,
            row: 2,
            related_pin: format!("pin_{}", i),
            lut_template: "test_template".to_string(),
            rise_trans: Array1::from_vec((0..5).map(|j| (i as f64 + 1.0) * 0.01 + j as f64 * 0.001).collect()),
            fall_trans: Array1::from_vec((0..5).map(|j| (i as f64 + 1.0) * 0.01 + j as f64 * 0.001).collect()),
            cell_rise: Array1::from_vec((0..5).map(|j| (i as f64 + 1.0) * 0.1 + j as f64 * 0.01).collect()),
            cell_fall: Array1::from_vec((0..5).map(|j| (i as f64 + 1.0) * 0.1 + j as f64 * 0.01).collect()),
        })
        .collect();
        
    group.bench_function("mean_reference_arc", |b| {
        b.iter(|| {
            black_box(mean_reference_arc(black_box(ref_arcs.clone())));
        });
    });
    
    group.finish();
}

/// Benchmark complete end-to-end workflow
fn bench_end_to_end(c: &mut Criterion) {
    let mut group = c.benchmark_group("end_to_end");
    group.measurement_time(Duration::from_secs(15));
    
    for size in [1, 5, 10, 25].iter() {
        let lib_str = generate_test_library(*size);
        
        group.bench_with_input(
            BenchmarkId::new("complete_workflow", size),
            &lib_str,
            |b, lib_str| {
                let clock_name = "CLK";
                let reset_name = Regex::new(r"RST").unwrap();
                
                b.iter(|| {
                    // Complete workflow: parse -> process -> convert back
                    let mut liberty = black_box(parse_lib(lib_str).expect("Parse failed"));
                    black_box(process_library(&mut liberty[0], clock_name, &reset_name, false));
                    let _ast = black_box(liberty.to_ast());
                });
            },
        );
    }
    
    group.finish();
}

/// Benchmark memory usage patterns
fn bench_memory_patterns(c: &mut Criterion) {
    let mut group = c.benchmark_group("memory");
    
    // Test with a moderately sized library
    let lib_str = generate_test_library(10);
    let liberty = parse_lib(&lib_str).expect("Failed to parse");
    
    // Benchmark cloning (memory allocation)
    group.bench_function("clone_liberty", |b| {
        b.iter(|| {
            black_box(liberty.clone());
        });
    });
    
    // Benchmark AST conversion (memory transformation)
    group.bench_function("liberty_to_ast", |b| {
        b.iter(|| {
            let lib_copy = liberty.clone();
            black_box(lib_copy.to_ast());
        });
    });
    
    // Benchmark string serialization (memory to string)
    group.bench_function("ast_to_string", |b| {
        let ast = liberty.clone().to_ast();
        b.iter(|| {
            black_box(format!("{}", ast));
        });
    });
    
    group.finish();
}

/// Generate RCELEM2X1-like test cell for benchmarking
fn generate_rcelem2x1_cell() -> String {
    r#"
library(rcelem_bench) {
    delay_model: table_lookup;
    time_unit: "1ns";
    
    lu_table_template(delay_template_5x5) {
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.01, 0.05, 0.1, 0.2, 0.5");
        index_2("0.005, 0.01, 0.02, 0.05, 0.1");
    }
    
    cell(RCELEM2X1_BENCH) {
        area: 12.5;
        cell_leakage_power: 111.501;
        
        latch(IQ,IQN) {
            clear: "!RN";
            data_in: "A*B+A*IQ+B*IQ";
            enable: "G";
        }
        
        pin(G) {
            direction: input;
            clock: true;
            capacitance: 0.008;
        }
        
        pin(A) {
            direction: input;
            capacitance: 0.005;
            timing() {
                related_pin: "INPUT";
                cell_rise(delay_template_5x5) {
                    values (
                        "0.1,0.12,0.15,0.18,0.22",
                        "0.11,0.13,0.16,0.19,0.23",
                        "0.13,0.15,0.18,0.21,0.25",
                        "0.16,0.18,0.21,0.24,0.28",
                        "0.21,0.23,0.26,0.29,0.33"
                    );
                }
                cell_fall(delay_template_5x5) {
                    values (
                        "0.08,0.1,0.13,0.16,0.2",
                        "0.09,0.11,0.14,0.17,0.21",
                        "0.11,0.13,0.16,0.19,0.23",
                        "0.14,0.16,0.19,0.22,0.26",
                        "0.19,0.21,0.24,0.27,0.31"
                    );
                }
                rise_transition(delay_template_5x5) {
                    values (
                        "0.02,0.025,0.03,0.035,0.042",
                        "0.022,0.027,0.032,0.037,0.044",
                        "0.025,0.03,0.035,0.04,0.047",
                        "0.03,0.035,0.04,0.045,0.052",
                        "0.038,0.043,0.048,0.053,0.06"
                    );
                }
                fall_transition(delay_template_5x5) {
                    values (
                        "0.018,0.023,0.028,0.033,0.04",
                        "0.02,0.025,0.03,0.035,0.042",
                        "0.023,0.028,0.033,0.038,0.045",
                        "0.028,0.033,0.038,0.043,0.05",
                        "0.036,0.041,0.046,0.051,0.058"
                    );
                }
            }
        }
        
        pin(B) {
            direction: input;
            capacitance: 0.005;
            timing() {
                related_pin: "INPUT";
                cell_rise(delay_template_5x5) {
                    values (
                        "0.12,0.14,0.17,0.2,0.24",
                        "0.13,0.15,0.18,0.21,0.25",
                        "0.15,0.17,0.2,0.23,0.27",
                        "0.18,0.2,0.23,0.26,0.3",
                        "0.23,0.25,0.28,0.31,0.35"
                    );
                }
                cell_fall(delay_template_5x5) {
                    values (
                        "0.09,0.11,0.14,0.17,0.21",
                        "0.1,0.12,0.15,0.18,0.22",
                        "0.12,0.14,0.17,0.2,0.24",
                        "0.15,0.17,0.2,0.23,0.27",
                        "0.2,0.22,0.25,0.28,0.32"
                    );
                }
                rise_transition(delay_template_5x5) {
                    values (
                        "0.025,0.03,0.035,0.04,0.047",
                        "0.027,0.032,0.037,0.042,0.049",
                        "0.03,0.035,0.04,0.045,0.052",
                        "0.035,0.04,0.045,0.05,0.057",
                        "0.043,0.048,0.053,0.058,0.065"
                    );
                }
                fall_transition(delay_template_5x5) {
                    values (
                        "0.02,0.025,0.03,0.035,0.042",
                        "0.022,0.027,0.032,0.037,0.044",
                        "0.025,0.03,0.035,0.04,0.047",
                        "0.03,0.035,0.04,0.045,0.052",
                        "0.038,0.043,0.048,0.053,0.06"
                    );
                }
            }
        }
        
        pin(RN) { direction: input; capacitance: 0.003; }
        
        pin(Q) {
            direction: output;
            function: "IQ";
            max_capacitance: 0.05;
            timing() {
                related_pin: "G";
                timing_sense: non_unate;
                timing_type: rising_edge;
                cell_rise(delay_template_5x5) {
                    values (
                        "0.15,0.17,0.2,0.23,0.27",
                        "0.16,0.18,0.21,0.24,0.28",
                        "0.18,0.2,0.23,0.26,0.3",
                        "0.21,0.23,0.26,0.29,0.33",
                        "0.26,0.28,0.31,0.34,0.38"
                    );
                }
                cell_fall(delay_template_5x5) {
                    values (
                        "0.12,0.14,0.17,0.2,0.24",
                        "0.13,0.15,0.18,0.21,0.25",
                        "0.15,0.17,0.2,0.23,0.27",
                        "0.18,0.2,0.23,0.26,0.3",
                        "0.23,0.25,0.28,0.31,0.35"
                    );
                }
                rise_transition(delay_template_5x5) {
                    values (
                        "0.025,0.03,0.035,0.04,0.047",
                        "0.027,0.032,0.037,0.042,0.049",
                        "0.03,0.035,0.04,0.045,0.052",
                        "0.035,0.04,0.045,0.05,0.057",
                        "0.043,0.048,0.053,0.058,0.065"
                    );
                }
                fall_transition(delay_template_5x5) {
                    values (
                        "0.023,0.028,0.033,0.038,0.045",
                        "0.025,0.03,0.035,0.04,0.047",
                        "0.028,0.033,0.038,0.043,0.05",
                        "0.033,0.038,0.043,0.048,0.055",
                        "0.041,0.046,0.051,0.056,0.063"
                    );
                }
            }
        }
    }
}
    "#.to_string()
}

/// Generate RACELEM21X1-like test cell for benchmarking
fn generate_racelem21x1_cell() -> String {
    r#"
library(racelem_bench) {
    delay_model: table_lookup;
    time_unit: "1ns";
    
    lu_table_template(delay_template_3x3) {
        variable_1: input_net_transition;
        variable_2: total_output_net_capacitance;
        index_1("0.01, 0.1, 0.5");
        index_2("0.01, 0.05, 0.1");
    }
    
    cell(RACELEM21X1_BENCH) {
        area: 15.8;
        cell_leakage_power: 124.067;
        
        latch(IQ,IQN) {
            clear: "!RN";
            data_in: "A*IQ+A*P1*P2+IQ*M1+IQ*M2";
            enable: "G";
        }
        
        pin(G) {
            direction: input;
            clock: true;
            capacitance: 0.008;
        }
        
        pin(A) {
            direction: input;
            capacitance: 0.004;
            timing() {
                related_pin: "INPUT";
                cell_rise(delay_template_3x3) {
                    values (
                        "0.15,0.2,0.25",
                        "0.18,0.23,0.28",
                        "0.25,0.3,0.35"
                    );
                }
                cell_fall(delay_template_3x3) {
                    values (
                        "0.12,0.17,0.22",
                        "0.15,0.2,0.25",
                        "0.22,0.27,0.32"
                    );
                }
                rise_transition(delay_template_3x3) {
                    values (
                        "0.03,0.04,0.05",
                        "0.035,0.045,0.055",
                        "0.05,0.06,0.07"
                    );
                }
                fall_transition(delay_template_3x3) {
                    values (
                        "0.025,0.035,0.045",
                        "0.03,0.04,0.05",
                        "0.045,0.055,0.065"
                    );
                }
            }
        }
        
        pin(M1) { direction: input; capacitance: 0.003; }
        pin(M2) { direction: input; capacitance: 0.003; }
        pin(P1) { direction: input; capacitance: 0.003; }
        pin(P2) { direction: input; capacitance: 0.003; }
        pin(RN) { direction: input; capacitance: 0.002; }
        
        pin(Q) {
            direction: output;
            function: "IQ";
            max_capacitance: 0.05;
            timing() {
                related_pin: "A";
                cell_rise(delay_template_3x3) {
                    values (
                        "0.2,0.25,0.3",
                        "0.23,0.28,0.33",
                        "0.3,0.35,0.4"
                    );
                }
                cell_fall(delay_template_3x3) {
                    values (
                        "0.18,0.23,0.28",
                        "0.21,0.26,0.31",
                        "0.28,0.33,0.38"
                    );
                }
                rise_transition(delay_template_3x3) {
                    values (
                        "0.04,0.05,0.06",
                        "0.045,0.055,0.065",
                        "0.06,0.07,0.08"
                    );
                }
                fall_transition(delay_template_3x3) {
                    values (
                        "0.035,0.045,0.055",
                        "0.04,0.05,0.06",
                        "0.055,0.065,0.075"
                    );
                }
            }
        }
    }
}
    "#.to_string()
}

/// Benchmark processing reference cell patterns
fn bench_reference_cells(c: &mut Criterion) {
    let mut group = c.benchmark_group("reference_cells");
    group.measurement_time(Duration::from_secs(10));
    
    let rcelem_lib = generate_rcelem2x1_cell();
    let racelem_lib = generate_racelem21x1_cell();
    
    // Parse libraries once for benchmarking
    let rcelem_liberty = parse_lib(&rcelem_lib).expect("Failed to parse RCELEM2X1");
    let racelem_liberty = parse_lib(&racelem_lib).expect("Failed to parse RACELEM21X1");
    
    let clock_name = "G";
    let reset_name = Regex::new(r"(R|S)N?").unwrap();
    
    // Benchmark RCELEM2X1 processing
    group.bench_function("rcelem2x1_ff_mode", |b| {
        b.iter(|| {
            let mut lib_copy = black_box(rcelem_liberty.clone());
            black_box(process_library(&mut lib_copy[0], clock_name, &reset_name, false));
        });
    });
    
    group.bench_function("rcelem2x1_latch_mode", |b| {
        b.iter(|| {
            let mut lib_copy = black_box(rcelem_liberty.clone());
            black_box(process_library(&mut lib_copy[0], clock_name, &reset_name, true));
        });
    });
    
    // Benchmark RACELEM21X1 processing  
    group.bench_function("racelem21x1_ff_mode", |b| {
        b.iter(|| {
            let mut lib_copy = black_box(racelem_liberty.clone());
            black_box(process_library(&mut lib_copy[0], clock_name, &reset_name, false));
        });
    });
    
    group.bench_function("racelem21x1_latch_mode", |b| {
        b.iter(|| {
            let mut lib_copy = black_box(racelem_liberty.clone());
            black_box(process_library(&mut lib_copy[0], clock_name, &reset_name, true));
        });
    });
    
    // Benchmark cell qualification for reference cells
    let rcelem_cell = rcelem_liberty[0].get_cell("RCELEM2X1_BENCH").unwrap();
    let racelem_cell = racelem_liberty[0].get_cell("RACELEM21X1_BENCH").unwrap();
    
    group.bench_function("rcelem2x1_qualification", |b| {
        b.iter(|| {
            black_box(cell_qualifies(black_box(rcelem_cell), black_box(clock_name)));
        });
    });
    
    group.bench_function("racelem21x1_qualification", |b| {
        b.iter(|| {
            black_box(cell_qualifies(black_box(racelem_cell), black_box(clock_name)));
        });
    });
    
    group.finish();
}

/// Benchmark real file processing if available
fn bench_real_file(c: &mut Criterion) {
    let input_path = "/Users/msartori/Developer/pseudosync/examples/ASCEND_FREEPDK45_ALHO_nom_1.10V_25C.lib";
    
    if !Path::new(input_path).exists() {
        eprintln!("Skipping real file benchmark - example file not found");
        return;
    }
    
    let mut group = c.benchmark_group("real_file");
    group.sample_size(10); // Reduce sample size for large files
    group.measurement_time(Duration::from_secs(30));
    
    group.bench_function("parse_large_liberty_file", |b| {
        b.iter(|| {
            let liberty = parse_liberty_file(black_box(Path::new(input_path)));
            black_box(liberty)
        })
    });
    
    // Only benchmark processing if parsing succeeds
    if let Ok(liberty) = parse_liberty_file(Path::new(input_path)) {
        group.bench_function("process_large_library_ff_mode", |b| {
            let clock_name = "G";
            let reset_name = Regex::new(r"(R|S)N?").unwrap();
            
            b.iter(|| {
                let mut lib_copy = black_box(liberty.clone());
                for lib in lib_copy.iter_mut() {
                    black_box(process_library(lib, clock_name, &reset_name, false));
                }
            });
        });
    }
    
    group.finish();
}

criterion_group!(
    benches,
    bench_parsing,
    bench_processing,
    bench_timing_calculations,
    bench_end_to_end,
    bench_memory_patterns,
    bench_reference_cells,
    bench_real_file
);
criterion_main!(benches);
