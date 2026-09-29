//! Display-only run statistics derived from tool lifecycle events.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    pub commands: usize,
    pub reads: usize,
    pub edits: usize,
    pub writes: usize,
    pub others: usize,
    pub failed: usize,
    pub duration_ms: u64,
}

#[derive(Default)]
pub struct RunStats {
    starts: usize,
    commands: usize,
    reads: BTreeSet<String>,
    edits: BTreeSet<String>,
    writes: BTreeSet<String>,
    others: usize,
    failed: usize,
}
impl RunStats {
    pub fn record_start(&mut self, name: &str, path: Option<&str>) {
        self.starts += 1;
        let files = match name.rsplit('.').next().unwrap_or(name) {
            "bash" | "powershell" => {
                self.commands += 1;
                None
            }
            "read" => Some(&mut self.reads),
            "edit" => Some(&mut self.edits),
            "write" => Some(&mut self.writes),
            _ => {
                self.others += 1;
                None
            }
        };
        if let (Some(files), Some(path)) = (files, path.filter(|path| !path.is_empty())) {
            files.insert(path.to_owned());
        }
    }
    pub fn record_result(&mut self, failed: bool) {
        self.failed += usize::from(failed);
    }
    pub fn finish(&self, duration_ms: u64) -> Option<RunSummary> {
        (self.starts >= 2).then_some(RunSummary {
            commands: self.commands,
            reads: self.reads.len(),
            edits: self.edits.len(),
            writes: self.writes.len(),
            others: self.others,
            failed: self.failed,
            duration_ms,
        })
    }
}

impl RunSummary {
    /// Text and exact number ranges share one formatter so styling cannot drift from copy text.
    pub fn formatted(&self) -> (String, Vec<(std::ops::Range<usize>, bool)>) {
        let mut text = String::new();
        let mut numbers = Vec::new();
        for (verb, count, unit, failed) in [
            ("ran ", self.commands, "command", false),
            ("read ", self.reads, "file", false),
            ("edited ", self.edits, "file", false),
            ("wrote ", self.writes, "file", false),
            ("", self.others, "other tool", false),
            ("", self.failed, "failed", true),
        ] {
            if count == 0 {
                continue;
            }
            if !text.is_empty() {
                text.push_str(", ");
            }
            text.push_str(verb);
            let start = text.len();
            text.push_str(&count.to_string());
            numbers.push((start..text.len(), failed));
            text.push(' ');
            text.push_str(unit);
            if count != 1 && !failed {
                text.push('s');
            }
        }
        if let Some(first) = text.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        if !text.is_empty() && self.duration_ms >= 1000 {
            let seconds = self.duration_ms / 1000;
            text.push_str(" · ");
            if seconds >= 3600 {
                text.push_str(&format!(
                    "{}h {}m {}s",
                    seconds / 3600,
                    seconds / 60 % 60,
                    seconds % 60
                ));
            } else if seconds >= 60 {
                text.push_str(&format!("{}m {}s", seconds / 60, seconds % 60));
            } else {
                text.push_str(&format!("{seconds}s"));
            }
        }
        (text, numbers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_file_counts_are_per_category_and_failures_are_additive() {
        let mut run = RunStats::default();
        for (name, path) in [
            ("read", Some("中文.rs")),
            ("read", Some("中文.rs")),
            ("read", Some("missing.rs")),
            ("read", Some("")),
            ("read", None),
            ("edit", Some("中文.rs")),
            ("write", Some("中文.rs")),
            ("bash", None),
            ("powershell", None),
            ("mcp__files__read", Some("extra.rs")),
        ] {
            run.record_start(name, path);
        }
        run.record_result(false);
        run.record_result(true);
        assert_eq!(
            run.finish(42_999),
            Some(RunSummary {
                commands: 2,
                reads: 2,
                edits: 1,
                writes: 1,
                others: 1,
                failed: 1,
                duration_ms: 42_999,
            })
        );
    }

    #[test]
    fn summary_requires_two_tool_starts() {
        let mut run = RunStats::default();
        assert_eq!(run.finish(42_000), None);
        run.record_start("bash", None);
        assert_eq!(run.finish(42_000), None);
        run.record_start("bash", None);
        assert!(run.finish(42_000).is_some());
    }

    #[test]
    fn complete_reference_wording_and_colored_number_ranges() {
        let summary = RunSummary {
            commands: 4,
            reads: 4,
            edits: 1,
            writes: 1,
            others: 12,
            failed: 2,
            duration_ms: 42_999,
        };
        let (text, ranges) = summary.formatted();
        assert_eq!(
            text,
            "Ran 4 commands, read 4 files, edited 1 file, wrote 1 file, 12 other tools, 2 failed \
             · 42s"
        );
        assert_eq!(
            ranges
                .iter()
                .map(|(range, failed)| (&text[range.clone()], *failed))
                .collect::<Vec<_>>(),
            vec![
                ("4", false),
                ("4", false),
                ("1", false),
                ("1", false),
                ("12", false),
                ("2", true)
            ]
        );
    }

    #[test]
    fn zero_categories_are_omitted_and_first_verb_is_capitalized() {
        assert_eq!(
            RunSummary {
                reads: 1,
                others: 1,
                duration_ms: 999,
                ..RunSummary::default()
            }
            .formatted()
            .0,
            "Read 1 file, 1 other tool"
        );
        assert_eq!(RunSummary::default().formatted().0, "");
    }

    #[test]
    fn duration_uses_reference_minutes_and_hours_format() {
        assert_eq!(
            RunSummary {
                commands: 1,
                duration_ms: 62_500,
                ..RunSummary::default()
            }
            .formatted()
            .0,
            "Ran 1 command · 1m 2s"
        );
        assert_eq!(
            RunSummary {
                commands: 1,
                duration_ms: 3_723_000,
                ..RunSummary::default()
            }
            .formatted()
            .0,
            "Ran 1 command · 1h 2m 3s"
        );
    }
}
