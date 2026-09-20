use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use tempfile::TempDir;

fn fixture(files: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().unwrap();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"oop_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    for (name, contents) in files {
        let path = dir.path().join("src").join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    dir
}

fn run(dir: &TempDir, file: &str, line: usize, mode: &str) -> (i32, Value) {
    run_at(dir, file, line, 4, mode)
}

fn run_at(dir: &TempDir, file: &str, line: usize, column: usize, mode: &str) -> (i32, Value) {
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            file,
            "--line",
            &line.to_string(),
            "--column",
            &column.to_string(),
            mode,
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    (output.status.code().unwrap(), value)
}

#[test]
fn moves_public_function_and_updates_cross_module_calls() {
    let dir = fixture(&[
        ("lib.rs", "mod user;\npub struct Point { pub x: i32 }\n\npub fn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\n\npub fn run(p: Point) -> i32 { user::run(p) }\n"),
        ("user.rs", "use crate::shift;\npub fn run(p: crate::Point) -> i32 { shift(&p, 2) }\n"),
    ]);
    let (code, output) = run_at(&dir, "src/lib.rs", 4, 8, "--write");
    assert_eq!(code, 0, "{output}");
    let lib = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    let user = fs::read_to_string(dir.path().join("src/user.rs")).unwrap();
    assert!(lib.contains("pub fn shift(&self, dx: i32)"));
    assert!(!lib.contains("pub fn shift(point:"));
    assert!(user.contains("p.shift(2)"));
    assert!(!user.contains("use crate::shift"));
}

#[test]
fn stats_groups_receivers_and_struct_selector_batches_direct_functions() {
    let source = "pub struct Point { pub x: i32 }\n\npub fn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\npub fn scale(point: &Point, factor: i32) -> i32 { point.x * factor }\npub fn optional(point: Option<&Point>) -> i32 { point.map_or(0, |p| p.x) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let stats = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop-stats",
            "--struct",
            "Point",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    assert!(stats.status.success());
    let data: Value = serde_json::from_slice(&stats.stdout).unwrap();
    assert_eq!(data["groups"][0]["count"], 3);
    assert_eq!(data["groups"][0]["selectable_count"], 2);
    assert_eq!(
        data["groups"][0]["functions"][0]["selection"],
        "src/lib.rs:3:8"
    );

    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--struct",
            "Point",
            "--write",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{result}");
    assert_eq!(result["targets"].as_array().unwrap().len(), 2);
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("pub fn shift(&self"));
    assert!(updated.contains("pub fn scale(&self"));
    assert!(updated.contains("pub fn optional(point: Option<&Point>)"));
}

#[test]
fn fast_write_still_compiles_and_moves_public_function() {
    let dir = fixture(&[("lib.rs", "pub struct Point { pub x: i32 }\npub fn shift(point: &Point) -> i32 { point.x }\npub fn run(p: Point) -> i32 { shift(&p) }\n")]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "2",
            "--column",
            "8",
            "--write",
            "--fast",
            "--check",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{result}");
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("pub fn shift(&self)"));
    assert!(updated.contains("p.shift()"));
}

#[test]
fn fast_write_does_not_require_workspace_cargo_check() {
    let source = "struct Point { x: i32 }\nfn shift(point: &Point) -> i32 { point.x }\nfn run(p: Point) -> i32 { shift(&p) }\nfn unrelated() { missing_symbol(); }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "2",
            "--column",
            "4",
            "--fast",
            "--write",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{result}");
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("p.shift()"));
    assert!(updated.contains("missing_symbol()"));
}

#[test]
fn moves_unsafe_free_function_to_unsafe_method() {
    let source = "struct Point { x: i32 }\nunsafe fn read(point: &Point) -> i32 { point.x }\nfn run(p: Point) -> i32 { unsafe { read(&p) } }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "2",
            "--column",
            "11",
            "--fast",
            "--write",
            "--check",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{result}");
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("unsafe fn read(&self)"));
    assert!(updated.contains("unsafe { p.read() }"));
}

#[test]
fn moves_function_containing_cfg_macro() {
    let source = "struct Point { x: i32 }\nfn value(point: &Point) -> i32 { eprintln!(\"{}\", point.x); if cfg!(target_os = \"linux\") { point.x } else { 0 } }\nfn run(p: Point) -> i32 { value(&p) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "2",
            "--column",
            "4",
            "--fast",
            "--write",
            "--check",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{result}");
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("cfg!(target_os = \"linux\")"));
    assert!(updated.contains("eprintln!(\"{}\", self.x)"));
    assert!(updated.contains("p.value()"));
}

#[test]
fn rewrites_receiver_inside_format_and_vec_macros() {
    let source = "struct Point { x: i32 }\nfn values(point: &Point) -> Vec<i32> { vec![point.x] }\nfn label(point: &Point) -> String { format!(\"{}\", point.x) }\nfn run(p: Point) -> (Vec<i32>, String) { (values(&p), label(&p)) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "2",
            "--line",
            "3",
            "--column",
            "4",
            "--fast",
            "--write",
            "--check",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{result}");
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("vec![self.x]"));
    assert!(updated.contains("format!(\"{}\", self.x)"));
}

#[test]
fn fast_mode_rewrites_calls_inside_nested_macro_arguments() {
    let source = "struct Point { x: i32 }\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\n#[test] fn check() { let p = Point { x: 3 }; assert_eq!(shift(&p, 1), 4); assert_eq!(Some(shift(&p, 2)), Some(5)); }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let command = || {
        let mut command = Command::cargo_bin("rust-refactor").unwrap();
        command
            .args([
                "to-oop",
                "--file",
                "src/lib.rs",
                "--line",
                "2",
                "--column",
                "4",
                "--fast",
                "--all-features",
                "--format",
                "json",
                "--manifest-path",
            ])
            .arg(dir.path().join("Cargo.toml"));
        command
    };
    let preview = command().arg("--dry-run").output().unwrap();
    let plan: Value = serde_json::from_slice(&preview.stdout).unwrap();
    assert_eq!(preview.status.code(), Some(0), "{plan}");
    assert_eq!(plan["status"], "planned");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );

    let write = command().arg("--write").arg("--check").output().unwrap();
    let result: Value = serde_json::from_slice(&write.stdout).unwrap();
    assert_eq!(write.status.code(), Some(0), "{result}");
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("assert_eq!(p.shift(1), 4)"));
    assert!(updated.contains("assert_eq!(Some(p.shift(2)), Some(5))"));
}

#[test]
fn fast_mode_refuses_function_value_inside_macro() {
    let source = "struct Point { x: i32 }\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\n#[test] fn check() { assert_eq!(Some(shift as fn(&Point, i32) -> i32).is_some(), true); }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "2",
            "--column",
            "4",
            "--fast",
            "--dry-run",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(3), "{result}");
    assert_eq!(result["diagnostics"][0]["code"], "UNHANDLED_REFERENCE");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[test]
fn fast_mode_refuses_ambiguous_function_name() {
    let dir = fixture(&[("lib.rs", "struct Point { x: i32 }\nfn shift(point: &Point) -> i32 { point.x }\nmod other { pub fn shift(x: i32) -> i32 { x } }\n")]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "2",
            "--column",
            "4",
            "--dry-run",
            "--fast",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(3), "{result}");
    assert_eq!(result["diagnostics"][0]["code"], "AMBIGUOUS_FUNCTION");
}

#[test]
fn dry_run_plans_without_changing_source() {
    let source = "struct Point { x: i32 }\n\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\n\nfn run(p: Point) -> i32 { shift(&p, 2) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let (code, output) = run(&dir, "src/lib.rs", 3, "--dry-run");
    assert_eq!(code, 0, "{output}");
    assert_eq!(output["status"], "planned");
    assert_eq!(output["edits"].as_array().unwrap().len(), 3);
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[test]
fn write_updates_cross_file_calls_and_checks() {
    let dir = fixture(&[
        ("lib.rs", "mod user;\nstruct Point { x: i32 }\n\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\n\nfn run() -> i32 { user::run(Point { x: 3 }) }\n"),
        ("user.rs", "use crate::shift;\npub(super) fn run(p: crate::Point) -> i32 { shift(&p, 2) + crate::shift(&p, 1) }\n"),
    ]);
    let (code, output) = run(&dir, "src/lib.rs", 4, "--write");
    assert_eq!(code, 0, "{output}");
    assert_eq!(output["status"], "applied");
    let lib = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    let user = fs::read_to_string(dir.path().join("src/user.rs")).unwrap();
    assert!(lib.contains("fn shift(&self, dx: i32)"));
    assert!(!lib.contains("fn shift(point:"));
    assert!(!user.contains("use crate::shift"));
    assert!(user.contains("p.shift(2)"));
    assert!(user.contains("p.shift(1)"));
}

#[test]
fn write_supports_mutable_and_by_value_receivers() {
    for (receiver, argument, expected, body) in [
        (
            "&mut Point",
            "&mut p",
            "&mut self",
            "point.x += dx; point.x",
        ),
        ("Point", "p", "self", "point.x + dx"),
    ] {
        let source = format!("struct Point {{ x: i32 }}\n\nfn shift(point: {receiver}, dx: i32) -> i32 {{ {body} }}\n\nfn run(mut p: Point) -> i32 {{ shift({argument}, 2) }}\n");
        let dir = fixture(&[("lib.rs", &source)]);
        let (code, output) = run(&dir, "src/lib.rs", 3, "--write");
        assert_eq!(code, 0, "{output}");
        let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
        assert!(updated.contains(&format!("fn shift({expected}, dx: i32)")));
    }
}

#[test]
fn refuses_function_pointer_without_changes() {
    let source = "struct Point { x: i32 }\n\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\n\nfn run() { let f = shift; let _ = f; }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let (code, output) = run(&dir, "src/lib.rs", 3, "--write");
    assert_eq!(code, 3, "{output}");
    assert_eq!(output["status"], "refused");
    assert_eq!(output["diagnostics"][0]["code"], "UNHANDLED_REFERENCE");
    assert_eq!(output["edits"].as_array().unwrap().len(), 0);
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[test]
fn fast_preview_refuses_callback_reference() {
    let source = "struct Point { x: i32 }\n\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\n\nfn run(p: Point) -> i32 { let callback: fn(&Point, i32) -> i32 = shift; callback(&p, 2) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "3",
            "--column",
            "4",
            "--dry-run",
            "--fast",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(3), "{result}");
    assert_eq!(result["diagnostics"][0]["code"], "UNHANDLED_REFERENCE");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[test]
fn refuses_existing_method_without_changes() {
    let source = "struct Point { x: i32 }\nimpl Point { fn shift(&self, dx: i32) -> i32 { self.x + dx } }\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let (code, output) = run(&dir, "src/lib.rs", 3, "--write");
    assert_eq!(code, 3, "{output}");
    assert_eq!(output["diagnostics"][0]["code"], "METHOD_CONFLICT");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[test]
fn verification_failure_restores_original_files() {
    let source = "struct Point { x: i32 }\n\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\n\nfn run(p: Point) -> i32 { shift(&p, 2) }\nfn broken() { no_such_function(); }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let (code, output) = run(&dir, "src/lib.rs", 3, "--write");
    assert_eq!(code, 1, "{output}");
    assert_eq!(output["status"], "error");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[test]
fn moves_documentation_with_method() {
    let source = "struct Point { x: i32 }\n\n/// Adds a delta.\n#[inline]\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\nfn run(p: Point) -> i32 { shift(&p, 2) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let (code, output) = run(&dir, "src/lib.rs", 5, "--write");
    assert_eq!(code, 0, "{output}");
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("/// Adds a delta.\n    #[inline]\n    fn shift(&self"));
}

#[test]
fn refuses_recursive_call_without_changes() {
    let source = "struct Point { x: i32 }\n\nfn shift(point: &Point, dx: i32) -> i32 { if dx == 0 { point.x } else { shift(point, dx - 1) } }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let (code, output) = run(&dir, "src/lib.rs", 3, "--write");
    assert_eq!(code, 3, "{output}");
    assert_eq!(output["diagnostics"][0]["code"], "RECURSIVE_CALL");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[test]
fn refuses_inactive_cfg_call_that_cannot_be_resolved() {
    let source = "struct Point { x: i32 }\n\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\n\n#[cfg(feature = \"extra\")]\nfn dormant(p: Point) -> i32 { shift(&p, 1) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let (code, output) = run(&dir, "src/lib.rs", 3, "--write");
    assert_eq!(code, 3, "{output}");
    assert_eq!(output["diagnostics"][0]["code"], "UNRESOLVED_CANDIDATE");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[test]
fn preserves_unrelated_same_name_function_and_evaluates_receiver_once() {
    let source = "struct Point { x: i32 }\nmod other { pub(super) fn shift(x: i32) -> i32 { x + 10 } }\nfn shift(point: Point, dx: i32) -> i32 { point.x + dx }\nfn make_point() -> Point { Point { x: 1 } }\nfn run() -> i32 { shift(make_point(), 2) + other::shift(3) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let (code, output) = run(&dir, "src/lib.rs", 3, "--write");
    assert_eq!(code, 0, "{output}");
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("(make_point()).shift(2)"));
    assert_eq!(updated.matches("make_point()").count(), 2); // definition and one invocation
    assert!(updated.contains("other::shift(3)"));
}

#[test]
fn permits_same_name_method_call_on_another_type() {
    let source = "struct Point { x: i32 }\nstruct Other;\ntrait Shift { fn shift(&self) -> i32; }\nimpl Shift for Other { fn shift(&self) -> i32 { 1 } }\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\nfn run(p: Point, o: Other) -> i32 { shift(&p, 2) + o.shift() }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let (code, output) = run(&dir, "src/lib.rs", 5, "--write");
    assert_eq!(code, 0, "{output}");
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("p.shift(2) + o.shift()"));
}

#[test]
fn fast_mode_ignores_same_name_methods_on_other_types() {
    let source = "struct Point { x: i32 }\nstruct Other;\nimpl Other { fn shift(&self) -> i32 { 1 } }\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\nfn run(p: Point, o: Other) -> i32 { shift(&p, 2) + o.shift() }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "4",
            "--column",
            "4",
            "--fast",
            "--write",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{result}");
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("p.shift(2) + o.shift()"));
}

#[test]
fn all_features_rewrites_enabled_feature_call() {
    let source = "struct Point { x: i32 }\n\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\n\n#[cfg(feature = \"extra\")]\nfn extra(p: Point) -> i32 { shift(&p, 1) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"oop_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[features]\nextra = []\n",
    ).unwrap();
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "3",
            "--column",
            "4",
            "--write",
            "--all-features",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{result}");
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("p.shift(1)"));
}

#[test]
fn batch_rewrites_two_functions_in_one_file() {
    let source = "struct Point { x: i32 }\n\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\n\nfn scale(point: &Point, factor: i32) -> i32 { point.x * factor }\n\nfn run(p: Point) -> i32 { shift(&p, 2) + scale(&p, 3) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "3",
            "--line",
            "5",
            "--column",
            "4",
            "--write",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{result}");
    assert_eq!(result["status"], "applied");
    assert_eq!(result["targets"].as_array().unwrap().len(), 2);
    assert_eq!(result["edits"].as_array().unwrap().len(), 5);
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert!(updated.contains("fn shift(&self, dx: i32)"));
    assert!(updated.contains("fn scale(&self, factor: i32)"));
    assert!(updated.contains("p.shift(2) + p.scale(3)"));
    assert_eq!(updated.matches("impl Point {").count(), 1);
}

#[test]
fn appends_to_existing_inherent_impl() {
    let source = "struct Point { x: i32 }\n\ntrait Label { fn label(&self) -> i32; }\nimpl Label for Point { fn label(&self) -> i32 { self.x } }\nimpl Point { fn original(&self) -> i32 { self.x } }\n\nfn shift(point: &Point, dx: i32) -> i32 { point.x + dx }\nfn run(p: Point) -> i32 { shift(&p, 2) + p.original() + p.label() }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let (code, result) = run(&dir, "src/lib.rs", 7, "--write");
    assert_eq!(code, 0, "{result}");
    let updated = fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    assert_eq!(updated.matches("impl Point {").count(), 1);
    assert!(updated.contains("fn original(&self)"));
    assert!(updated.contains("fn shift(&self, dx: i32)"));
    assert!(updated.contains("p.shift(2)"));
}

#[test]
fn batch_selects_functions_across_files() {
    let dir = fixture(&[
        ("lib.rs", "mod other;\nstruct Point { x: i32 }\nfn shift(point: &Point) -> i32 { point.x + 1 }\nfn run(p: Point) -> i32 { shift(&p) + other::run() }\n"),
        ("other.rs", "struct Other { n: i32 }\n\nfn bump(value: Other) -> i32 { value.n + 1 }\npub(super) fn run() -> i32 { bump(Other { n: 2 }) }\n"),
    ]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--selection",
            "src/lib.rs:3:4",
            "--selection",
            "src/other.rs:3:4",
            "--write",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{result}");
    assert_eq!(result["targets"].as_array().unwrap().len(), 2);
    assert!(fs::read_to_string(dir.path().join("src/lib.rs"))
        .unwrap()
        .contains("fn shift(&self)"));
    assert!(fs::read_to_string(dir.path().join("src/other.rs"))
        .unwrap()
        .contains("fn bump(self)"));
}

#[test]
fn batch_refusal_keeps_all_files_unchanged() {
    let source = "struct Point { x: i32 }\n\nfn shift(point: &Point) -> i32 { point.x + 1 }\n\nfn scale(point: &Point) -> i32 { point.x * 2 }\n\nfn run(p: Point) -> i32 { let f = scale; shift(&p) + f(&p) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "3",
            "--line",
            "5",
            "--column",
            "4",
            "--write",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(3), "{result}");
    assert_eq!(result["status"], "refused");
    assert!(result["edits"].as_array().unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[test]
fn batch_refuses_overlapping_function_edits() {
    let source = "struct Point { x: i32 }\n\nfn shift(point: &Point) -> i32 { scale(point) }\n\nfn scale(point: &Point) -> i32 { point.x * 2 }\n\nfn run(p: Point) -> i32 { shift(&p) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "3",
            "--line",
            "5",
            "--column",
            "4",
            "--write",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(dir.path().join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(3), "{result}");
    assert_eq!(result["diagnostics"][0]["code"], "CONFLICTING_EDITS");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}

#[cfg(unix)]
#[test]
fn selects_file_through_symlinked_manifest_path() {
    let source = "struct Point { x: i32 }\n\nfn shift(point: &Point) -> i32 { point.x + 1 }\nfn run(p: Point) -> i32 { shift(&p) }\n";
    let dir = fixture(&[("lib.rs", source)]);
    let link_parent = TempDir::new().unwrap();
    let linked_root = link_parent.path().join("linked-repo");
    std::os::unix::fs::symlink(dir.path(), &linked_root).unwrap();
    let output = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "to-oop",
            "--file",
            "src/lib.rs",
            "--line",
            "3",
            "--column",
            "4",
            "--dry-run",
            "--format",
            "json",
            "--manifest-path",
        ])
        .arg(linked_root.join("Cargo.toml"))
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{result}");
    assert_eq!(result["status"], "planned");
    assert_eq!(
        fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        source
    );
}
