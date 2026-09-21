# rust-refactor

This is a refactoring tool, to aid translation of code to idiomatic Rust.
The benefit of using this tool over "raw" LLM is that 

* Synchronized edits will be made across files, informed by static analysis - More precise and avoids LLMs not wanting to make "breaking edits"
* Less token usage because the tool is tailored for the purpose

The CLI has ten subcommands: `constants-to-enum`, `constants-to-enum-csv`,
`constants-to-enum-stats`, `enum-hoist`, `enum-hoist-stats`, `inline`, `to-oop`,
`to-oop-stats`, `remove-function`, and `simplify-wrapper`. Run
`rust-refactor <command> --help` for all options.


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

## Command: Introduce an enum for integer constants

Start with `constants-to-enum-stats`. It builds sets of constants from usage:
patterns of one match, comparisons with one subject, assignments to one
destination, returns from one function, and a fixed parameter of one uniquely
resolved workspace function. Generic constructors and wrappers such as
`Some`, `Ok`, `Box::new`, and `Vec::from` do not contribute call evidence.
Sets that overlap are merged transitively. This recovers default values,
result codes, and constants used as outputs as well as input discriminants.
Locals and parameters are keyed by their declarations, and fields include the
receiver identity. Dispatcher observations containing several complete name
prefix families are split before merging.

```sh
./target/debug/rust-refactor constants-to-enum-stats \
  --manifest-path /path/to/Cargo.toml --min-constants 3 --format json
```

Candidates are sorted by number of distinct constants. JSON reports the
subject expression and containing function, each constant's definition and
raw value, plus evidence locations. A match evidence `selection` can be passed
to `constants-to-enum --match` after checking that its constants are the exact
family being converted. Name resolution in this fast discovery pass prefers a
same-file definition and otherwise accepts a name with one definition in the
workspace. The conversion command performs the stricter semantic checks.

For an editable workspace inventory, request CSV and redirect stdout:

```sh
./target/debug/rust-refactor constants-to-enum-stats \
  --manifest-path /path/to/Cargo.toml --format csv > constants.csv
```

The CSV has one row for every module, associated, trait, and local constant.
`proposed_enum_group` is empty when usage did not assign the constant to a
numeric enum candidate. Assigned rows also contain proposed enum and variant
names, subjects, match selections, and all evidence selections. Constants with
shift or bitwise initializers, constants used in bitwise expressions, and
constants used as ordering bounds with `<`, `>`, `<=`, or `>=` are removed
before candidate sets are merged. Their `unassigned_reason` is `bitwise_use` or
`ordering_comparison`. Thus one numeric bound cannot pull an otherwise valid
enum family into a limit group. Groups dominated by size, count, limit, or mask
names are also omitted. Unassigned numeric constants can have weak suggestions in
`possible_enum_groups`, based on membership in a detected name prefix; these
suggestions never merge sets automatically. `unassigned_reason` distinguishes
unsupported types, non-module constants, and constants with no strong set.
For equal raw values within one lexical name family, one row is canonical and
later rows name it in `alias_of`; those rows intentionally share the canonical
variant. An unassigned sibling is merged automatically when it is the only
equal-value candidate for an enum group of at least five constants and is
declared within five lines of the canonical constant. Other equal-value rows
name the candidate in `possible_alias_of`; an LLM can accept one by copying the
group and variant and moving the suggestion to `alias_of`. This keeps parallel
numeric domains that reuse 0, 1, 2 from being merged as aliases.

Repeated declarations are reported separately from value aliases. Rows with
the same constant name, type, and initializer receive `duplicate_kind=exact`;
integer literal spellings that normalize to the same value, such as `2`,
`0x2`, and `2 as i32`, receive `duplicate_kind=normalized`. Compatible rows
share a `duplicate_group` and a `canonical_candidate` source location. The
candidate favors public module constants, files containing more declarations
from the same name family, and declarations with stronger usage evidence. It
is a review hint rather than an instruction to delete the other declarations.
`duplicate_name_conflict=true` warns that the same name also has another type
or normalized value; an incompatible singleton is marked `conflict`. Constants
with different names are never classified as duplicate declarations merely
because their values are equal. Review duplicate families before enum groups,
because replacing copied declarations with one shared definition can turn
several file-local enum candidates into one workspace-wide family.

An LLM can edit the group, kind, enum, variant, and selection cells, then apply
one reviewed group directly:

```sh
./target/debug/rust-refactor constants-to-enum-csv \
  --manifest-path /path/to/Cargo.toml \
  --table constants.csv --group group_001 --dry-run --format json
```

Review the plan, then replace `--dry-run` with `--write`. The importer requires
one definition file, one enum name, nonempty variants, `proposed_group_kind`
set to `enum`, and at least one match or equality-comparison selection. It
validates that every `alias_of` row points to a selected canonical row with
the same raw type, value, and variant. It
refuses discovery-only groups supported solely by assignment, return, or
argument evidence because the current transform cannot yet introduce enum
types at those boundaries.
The importer also refuses plausible unselected siblings from
`possible_enum_groups`. Add such a row to the reviewed group or clear that
cell to record an explicit decision that it does not belong.
Use `--enum-path` when selected matches are outside the definition module.
Empty group cells are intentional and remain available for classification.

`constants-to-enum` turns an explicitly selected family of integer constants
into a numeric enum and converts selected `match` expressions to use it. Raw
function parameters and struct fields remain integers in this first stage.
The original constants become aliases such as
`MODE_BYTE: i32 = Mode::Byte.to_raw()`, so existing raw boundaries continue to
compile while selected matches use `Mode::from_raw(value)`.

```sh
./target/debug/rust-refactor constants-to-enum \
  --manifest-path /path/to/Cargo.toml \
  --file src/lib.rs --enum-name Mode \
  --constant MODE_BYTE=Byte \
  --constant MODE_SHORT=Short \
  --match src/lib.rs:40:5 \
  --dry-run --format json
```

Repeat `--constant CONSTANT=Variant` to define exact membership and spelling.
Repeat `--match FILE:LINE:COLUMN` to convert
several matches atomically. Use `--enum-path crate::module::Mode` when a match
is in another module, and `--visibility private|pub-crate|pub` when the selected
constants do not share a usable visibility. Selected constants need one
primitive integer type and literal values. Equal values are supported when
their `--constant` specifications use the same variant, preserving both
constant names as aliases of one enum variant. A selected match must use
only those constants plus a `_` arm. Bit masks are outside this command's
scope. `--write` formats touched files, runs `cargo check`, and restores them
on failure. See [the design and staged migration plan](CONSTANTS_TO_ENUM.md).
The follow-on parameter and struct-field propagation design is documented in
[ENUM_HOISTING.md](ENUM_HOISTING.md).

## Command: Hoist an enum through typed value flow

After introducing an enum, find places that repeatedly convert the same raw
value:

```sh
./target/debug/rust-refactor enum-hoist-stats \
  --manifest-path /path/to/Cargo.toml \
  --enum-file src/modes.rs --enum-name Mode --format json
```

The report identifies parameters, named fields, locals, and direct function
returns used as `from_raw` subjects, counts their conversions and references,
and emits source selections. Each reported kind can seed the transform.

Select one or more value flows and preview the complete propagation plan:

```sh
./target/debug/rust-refactor enum-hoist \
  --manifest-path /path/to/Cargo.toml \
  --enum-file src/modes.rs --enum-name Mode \
  --enum-path crate::modes::Mode \
  --parameter src/worker.rs:40:18 \
  --field src/window.rs:12:9 \
  --local src/worker.rs:55:9 \
  --return src/source.rs:20:8 \
  --dry-run --format json
```

The command changes connected raw parameters, named struct fields, function
returns, and simple local bindings to the enum. It updates direct free-function
and method calls, rewrites ordinary struct initializers and assignments,
removes redundant `from_raw`/`to_raw` pairs, and places `to_raw` only at
remaining raw consumers. Compatibility constants and unique matching integer
literals become enum variants. An existing
`let Some(value) = Mode::from_raw(raw) else { ... };` can remain as the single
raw entry boundary while downstream calls become typed.

Propagation uses a work queue to a fixpoint: each selected or discovered
parameter, field, local, or return value adds its producer places and consumer
places until no new typed place is found. Cycles and multi-function chains are
planned as one closure. Locals are discovered from simple identifier bindings;
function returns support direct tail expressions and explicit `return`
expressions. Unsupported destructuring, branch-valued expressions, and async
returns refuse the atomic plan.

The plan is atomic. It refuses function-value references, unknown argument or
field producers, compound field writes, macro references, explicit observation
of `None` in a converted match, conflicting edits, and plans that do not reduce
the number of conversions. `--write` snapshots touched files, runs rustfmt and
`cargo check`, and restores every file when verification fails. No forwarding
wrappers are generated.

Use repeated `--comparison FILE:LINE:COLUMN` selections for `==` and `!=`
expressions. A selected `raw == MODE_BYTE` becomes
`Mode::from_raw(raw) == Some(Mode::Byte)`. Match and comparison selections can
be combined in one transaction.

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

## Command: Remove a transparent drop wrapper

`simplify-wrapper` recognizes a selected free function whose only operation
is `drop` on its single owned parameter. It replaces resolved calls with
`::core::mem::drop(argument)`, removes simple imports, and deletes the wrapper.
Selection is by source position; the function's name does not determine whether
it matches. Other wrapper shapes are currently refused. For large workspaces,
`--fast` scans source without loading rust-analyzer; it requires the selected
name to be unique among workspace functions.

```sh
./target/debug/rust-refactor simplify-wrapper \
  --manifest-path /path/to/Cargo.toml \
  --file src/lib.rs --line 42 --column 8 --dry-run --format json
```

Review the JSON plan, then replace `--dry-run` with `--write`. A write formats
touched files and runs `cargo check`, restoring those files if verification
fails. Function-value references and calls the tool cannot rewrite cause a
refusal with exit code 3. Unqualified `drop` must resolve to the standard
function or be unshadowed in the defining source file.
