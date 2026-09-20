# Rust Refactor Tool Plan

## Goal

Build a Rust refactoring CLI in Rust, initially focused on an annotation-driven
function inliner:

```rust
#[rust_refactor::inline]
fn helper(x: i32) -> i32 {
    x + 1
}
```

The tool should find annotated functions, inline their call sites across files,
delete the original function when all call sites are handled, format the result,
and verify the project still compiles.

This project should not use ast-grep-style pattern matching as a core feature.
The primary foundation should be rust-analyzer's semantic model.

## Design Principles

- Use rust-analyzer for semantic analysis: symbol resolution, references,
  module/crate structure, and source mapping.
- Keep refactor planning separate from file mutation.
- Represent every rewrite as explicit text edits.
- Apply edits transactionally and verify after application.
- Start with a deliberately narrow, safe subset of Rust.
- Prefer refusing an unsafe inline over producing a suspicious rewrite.

## Architecture

```text
rust_refactor
  cli
    parse commands and flags

  project
    discover Cargo workspace
    load rust-analyzer database
    map files, crates, modules, and cfg state

  analysis
    find annotated items
    resolve symbols
    find references and call sites
    classify whether each call site is supported

  refactors
    inline_function
      discover targets
      validate target function
      plan call-site edits
      plan definition deletion

  edits
    TextEdit
    RefactorPlan
    edit conflict detection
    preview/diff rendering
    transactional apply

  verify
    rustfmt
    cargo check
    optional cargo test
```

## Core Data Model

```rust
struct RefactorPlan {
    edits: Vec<TextEdit>,
    diagnostics: Vec<Diagnostic>,
}

struct TextEdit {
    file: camino::Utf8PathBuf,
    range: text_size::TextRange,
    replacement: String,
}

enum DiagnosticLevel {
    Error,
    Warning,
    Note,
}

struct Diagnostic {
    level: DiagnosticLevel,
    message: String,
    file: Option<camino::Utf8PathBuf>,
    range: Option<text_size::TextRange>,
}
```

Each refactor should produce a `RefactorPlan`. Applying that plan should be a
separate step.

## CLI Shape

Initial commands:

```text
rust-refactor inline --dry-run
rust-refactor inline --write
rust-refactor inline --write --check
```

Later commands can reuse the same planning/apply/verify pipeline:

```text
rust-refactor plan
rust-refactor apply
rust-refactor verify
```

## Annotation Format

Preferred attribute:

```rust
#[rust_refactor::inline]
fn helper(...) -> ... {
    ...
}
```

Fallback attribute for early prototypes:

```rust
#[allow(unused_attributes)]
#[doinline]
fn helper(...) -> ... {
    ...
}
```

Use a namespaced attribute long term. It is easier to identify, less likely to
collide with unrelated attributes, and leaves room for options:

```rust
#[rust_refactor::inline(delete = true)]
```

## First Milestone

Support only free functions with simple expression bodies.

Supported:

- non-async free functions
- non-const functions
- functions with a normal block body whose final expression is the return value
- direct calls such as `helper(a)` and qualified calls such as `module::helper(a)`
- call sites in files loaded by the active Cargo workspace
- deletion of the original function only when every resolved call site is
  successfully rewritten

Rejected:

- methods
- trait functions
- generic functions
- async functions
- const functions
- unsafe functions
- functions with explicit `return`
- functions using `?`
- functions containing macros
- recursive functions
- call sites inside macro expansions
- call sites where the callee cannot be resolved with confidence
- call sites requiring import rewriting

## Inlining Rules

The inliner should preserve argument evaluation order and avoid duplicated
evaluation.

Example:

```rust
#[rust_refactor::inline]
fn square(x: i32) -> i32 {
    x * x
}

let y = square(a + b);
```

Because `x` is used twice, the generated replacement should introduce a fresh
temporary:

```rust
let __rust_refactor_x = a + b;
let y = __rust_refactor_x * __rust_refactor_x;
```

Direct substitution is only safe when an argument is cheap and side-effect-free
or the parameter is used once. Early versions can conservatively introduce
temporaries whenever there is doubt.

Fresh temporary names must avoid collisions with names visible at the call site.

## Edit Application

The edit layer should:

- group edits by file
- sort edits in descending byte-offset order before applying
- reject overlapping edits unless a refactor explicitly knows how to merge them
- write changed files only after the full plan is valid
- keep original file contents available for rollback if verification fails

## Verification

For `--write --check`, run:

```text
cargo fmt
cargo check
```

Later options:

```text
rust-refactor inline --write --test
rust-refactor inline --write --check --all-features
rust-refactor inline --write --check --target <triple>
```

If verification fails, either:

- restore original file contents automatically, or
- leave files changed and emit the failing command/output when explicitly
  requested with a flag such as `--keep-broken`.

Default behavior should favor rollback.

## rust-analyzer Integration

Use rust-analyzer crates for:

- Cargo workspace loading
- parsing source files
- locating attributes and function definitions
- resolving a call expression to the target function
- finding references to the target definition
- mapping source ranges back to files

Pin rust-analyzer crate versions. Treat rust-analyzer integration as an internal
adapter layer so future API churn is isolated to `project` and `analysis`.

## Not In Scope Initially

- ast-grep-style pattern matching
- general lint framework
- arbitrary code generation rules
- IDE integration
- partial automatic inlining when some call sites fail
- rewriting calls inside macros
- preserving every original formatting choice before `rustfmt`
- changing public API exports or downstream crates

## Decisions

- The tool does not require a clean git worktree before `--write`; rollback is
  handled from an in-memory snapshot of workspace Rust files when verification
  fails.
- Deletion is implied by the inline annotation once every resolved reference is
  handled by a supported rewrite or import cleanup.
- Call sites in tests/examples/benches are included by default because Rust
  source discovery scans workspace package roots, and rust-analyzer loading uses
  all targets.
- Verification defaults to manifest-scoped `cargo check` when `--check` is
  requested. `--test`, `--all-features`, and `--target <triple>` are explicit
  opt-ins.
- No annotation crate is required yet. The tool recognizes
  `#[rust_refactor::inline]` and the inert fallback `#[doinline]`.

## Recommended Next Step

Create the CLI skeleton and implement a dry-run path:

1. Load the Cargo workspace.
2. Build the rust-analyzer analysis database.
3. Find `#[rust_refactor::inline]` function definitions.
4. Print each target and the resolved call-site count.

Do not mutate files until symbol resolution and call-site discovery are working
reliably.

## Implementation Status

Implemented:

- CLI crate skeleton with `rust-refactor inline`.
- `--dry-run`, `--write`, `--check`, and `--manifest-path` flags.
- Cargo workspace discovery through `cargo_metadata`.
- Rust source discovery for workspace packages.
- rust-analyzer syntax parsing through `ra_ap_syntax`.
- rust-analyzer semantic workspace loading through `ra_ap_load-cargo` and
  `ra_ap_ide`.
- semantic reference filtering for inline call sites.
- discovery of `#[rust_refactor::inline]` and `#[doinline]` free functions.
- reporting of resolved direct/qualified call candidates.
- conservative planning for simple expression-body and statement-plus-tail
  function inlining.
- text-edit conflict detection and application.
- dry-run edit preview output.
- deletion of supported annotated function definitions after call edits are
  planned.
- guarded `--write` support.
- manifest-scoped `cargo fmt` and `cargo check` verification.
- manifest-scoped `cargo test` verification through `--test`.
- `--all-features` verification option for `cargo check`/`cargo test`.
- `--target <triple>` verification option for `cargo check`/`cargo test`.
- rollback of workspace Rust files when `--write --check` verification fails.
- rollback of workspace Rust files when `--write --test` verification fails.
- `--keep-broken` mode for inspecting failed verification output without
  automatic rollback.
- temporary binding generation for duplicated complex arguments.
- temporary binding names avoid identifiers already present in the call-site
  file.
- `RefactorPlan`, `TextEdit`, and `Diagnostic` data structures.
- focused unit tests for annotation discovery, call candidate counting, edit
  application, substitution, and unsafe duplicated evaluation rejection.
- integration tests for `--write`, `--write --check`, and rollback on failed
  verification.
- integration coverage proving same-terminal-name calls that resolve to another
  function are not rewritten.
- write plans fail when rust-analyzer finds a resolved reference that is not a
  supported call site, preventing unsafe deletion.
- import references are included in reference search.
- simple private imports such as `use crate::helper;` are deleted when they only
  import the inlined function.
- grouped private imports such as `use crate::{helper, other};` are partially
  rewritten when the inlined function is one member of the group.
- renamed, glob, public, or attributed imports still block writes.
- recursive annotated functions are rejected.
- annotated associated functions are rejected.
- function bodies that shadow parameter names are rejected to avoid incorrect
  substitution.
- substitution is limited to path-expression references so field names and local
  bindings are not rewritten as parameters.
- nightly toolchain pin required by the current rust-analyzer semantic crates.

Current limitations:

- the inliner only supports simple identifier parameters.
- renamed/glob/public/attributed import rewriting, methods, generics, macros,
  async/const/unsafe functions, and non-local control flow remain unsupported.

Possible Future Work:

- Support renamed/glob/public/attributed import rewriting where safe.
- Support selected generic free functions.
- Support methods and associated functions.
- Add IDE integration.
- Add richer diff output.
