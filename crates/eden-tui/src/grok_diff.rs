//! Adapted from xAI Grok Build 2bdd1d6, xai-grok-pager-diff (Apache-2.0).
//! Copyright 2023–2026 xAI. Local changes: crate-private API, Eden rendering adapter.
use similar::{ChangeTag, TextDiff};
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DiffLine {
    pub(crate) text: String,
    pub(crate) lo: usize,
    pub(crate) ln: usize,
    pub(crate) tag: ChangeTag,
}

pub(crate) type DiffHunk = Vec<DiffLine>;

/// Unchanged lines kept on each side of a change
const MAX_CONTEXT: usize = 3;

/// Whole-file diffs run on the render thread; past this `similar` returns a coarser but still correct diff
const WHOLE_FILE_DIFF_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

pub(crate) fn diff_hunks_from_strings(
    old_text: &str,
    new_text: &str,
    start_line: usize,
) -> Vec<DiffHunk> {
    let diff = TextDiff::configure()
        .timeout(WHOLE_FILE_DIFF_TIMEOUT)
        .diff_lines(old_text, new_text);

    diff.grouped_ops(MAX_CONTEXT)
        .iter()
        .map(|group| {
            let mut hunk = DiffHunk::new();
            for op in group {
                let mut lo = op.old_range().start.saturating_add(start_line);
                let mut ln = op.new_range().start.saturating_add(start_line);
                for change in diff.iter_changes(op) {
                    let tag = change.tag();
                    hunk.push(DiffLine {
                        text: change.value().to_owned(),
                        lo,
                        ln,
                        tag,
                    });
                    match tag {
                        ChangeTag::Equal => {
                            lo = lo.saturating_add(1);
                            ln = ln.saturating_add(1);
                        }
                        ChangeTag::Delete => lo = lo.saturating_add(1),
                        ChangeTag::Insert => ln = ln.saturating_add(1),
                    }
                }
            }
            hunk
        })
        .collect()
}
