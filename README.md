# rust-refactor

This is a refactoring tool, to aid translation of code to idiomatic Rust.
The benefit of using this tool over "raw" LLM is that 

* Synchronized edits will be made across files, informed by static analysis - More precise and avoids LLMs not wanting to make "breaking edits"
* Less token usage because the tool is tailored for the purpose

The CLI has four subcommands: `inline`, `to-oop`, `to-oop-stats`, and
`remove-function`. Run `rust-refactor <command> --help` for all options.


## Command: Inline annotated functions

`inline` finds all free functions annotated with `#[doinline]` or
`#[rust_refactor::inline]` in the Cargo workspace, replaces their supported
call sites with the function body, and removes the definitions. There is no
per-function selection flag: every annotated function is considered in one
run. For a compilable source file, the `#[doinline]` marker can be used with
`#![allow(unused_attributes)]`.

```rust
#![allow(unused_attributes)]

#[doinline]
fn double(x: i32) -> i32 { x * 2 }
```

```sh
./target/debug/rust-refactor inline \
  --manifest-path /path/to/Cargo.toml --dry-run
./target/debug/rust-refactor inline \
  --manifest-path /path/to/Cargo.toml --write --check
```

`--dry-run` prints the planned edits without writing. `--write` applies them;
add `--check` to run `cargo fmt` and `cargo check`, or `--test` to also run
`cargo test`. Verification failure restores the original files unless
`--keep-broken` is set. `--all-features` and `--target` configure verification.
The inliner supports a limited subset of free functions and calls. It reports
unsupported annotated functions and references as errors; review the dry-run
diagnostics before writing.


## Command: Making C-oriented code more object oriented

Its `to-oop` command moves a free function whose first parameter is
`T`, `&T`, or `&mut T` into an inherent `impl T`, changes the parameter to
`self`, `&self`, or `&mut self`, and updates call sites. It either
applies the complete plan or refuses it with a diagnostic. Normal-mode
`--write` formats touched files and runs `cargo check`, restoring those files
if verification fails. Fast-mode `--write` runs `cargo check` with `--check`.
Public functions are moved too; this changes their free-function API.

Build from this repository with `cargo build --locked`. Use `--format json`
when an LLM calls the tool: stdout then contains `status`, `targets`, `edits`,
and `diagnostics`. Exit code 0 means the plan or write succeeded, 3 means the
refactor was safely refused, and 1 means an operational or verification error.
`--dry-run` validates the planned edits and leaves the workspace untouched.
For very large workspaces, `--fast` plans from parsed source without loading
rust-analyzer. It requires the selected free-function name to be unique in the
workspace. The preview applies edits in memory and parses touched files.
`--write --fast` backs up and formats only touched files, then parses them;
add `--check` to run `cargo check`. Failed writes restore those files.
Both modes refuse a selected function used as a callback or other function
value, since that reference cannot become a method call automatically.
Fast mode also scans macro arguments for direct calls, such as
`assert_eq!(shift(&p, 2), 5)`, and rewrites them to method calls. A function
value inside a macro is refused.

## Command: Finding functions that are suitable as methods

Use `to-oop-stats` below to obtain a current `FILE:LINE:COLUMN` selection.
For one function:

```sh
./target/debug/rust-refactor to-oop \
  --manifest-path /path/to/Cargo.toml \
  --selection src/lib.rs:42:8 --dry-run --format json
./target/debug/rust-refactor to-oop \
  --manifest-path /path/to/Cargo.toml \
  --selection src/lib.rs:42:8 --write --format json
```

The tool has no coordination with concurrent editors. Keep other writers away
from files in the plan until the write completes. It currently requires the
struct and selected function in the same module. Optional receivers such as
`Option<&T>` need a separate source refactor to handle the `None` branch.

## Command: OOP-ifying multiple functions

One invocation can refactor several independent functions. For functions in
one file with names starting at the same column, repeat `--line`:

```sh
./target/debug/rust-refactor to-oop --manifest-path /path/to/Cargo.toml \
  --file src/lib.rs --line 12 --line 30 --column 4 --dry-run --format json
```

For different files or columns, repeat `--selection FILE:LINE:COLUMN`. A batch
uses one workspace analysis and one combined validation. If any selection
fails, edits overlap, or verification fails, the whole batch is refused or
rolled back. Functions in one batch currently need distinct names. When
selected functions call one another, convert the called functions first;
then refresh line numbers and select their callers.

Methods for one struct share a single inherent `impl` block. If one already
exists in the same module, the tool appends methods to it. Simple borrowed
receiver expressions at call sites become direct method calls, such as
`shift(&p, 2)` becoming `p.shift(2)`; these formatting steps are automatic.

## Find receiver groups

`to-oop-stats` scans the Cargo workspace without loading rust-analyzer and
groups free functions by the struct named in their first parameter. Groups
are sorted by function count. The text table and JSON include each function's
`FILE:LINE:COLUMN` selection; JSON also reports whether the struct is local
and whether the receiver is direct or optional. `selectable_count` counts
direct receivers with a struct in the same module; each candidate can still
be refused during planning.

```sh
./target/debug/rust-refactor to-oop-stats \
  --manifest-path /path/to/Cargo.toml --format json
./target/debug/rust-refactor to-oop-stats \
  --manifest-path /path/to/Cargo.toml --struct Widget
```

To select every syntactically direct free function for one struct in a single
batch, use `to-oop --struct Widget --dry-run --format json`. Optional receivers
are listed in stats but excluded from this selection. An unsupported function
or conflicting edit refuses the whole batch. For a smaller batch, use the
reported `selection` values with repeated `--selection` arguments. The tool
does not generate forwarding wrappers.

A large translation case is documented in
[the imod case study](docs/IMOD_CASE_STUDY.md).

## Command: Remove a function and its standalone calls

`remove-function` deletes a selected free function, direct calls used as
statements, and simple imports of that function. It is useful for no-op
translation artifacts. Select the function name by its
current one-based location:

```sh
./target/debug/rust-refactor remove-function \
  --manifest-path /path/to/Cargo.toml \
  --file src/lib.rs --line 42 --column 8 --dry-run --format json
```

Review the JSON edits, then use `--write` in place of `--dry-run`. A write
formats edited files and runs `cargo check`; failed verification restores
their original contents. The command refuses calls whose result is used,
function pointers/callbacks, and references it cannot remove. It also refuses
names shared by multiple workspace functions. These limits prevent a partial
deletion; the JSON `diagnostics` field explains a refusal.
