use std::collections::BTreeSet;
use std::fmt::Display;

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

const INDENT_WIDTH: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillChange {
    Added,
    Updated,
    Unchanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceUpdateReport {
    pub name: String,
    pub previous_revision: Option<String>,
    pub new_revision: String,
    pub added_skills: BTreeSet<String>,
    pub removed_skills: BTreeSet<String>,
    pub moved_skills: BTreeSet<String>,
    pub diff_unavailable: Option<String>,
}

pub struct Output {
    progress: MultiProgress,
    phase: ProgressBar,
}

impl Output {
    pub fn new() -> Self {
        let progress = MultiProgress::new();
        let phase = progress.add(ProgressBar::new_spinner());
        phase.set_style(spinner_style());
        phase.enable_steady_tick(std::time::Duration::from_millis(80));
        phase.set_message("Preparing");
        Self { progress, phase }
    }

    pub fn phase(&self, message: impl Into<String>) {
        self.phase.set_message(message.into());
    }

    pub fn source(&self, name: &str) -> ProgressBar {
        let bar = self
            .progress
            .insert_before(&self.phase, ProgressBar::new_spinner());
        bar.set_style(spinner_style());
        bar.enable_steady_tick(std::time::Duration::from_millis(80));
        bar.set_message(format!("Syncing source {name}"));
        bar
    }

    pub fn finish(&self) {
        self.phase.finish_and_clear();
        let _ = self.progress.clear();
    }

    pub fn summary(
        &self,
        added: &BTreeSet<String>,
        updated: &BTreeSet<String>,
        removed: &BTreeSet<String>,
    ) {
        self.finish();
        if added.is_empty() && updated.is_empty() && removed.is_empty() {
            println!("{}", style("Already up to date").dim());
            return;
        }

        let out = Indented::new();
        write_changes(&out, ChangeSymbol::Added, added);
        write_changes(&out, ChangeSymbol::Changed, updated);
        write_changes(&out, ChangeSymbol::Removed, removed);
    }

    pub fn update_summary(&self, reports: &[SourceUpdateReport]) {
        self.finish();
        if reports.is_empty() {
            println!("{}", style("Already up to date").dim());
            return;
        }

        let out = Indented::new();
        for report in reports {
            match &report.previous_revision {
                Some(previous) => out.line(format_args!(
                    "{} {}: {} -> {}",
                    style("Updated").cyan().bold(),
                    report.name,
                    short_revision(previous),
                    short_revision(&report.new_revision),
                )),
                None => out.line(format_args!(
                    "{} {}: {}",
                    style("Locked").green().bold(),
                    report.name,
                    short_revision(&report.new_revision),
                )),
            }
            out.nested(|child| {
                write_changes(child, ChangeSymbol::Added, &report.added_skills);
                write_changes(child, ChangeSymbol::Changed, &report.moved_skills);
                write_changes(child, ChangeSymbol::Removed, &report.removed_skills);
                if let Some(reason) = &report.diff_unavailable {
                    child.line(format_args!(
                        "{}: {reason}",
                        style("Skill diff unavailable").yellow()
                    ));
                }
            });
        }
    }
}

fn short_revision(revision: &str) -> &str {
    revision.get(..12).unwrap_or(revision)
}

impl Default for Output {
    fn default() -> Self {
        Self::new()
    }
}

fn spinner_style() -> ProgressStyle {
    ProgressStyle::with_template("{spinner:.cyan.bold} {msg}")
        .expect("valid progress template")
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"])
}

struct Indented {
    depth: usize,
}

impl Indented {
    fn new() -> Self {
        Self { depth: 0 }
    }

    fn line(&self, content: impl Display) {
        println!("{:width$}{content}", "", width = self.depth * INDENT_WIDTH);
    }

    fn nested(&self, f: impl FnOnce(&Self)) {
        f(&Self {
            depth: self.depth + 1,
        });
    }
}

#[derive(Clone, Copy)]
enum ChangeSymbol {
    Added,
    Changed,
    Removed,
}

impl ChangeSymbol {
    fn styled(self) -> console::StyledObject<&'static str> {
        match self {
            Self::Added => style("+").green().bold(),
            Self::Changed => style("~").cyan().bold(),
            Self::Removed => style("-").red().bold(),
        }
    }
}

fn write_changes(out: &Indented, symbol: ChangeSymbol, names: &BTreeSet<String>) {
    let prefix = symbol.styled();
    for name in names {
        out.line(format_args!("{prefix} {name}"));
    }
}
