use assert_cmd::Command;
use std::fs;
use tempfile::TempDir;

fn create_fixture(source: &str) -> (TempDir, std::path::PathBuf) {
    let project = TempDir::new().unwrap();
    let src = project.path().join("src");
    fs::create_dir(&src).unwrap();

    fs::write(
        project.path().join("Cargo.toml"),
        r#"
[package]
name = "fixture"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    let lib_rs = src.join("lib.rs");
    fs::write(&lib_rs, source).unwrap();

    (project, lib_rs)
}

fn create_fixture_with_files(files: &[(&str, &str)]) -> TempDir {
    let project = TempDir::new().unwrap();
    let src = project.path().join("src");
    fs::create_dir(&src).unwrap();

    fs::write(
        project.path().join("Cargo.toml"),
        r#"
[package]
name = "fixture"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();

    for (path, source) in files {
        let full_path = src.join(path);
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(full_path, source).unwrap();
    }

    project
}

fn create_fixture_with_manifest(source: &str, manifest: &str) -> (TempDir, std::path::PathBuf) {
    let project = TempDir::new().unwrap();
    let src = project.path().join("src");
    fs::create_dir(&src).unwrap();

    fs::write(project.path().join("Cargo.toml"), manifest).unwrap();

    let lib_rs = src.join("lib.rs");
    fs::write(&lib_rs, source).unwrap();

    (project, lib_rs)
}

#[test]
fn write_inlines_simple_annotated_function() {
    let (project, lib_rs) = create_fixture(
        r#"
#![allow(unused_attributes)]

#[doinline]
fn helper(x: i32) -> i32 {
    x + 1
}

pub fn run(v: i32) -> i32 {
    helper(v)
}
"#,
    );

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .success();

    let updated = fs::read_to_string(&lib_rs).unwrap();

    assert!(!updated.contains("fn helper"));
    assert!(updated.contains("v + 1"));
}

#[test]
fn dry_run_previews_edits_without_writing() {
    let original = r#"
#![allow(unused_attributes)]

#[doinline]
fn helper(x: i32) -> i32 {
    x + 1
}

pub fn run(v: i32) -> i32 {
    helper(v)
}
"#;
    let (project, lib_rs) = create_fixture(original);

    let assert = Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--dry-run",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(stdout.contains("planned edits: 2"));
    assert!(stdout.contains("edit "));
    assert_eq!(fs::read_to_string(lib_rs).unwrap(), original);
}

#[test]
fn write_inlines_simple_annotated_function_across_files() {
    let lib = r#"
#![allow(unused_attributes)]

mod user;

#[doinline]
fn helper(x: i32) -> i32 {
    x + 1
}

pub use user::run;
"#;
    let user = r#"
pub fn run(v: i32) -> i32 {
    crate::helper(v)
}
"#;
    let project = create_fixture_with_files(&[("lib.rs", lib), ("user.rs", user)]);

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--check",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .success();

    let lib_rs = fs::read_to_string(project.path().join("src/lib.rs")).unwrap();
    let user_rs = fs::read_to_string(project.path().join("src/user.rs")).unwrap();

    assert!(!lib_rs.contains("fn helper"));
    assert!(user_rs.contains("v + 1"));
}

#[test]
fn write_check_verifies_selected_manifest() {
    let (project, lib_rs) = create_fixture(
        r#"
#![allow(unused_attributes)]

#[doinline]
fn helper(x: i32) -> i32 {
    x + 1
}

pub fn run(v: i32) -> i32 {
    helper(v)
}
"#,
    );

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--check",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .success();

    let updated = fs::read_to_string(&lib_rs).unwrap();

    assert!(!updated.contains("fn helper"));
    assert!(updated.contains("v + 1"));
}

#[test]
fn write_test_runs_cargo_test() {
    let (project, lib_rs) = create_fixture(
        r#"
#![allow(unused_attributes)]

#[doinline]
fn helper(x: i32) -> i32 {
    x + 1
}

pub fn run(v: i32) -> i32 {
    helper(v)
}

#[test]
fn run_works() {
    assert_eq!(run(1), 2);
}
"#,
    );

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--test",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .success();

    let updated = fs::read_to_string(&lib_rs).unwrap();

    assert!(!updated.contains("fn helper"));
    assert!(updated.contains("v + 1"));
}

#[test]
fn write_check_accepts_all_features() {
    let (project, lib_rs) = create_fixture_with_manifest(
        r#"
#![allow(unused_attributes)]

#[doinline]
fn helper(x: i32) -> i32 {
    x + 1
}

pub fn run(v: i32) -> i32 {
    helper(v)
}

#[cfg(feature = "extra")]
pub fn extra() -> i32 {
    run(1)
}
"#,
        r#"
[package]
name = "fixture"
version = "0.1.0"
edition = "2021"

[features]
extra = []
"#,
    );

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--check",
            "--all-features",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .success();

    let updated = fs::read_to_string(&lib_rs).unwrap();

    assert!(!updated.contains("fn helper"));
}

#[test]
fn write_rejects_verification_options_without_verification_mode() {
    let original = r#"
#![allow(unused_attributes)]

#[doinline]
fn helper(x: i32) -> i32 {
    x + 1
}

pub fn run(v: i32) -> i32 {
    helper(v)
}
"#;
    let (project, lib_rs) = create_fixture(original);

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--all-features",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .failure();

    assert_eq!(fs::read_to_string(lib_rs).unwrap(), original);

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--target",
            "x86_64-unknown-linux-gnu",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .failure();

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--keep-broken",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .failure();
}

#[test]
fn write_test_restores_files_when_tests_fail() {
    let original = r#"
#![allow(unused_attributes)]

#[doinline]
fn helper(x: i32) -> i32 {
    x + 1
}

pub fn run(v: i32) -> i32 {
    helper(v)
}

#[test]
fn failing_test() {
    assert_eq!(run(1), 10);
}
"#;
    let (project, lib_rs) = create_fixture(original);

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--test",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .failure();

    assert_eq!(fs::read_to_string(lib_rs).unwrap(), original);
}

#[test]
fn write_check_restores_files_when_verification_fails() {
    let original = r#"
#![allow(unused_attributes)]

#[doinline]
fn helper(x: i32) -> i32 {
    x + 1
}

pub fn run(v: i32) -> i32 {
    helper(v)
}

pub fn broken() {
    does_not_exist();
}
"#;
    let (project, lib_rs) = create_fixture(original);

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--check",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .failure();

    let updated = fs::read_to_string(&lib_rs).unwrap();

    assert_eq!(updated, original);
}

#[test]
fn write_check_keep_broken_keeps_files_when_verification_fails() {
    let original = r#"
#![allow(unused_attributes)]

#[doinline]
fn helper(x: i32) -> i32 {
    x + 1
}

pub fn run(v: i32) -> i32 {
    helper(v)
}

pub fn broken() {
    does_not_exist();
}
"#;
    let (project, lib_rs) = create_fixture(original);

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--check",
            "--keep-broken",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .failure();

    let updated = fs::read_to_string(lib_rs).unwrap();

    assert_ne!(updated, original);
    assert!(!updated.contains("fn helper"));
    assert!(updated.contains("v + 1"));
}

#[test]
fn write_check_introduces_temp_for_duplicated_complex_argument() {
    let (project, lib_rs) = create_fixture(
        r#"
#![allow(unused_attributes)]

#[doinline]
fn square(x: i32) -> i32 {
    x * x
}

fn next(v: i32) -> i32 {
    v + 1
}

pub fn run(v: i32) -> i32 {
    square(next(v))
}
"#,
    );

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--check",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .success();

    let updated = fs::read_to_string(&lib_rs).unwrap();

    assert!(!updated.contains("fn square"));
    assert!(updated.contains("let __rust_refactor_x = next(v);"));
    assert!(updated.contains("__rust_refactor_x * __rust_refactor_x"));
}

#[test]
fn write_check_inlines_statement_body() {
    let (project, lib_rs) = create_fixture(
        r#"
#![allow(unused_attributes)]

#[doinline]
fn helper(x: i32) -> i32 {
    let y = x + 1;
    y * 2
}

pub fn run(v: i32) -> i32 {
    helper(v)
}
"#,
    );

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--check",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .success();

    let updated = fs::read_to_string(&lib_rs).unwrap();

    assert!(!updated.contains("fn helper"));
    assert!(updated.contains("let y = v + 1;"));
    assert!(updated.contains("y * 2"));
}

#[test]
fn write_avoids_existing_temp_name() {
    let (project, lib_rs) = create_fixture(
        r#"
#![allow(unused_attributes)]

#[doinline]
fn square(x: i32) -> i32 {
    x * x
}

fn next(v: i32) -> i32 {
    v + 1
}

pub fn run(v: i32) -> i32 {
    let __rust_refactor_x = 10;
    square(next(v)) + __rust_refactor_x
}
"#,
    );

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--check",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .success();

    let updated = fs::read_to_string(&lib_rs).unwrap();

    assert!(updated.contains("let __rust_refactor_x_1 = next(v);"));
}

#[test]
fn write_refuses_explicit_return_body() {
    let original = r#"
#![allow(unused_attributes)]

#[doinline]
fn helper(x: i32) -> i32 {
    return x + 1;
}

pub fn run(v: i32) -> i32 {
    helper(v)
}
"#;
    let (project, lib_rs) = create_fixture(original);

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .failure();

    assert_eq!(fs::read_to_string(lib_rs).unwrap(), original);
}

#[test]
fn write_refuses_unhandled_non_call_reference() {
    let original = r#"
#![allow(unused_attributes)]

#[doinline]
fn helper(x: i32) -> i32 {
    x + 1
}

pub fn run(v: i32) -> i32 {
    let f = helper;
    f(v)
}
"#;
    let (project, lib_rs) = create_fixture(original);

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .failure();

    assert_eq!(fs::read_to_string(lib_rs).unwrap(), original);
}

#[test]
fn write_cleans_simple_import_reference() {
    let lib = r#"
#![allow(unused_attributes)]

mod user;

#[doinline]
pub fn helper(x: i32) -> i32 {
    x + 1
}
"#;
    let user = r#"
use crate::helper;

pub fn run(v: i32) -> i32 {
    helper(v)
}
"#;
    let project = create_fixture_with_files(&[("lib.rs", lib), ("user.rs", user)]);
    let user_rs = project.path().join("src/user.rs");

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--check",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .success();

    let updated = fs::read_to_string(user_rs).unwrap();

    assert!(!updated.contains("use crate::helper;"));
    assert!(updated.contains("v + 1"));
}

#[test]
fn write_rewrites_grouped_import_reference() {
    let lib = r#"
#![allow(unused_attributes)]

mod user;

#[doinline]
pub fn helper(x: i32) -> i32 {
    x + 1
}

pub fn other(x: i32) -> i32 {
    x - 1
}
"#;
    let user = r#"
use crate::{helper, other};

pub fn run(v: i32) -> i32 {
    helper(v) + other(v)
}
"#;
    let project = create_fixture_with_files(&[("lib.rs", lib), ("user.rs", user)]);
    let lib_rs = project.path().join("src/lib.rs");
    let user_rs = project.path().join("src/user.rs");

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--check",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .success();

    let updated_lib = fs::read_to_string(lib_rs).unwrap();
    let updated_user = fs::read_to_string(user_rs).unwrap();

    assert!(!updated_lib.contains("fn helper"));
    assert!(updated_user.contains("use crate::other;"));
    assert!(updated_user.contains("v + 1"));
    assert!(updated_user.contains("other(v)"));
}

#[test]
fn write_refuses_renamed_import_reference_that_would_need_rewriting() {
    let lib = r#"
#![allow(unused_attributes)]

mod user;

#[doinline]
pub fn helper(x: i32) -> i32 {
    x + 1
}
"#;
    let user = r#"
use crate::helper as renamed;

pub fn run(v: i32) -> i32 {
    renamed(v)
}
"#;
    let project = create_fixture_with_files(&[("lib.rs", lib), ("user.rs", user)]);
    let lib_rs = project.path().join("src/lib.rs");
    let user_rs = project.path().join("src/user.rs");

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .failure();

    assert_eq!(fs::read_to_string(lib_rs).unwrap(), lib);
    assert_eq!(fs::read_to_string(user_rs).unwrap(), user);
}

#[test]
fn write_refuses_annotated_associated_function() {
    let original = r#"
#![allow(unused_attributes)]

struct Value;

impl Value {
    #[doinline]
    fn helper(x: i32) -> i32 {
        x + 1
    }
}

pub fn run(v: i32) -> i32 {
    Value::helper(v)
}
"#;
    let (project, lib_rs) = create_fixture(original);

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .failure();

    assert_eq!(fs::read_to_string(lib_rs).unwrap(), original);
}

#[test]
fn write_ignores_same_name_function_that_is_not_the_resolved_target() {
    let lib = r#"
#![allow(unused_attributes)]

mod other;

#[doinline]
fn helper(x: i32) -> i32 {
    x + 1
}

pub fn run(v: i32) -> i32 {
    helper(v)
}

pub fn run_other(v: i32) -> i32 {
    other::helper(v)
}
"#;
    let other = r#"
pub fn helper(x: i32) -> i32 {
    x - 1
}
"#;
    let project = create_fixture_with_files(&[("lib.rs", lib), ("other.rs", other)]);
    let lib_rs = project.path().join("src/lib.rs");

    Command::cargo_bin("rust-refactor")
        .unwrap()
        .args([
            "inline",
            "--write",
            "--manifest-path",
            project.path().join("Cargo.toml").to_str().unwrap(),
        ])
        .assert()
        .success();

    let updated = fs::read_to_string(lib_rs).unwrap();

    assert!(!updated.contains("fn helper"));
    assert!(updated.contains("v + 1"));
    assert!(updated.contains("other::helper(v)"));
    assert_eq!(
        fs::read_to_string(project.path().join("src/other.rs")).unwrap(),
        other
    );
}
