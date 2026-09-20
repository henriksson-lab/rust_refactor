# imod-rs case study

This records use of `rust-refactor` on the separate `imod-rs` C/C++
translation. It is historical context, not a list of current line numbers.
Run `to-oop-stats` again before choosing selections in that workspace.

## Early candidates and limits

Source inspection first identified `flip_clips` in `libimod/imodel.rs`: it
took `clips: &mut Iclip_planes`, and its struct was in the same module. A
semantic preview on the large workspace took too long to complete. Later,
`--fast` made syntax-based planning practical there. Source discovery and
preview skip `target`, `.tmp`, and agent worktree directories; clearing `.tmp`
was unnecessary.

Nearby functions such as `cleanup_scan_contour` in `icont.rs` and
`clips_assign` in `iview.rs` use structs defined in another module. Many
`imod_*` functions use `Option<&T>` or `Option<&mut T>`; moving them to a
receiver requires handling the `None` branch and remains outside the direct
`to-oop` transformation.

## Applied conversions

The first `Nnpi` run moved `nnpi_setwmin` and three `nnpi_get_*` functions
into `impl Nnpi`, updating calls in `nnpi.rs` and `nnai.rs`. The semantic
post-check was too slow on this workspace, so later runs used `--fast` and
separate Cargo verification.

Stats then identified several dense state groups. Batch conversions moved
functions for `B3dGfxState`, `MovieConState`, `MvImageState`, `FgData`,
`ContourEditState` (33), `InfoCbState` (28), and `SlicerRegistry` (19). Methods
for one struct were combined in one `impl`. Some batches needed dependency
ordering: when a selected function called another selected function, their
edits overlapped, so the called functions were moved first and caller
selections were refreshed afterward. Later macro support allowed
`ContourEditState` functions containing `format!` and `vec!` to move too.

A later pass converted all direct candidates for `MvOglState` (23),
`MvMovieState` (21), `SlicerRegistry` (19), and `ImodPlugState` (18).
`ImodIoState` converted 13 of 14; `imod_autosave` remains a free function
because it is passed as a callback value. Moving it to a method would require
changing that callback interface. Unsafe free functions moved to unsafe
methods, and direct calls inside test macros are now rewritten by fast mode.

`UndoRedo` was a poor bulk candidate despite a high stats count. Its free
functions already forwarded to existing methods; moving those functions
would preserve redundant wrappers. Replacing their call sites with the
existing methods and removing the wrappers is a separate cleanup. Geometry
helpers on `Ipoint` remain OOP candidates even when several arguments have
equal roles.

The imod checkout was being edited concurrently. Before each write, workers
coordinated files that could contain call sites. The tool reports the edited
file list, and Cargo checks were run during the conversion sequence.
