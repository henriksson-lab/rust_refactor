use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;
use std::{collections::BTreeMap, fs};
use text_size::TextRange;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefactorPlan {
    pub edits: Vec<TextEdit>,
    pub diagnostics: Vec<Diagnostic>,
}

impl RefactorPlan {
    pub fn empty() -> Self {
        Self {
            edits: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.level == DiagnosticLevel::Error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextEdit {
    pub file: Utf8PathBuf,
    pub range: TextRange,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticLevel {
    Error,
    Warning,
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: DiagnosticLevel,
    pub message: String,
    pub file: Option<Utf8PathBuf>,
    pub range: Option<TextRange>,
}

pub fn apply_plan(plan: &RefactorPlan) -> Result<()> {
    if plan.has_errors() {
        bail!("refactor plan contains errors");
    }

    let mut by_file: BTreeMap<&Utf8PathBuf, Vec<&TextEdit>> = BTreeMap::new();
    for edit in &plan.edits {
        by_file.entry(&edit.file).or_default().push(edit);
    }

    let mut prepared = Vec::new();
    for (file, edits) in by_file {
        let source = fs::read_to_string(file).with_context(|| format!("failed to read {file}"))?;
        let updated = apply_edits_to_string(&source, &edits)?;
        prepared.push((file, source, updated));
    }

    let mut written = Vec::new();
    for (file, original, updated) in &prepared {
        if let Err(error) = fs::write(file, updated) {
            fs::write(file, original)
                .with_context(|| format!("failed to roll back partially written {file}"))?;
            for (path, contents) in written {
                fs::write(path, contents).with_context(|| format!("failed to roll back {path}"))?;
            }
            return Err(error).with_context(|| format!("failed to write {file}"));
        }
        written.push((*file, original));
    }

    Ok(())
}

pub(crate) fn apply_edits_to_string(source: &str, edits: &[&TextEdit]) -> Result<String> {
    let mut edits = edits.to_vec();
    edits.sort_by_key(|edit| u32::from(edit.range.start()));

    for window in edits.windows(2) {
        if window[0].range.end() > window[1].range.start() {
            bail!("overlapping edits for range {:?}", window[1].range);
        }
    }

    for edit in &edits {
        let start = u32::from(edit.range.start()) as usize;
        let end = u32::from(edit.range.end()) as usize;
        if end > source.len() || !source.is_char_boundary(start) || !source.is_char_boundary(end) {
            bail!("invalid edit range {:?}", edit.range);
        }
    }

    let mut updated = source.to_owned();
    for edit in edits.into_iter().rev() {
        let start = u32::from(edit.range.start()) as usize;
        let end = u32::from(edit.range.end()) as usize;
        updated.replace_range(start..end, &edit.replacement);
    }

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use text_size::TextSize;

    fn edit(start: u32, end: u32, replacement: &str) -> TextEdit {
        TextEdit {
            file: Utf8PathBuf::from("src/lib.rs"),
            range: TextRange::new(TextSize::from(start), TextSize::from(end)),
            replacement: replacement.to_owned(),
        }
    }

    #[test]
    fn applies_edits_from_end_to_start() {
        let edits = [edit(0, 3, "one"), edit(8, 11, "three")];
        let refs = edits.iter().collect::<Vec<_>>();

        assert_eq!(
            apply_edits_to_string("two and two", &refs).unwrap(),
            "one and three"
        );
    }

    #[test]
    fn rejects_overlapping_edits() {
        let edits = [edit(0, 4, "x"), edit(3, 5, "y")];
        let refs = edits.iter().collect::<Vec<_>>();

        assert!(apply_edits_to_string("abcdef", &refs).is_err());
    }

    #[test]
    fn rejects_edit_inside_utf8_character() {
        let edits = [edit(1, 2, "x")];
        let refs = edits.iter().collect::<Vec<_>>();
        assert!(apply_edits_to_string("é", &refs).is_err());
    }
}
