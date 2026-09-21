use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

const ENUM: &str = r#"
#[repr(i32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode { Slow = 0, Fast = 1 }
impl Mode {
    pub const fn from_raw(value: i32) -> Option<Self> {
        match value { 0 => Some(Self::Slow), 1 => Some(Self::Fast), _ => None }
    }
    pub const fn to_raw(self) -> i32 { self as i32 }
}
pub const MODE_SLOW: i32 = Mode::Slow.to_raw();
pub const MODE_FAST: i32 = Mode::Fast.to_raw();
"#;

fn project(source: &str) -> TempDir {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='enum-hoist-fixture'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/lib.rs"),
        format!("{ENUM}\n{source}"),
    )
    .unwrap();
    project
}

fn position(source: &str, needle: &str) -> (usize, usize) {
    let offset = source.find(needle).unwrap();
    let line = source[..offset]
        .bytes()
        .filter(|byte| *byte == b'\n')
        .count()
        + 1;
    let line_start = source[..offset].rfind('\n').map_or(0, |index| index + 1);
    (line, offset - line_start + 1)
}

fn run_write(project: &TempDir, seed_kind: &str, needle: &str) -> Value {
    let path = project.path().join("src/lib.rs");
    let source = fs::read_to_string(&path).unwrap();
    let (line, column) = position(&source, needle);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "enum-hoist",
            "--enum-file",
            "src/lib.rs",
            "--enum-name",
            "Mode",
            seed_kind,
            &format!("src/lib.rs:{line}:{column}"),
            "--write",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn hoists_a_closed_parameter_chain_to_the_variant_source() {
    let project = project(
        r#"
pub fn classify(mode: i32) -> bool {
    Mode::from_raw(mode) == Some(Mode::Fast)
}
pub fn relay(mode: i32) -> bool { classify(mode) }
pub fn root() -> bool { relay(MODE_FAST) }
"#,
    );
    let result = run_write(&project, "--parameter", "mode: i32");
    assert_eq!(result["status"], "applied");
    assert_eq!(
        result["target"]["typed_places"].as_array().unwrap().len(),
        2
    );
    let output = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    assert!(output.contains("fn classify(mode: Mode)"), "{output}");
    assert!(output.contains("mode == Mode::Fast"), "{output}");
    assert!(output.contains("fn relay(mode: Mode)"), "{output}");
    assert!(output.contains("relay(Mode::Fast)"), "{output}");
    assert!(!output.contains("Mode::from_raw(mode)"), "{output}");
}

#[test]
fn hoists_through_an_inferred_local_and_function_return() {
    let project = project(
        r#"
pub fn classify(mode: i32) -> bool {
    Mode::from_raw(mode) == Some(Mode::Fast)
        && Mode::from_raw(mode).is_some()
        && Mode::from_raw(mode).is_some()
}
pub fn produce(mode: i32) -> i32 {
    let local = mode;
    local
}
pub fn root() -> bool { classify(produce(MODE_FAST)) }
pub fn check() -> bool { produce(MODE_SLOW) == MODE_SLOW }
pub fn raw_sink() -> i32 { std::hint::black_box::<i32>(produce(MODE_FAST)) }
pub fn validate() -> bool { Mode::from_raw(produce(MODE_FAST)).is_some() }
"#,
    );
    let result = run_write(&project, "--parameter", "mode: i32");
    assert_eq!(result["status"], "applied");
    assert_eq!(
        result["target"]["typed_places"].as_array().unwrap().len(),
        4
    );
    let output = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    assert!(output.contains("fn classify(mode: Mode)"), "{output}");
    assert!(
        output.contains("fn produce(mode: Mode) -> Mode"),
        "{output}"
    );
    assert!(output.contains("produce(Mode::Fast)"), "{output}");
    assert!(
        output.contains("produce(Mode::Slow) == Mode::Slow"),
        "{output}"
    );
    assert!(output.contains("produce(Mode::Fast).to_raw()"), "{output}");
    assert!(!output.contains("Mode::from_raw(produce"), "{output}");
    assert!(!output.contains("Mode::from_raw(mode)"), "{output}");
}

#[test]
fn hoists_all_writes_to_an_inferred_local() {
    let project = project(
        r#"
pub fn classify(mode: i32) -> bool { Mode::from_raw(mode).is_some() }
pub fn root(change: bool) -> bool {
    let mut mode = MODE_SLOW;
    if change { mode = MODE_FAST; }
    classify(mode)
}
"#,
    );
    let result = run_write(&project, "--parameter", "mode: i32");
    assert_eq!(result["status"], "applied");
    let output = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    assert!(output.contains("let mut mode = Mode::Slow"), "{output}");
    assert!(output.contains("mode = Mode::Fast"), "{output}");
    assert!(output.contains("classify(mode)"), "{output}");
}

#[test]
fn accepts_a_local_as_the_initial_seed() {
    let project = project(
        r#"
pub fn root() -> bool {
    let mode = MODE_FAST;
    Mode::from_raw(mode).is_some()
}
"#,
    );
    run_write(&project, "--local", "mode =");
    let output = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    assert!(output.contains("let mode = Mode::Fast"), "{output}");
    assert!(!output.contains("Mode::from_raw(mode)"), "{output}");
}

#[test]
fn accepts_a_function_return_as_the_initial_seed() {
    let project = project(
        r#"
pub fn produce() -> i32 { MODE_FAST }
pub fn root() -> bool { Mode::from_raw(produce()).is_some() }
"#,
    );
    run_write(&project, "--return", "produce");
    let output = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    assert!(
        output.contains("fn produce() -> Mode {\n    Mode::Fast\n}"),
        "{output}"
    );
    assert!(!output.contains("Mode::from_raw(produce())"), "{output}");
}

#[test]
fn keeps_one_existing_validation_at_the_raw_boundary() {
    let project = project(
        r#"
pub fn relay(mode: i32) -> bool {
    Mode::from_raw(mode) == Some(Mode::Fast)
}
pub fn parse(raw: i32) -> bool {
    let Some(mode) = Mode::from_raw(raw) else { return false };
    relay(mode.to_raw())
}
"#,
    );
    run_write(&project, "--parameter", "mode: i32");
    let output = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    assert!(output.contains("fn relay(mode: Mode)"), "{output}");
    assert!(
        output.contains("let Some(mode) = Mode::from_raw(raw)"),
        "{output}"
    );
    assert!(output.contains("relay(mode)"), "{output}");
    assert_eq!(output.matches("Mode::from_raw(").count(), 1, "{output}");
}

#[test]
fn hoists_a_field_setter_initializers_and_reads_atomically() {
    let project = project(
        r#"
pub struct Window { pub axis: i32 }
impl Window {
    pub fn new() -> Self { Self { axis: MODE_SLOW } }
    pub fn set_axis(&mut self, axis: i32) { self.axis = axis; }
    pub fn draw(&self) -> bool {
        match Mode::from_raw(self.axis) {
            Some(Mode::Slow) => false,
            Some(Mode::Fast) => true,
            _ => false,
        }
    }
}
pub fn event(window: &mut Window) { window.set_axis(MODE_FAST); }
"#,
    );
    let result = run_write(&project, "--field", "axis: i32");
    assert_eq!(
        result["target"]["typed_places"].as_array().unwrap().len(),
        2
    );
    let output = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    assert!(output.contains("pub axis: Mode"), "{output}");
    assert!(output.contains("axis: Mode::Slow"), "{output}");
    assert!(
        output.contains("fn set_axis(&mut self, axis: Mode)"),
        "{output}"
    );
    assert!(output.contains("set_axis(Mode::Fast)"), "{output}");
    assert!(output.contains("match self.axis"), "{output}");
    assert!(output.contains("Mode::Slow => false"), "{output}");
}

#[test]
fn refuses_a_nonvariant_argument_without_writing() {
    let project = project(
        r#"
pub fn classify(mode: i32) -> bool {
    Mode::from_raw(mode) == Some(Mode::Fast)
}
pub fn root(raw: i32) -> bool { classify(raw + 1) }
"#,
    );
    let path = project.path().join("src/lib.rs");
    let before = fs::read_to_string(&path).unwrap();
    let (line, column) = position(&before, "mode: i32");
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "enum-hoist",
            "--enum-file",
            "src/lib.rs",
            "--enum-name",
            "Mode",
            "--parameter",
            &format!("src/lib.rs:{line}:{column}"),
            "--write",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["diagnostics"][0]["code"],
        "UNSUPPORTED_ARGUMENT_SOURCE"
    );
    assert_eq!(fs::read_to_string(path).unwrap(), before);
}

#[test]
fn stats_reports_repeated_parameter_and_field_conversions() {
    let project = project(
        r#"
pub struct Window { pub axis: i32 }
pub fn classify(mode: i32, window: &Window) -> bool {
    let first = Mode::from_raw(mode) == Some(Mode::Fast);
    let second = Mode::from_raw(mode).is_some();
    first && second && Mode::from_raw(window.axis).is_some()
}
"#,
    );
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "enum-hoist-stats",
            "--enum-file",
            "src/lib.rs",
            "--enum-name",
            "Mode",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["candidate_count"], 2, "{result:#}");
    assert_eq!(result["candidates"][0]["kind"], "parameter");
    assert_eq!(result["candidates"][0]["subject"], "mode");
    assert_eq!(result["candidates"][0]["from_raw_count"], 2);
    assert_eq!(result["candidates"][1]["kind"], "field");
    assert_eq!(result["candidates"][1]["subject"], "axis");
}

fn run_refusal(project: &TempDir, seed_kind: &str, needle: &str) -> Value {
    let source = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    let (line, column) = position(&source, needle);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "enum-hoist",
            "--enum-file",
            "src/lib.rs",
            "--enum-name",
            "Mode",
            seed_kind,
            &format!("src/lib.rs:{line}:{column}"),
            "--dry-run",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn refuses_a_function_value_reference_atomically() {
    let project = project(
        r#"
pub fn classify(mode: i32) -> bool { Mode::from_raw(mode).is_some() }
pub fn root() -> bool {
    let callback: fn(i32) -> bool = classify;
    callback(MODE_FAST)
}
"#,
    );
    let result = run_refusal(&project, "--parameter", "mode: i32");
    assert_eq!(
        result["diagnostics"][0]["code"],
        "UNRESOLVED_FUNCTION_REFERENCE"
    );
}

#[test]
fn refuses_an_explicit_invalid_value_branch() {
    let project = project(
        r#"
pub fn classify(mode: i32) -> bool {
    match Mode::from_raw(mode) {
        Some(Mode::Fast) => true,
        Some(Mode::Slow) => false,
        None => false,
    }
}
pub fn root() -> bool { classify(MODE_FAST) }
"#,
    );
    let result = run_refusal(&project, "--parameter", "mode: i32");
    assert_eq!(
        result["diagnostics"][0]["code"],
        "INVALID_VALUE_POLICY_REQUIRED"
    );
}

#[test]
fn refuses_derived_default_for_a_migrated_field() {
    let project = project(
        r#"
#[derive(Default)]
pub struct Window { pub axis: i32 }
impl Window {
    pub fn draw(&self) -> bool { Mode::from_raw(self.axis).is_some() }
}
"#,
    );
    let result = run_refusal(&project, "--field", "axis: i32");
    assert_eq!(result["diagnostics"][0]["code"], "UNHANDLED_FIELD_WRITE");
}

#[test]
fn hoists_across_modules_with_an_explicit_enum_path() {
    let project = TempDir::new().unwrap();
    fs::create_dir(project.path().join("src")).unwrap();
    fs::write(
        project.path().join("Cargo.toml"),
        "[package]\nname='enum-hoist-modules'\nversion='0.1.0'\nedition='2021'\n",
    )
    .unwrap();
    fs::write(
        project.path().join("src/lib.rs"),
        "pub mod mode; pub mod worker;\n",
    )
    .unwrap();
    fs::write(project.path().join("src/mode.rs"), ENUM).unwrap();
    fs::write(
        project.path().join("src/worker.rs"),
        r#"use crate::mode::{Mode, MODE_FAST};
pub fn classify(mode: i32) -> bool {
    Mode::from_raw(mode) == Some(Mode::Fast)
}
pub fn root() -> bool { classify(MODE_FAST) }
"#,
    )
    .unwrap();
    let source = fs::read_to_string(project.path().join("src/worker.rs")).unwrap();
    let (line, column) = position(&source, "mode: i32");
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "enum-hoist",
            "--enum-file",
            "src/mode.rs",
            "--enum-name",
            "Mode",
            "--enum-path",
            "crate::mode::Mode",
            "--parameter",
            &format!("src/worker.rs:{line}:{column}"),
            "--write",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let output = fs::read_to_string(project.path().join("src/worker.rs")).unwrap();
    assert!(output.contains("mode: crate::mode::Mode"), "{output}");
    assert!(output.contains("mode == Mode::Fast"), "{output}");
    assert!(
        output.contains("classify(crate::mode::Mode::Fast)"),
        "{output}"
    );
}

#[test]
fn keeps_one_conversion_at_an_unavoidable_raw_sink() {
    let project = project(
        r#"
pub fn classify(mode: i32) -> bool {
    let known = Mode::from_raw(mode).is_some() && Mode::from_raw(mode).is_some();
    std::hint::black_box::<i32>(mode);
    known
}
pub fn root() -> bool { classify(MODE_FAST) }
"#,
    );
    run_write(&project, "--parameter", "mode: i32");
    let output = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    assert!(output.contains("fn classify(mode: Mode)"), "{output}");
    assert!(
        output.contains("black_box::<i32>(mode.to_raw())"),
        "{output}"
    );
    assert!(!output.contains("Mode::from_raw(mode)"), "{output}");
}

#[test]
fn verification_failure_restores_every_touched_file() {
    let project = project(
        r#"
pub fn classify(mode: i32) -> bool {
    Mode::from_raw(mode) == Some(Mode::Fast)
}
pub fn root() -> bool { classify(MODE_FAST) }
compile_error!("verification must fail");
"#,
    );
    let path = project.path().join("src/lib.rs");
    let before = fs::read_to_string(&path).unwrap();
    let (line, column) = position(&before, "mode: i32");
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "enum-hoist",
            "--enum-file",
            "src/lib.rs",
            "--enum-name",
            "Mode",
            "--parameter",
            &format!("src/lib.rs:{line}:{column}"),
            "--write",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(project.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["diagnostics"][0]["code"], "OPERATION_FAILED");
    assert_eq!(fs::read_to_string(path).unwrap(), before);
}

#[test]
fn hoists_a_mutually_recursive_parameter_component_atomically() {
    let project = project(
        r#"
pub fn left(mode: i32, depth: usize) -> bool {
    if depth == 0 { Mode::from_raw(mode).is_some() } else { right(mode, depth - 1) }
}
pub fn right(mode: i32, depth: usize) -> bool { left(mode, depth) }
pub fn root() -> bool { left(MODE_FAST, 2) }
"#,
    );
    let result = run_write(&project, "--parameter", "mode: i32");
    assert_eq!(
        result["target"]["typed_places"].as_array().unwrap().len(),
        2
    );
    let output = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    assert!(output.contains("fn left(mode: Mode"), "{output}");
    assert!(output.contains("fn right(mode: Mode"), "{output}");
    assert!(output.contains("left(Mode::Fast, 2)"), "{output}");
}
