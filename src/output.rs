use std::collections::BTreeSet;

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

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

        print_group("Added", added, GroupStyle::Added);
        print_group("Updated", updated, GroupStyle::Updated);
        print_group("Removed", removed, GroupStyle::Removed);
    }

    pub fn update_summary(&self, reports: &[SourceUpdateReport]) {
        self.finish();
        if reports.is_empty() {
            println!("{}", style("Already up to date").dim());
            return;
        }

        for report in reports {
            match &report.previous_revision {
                Some(previous) => println!(
                    "{} {}: {} -> {}",
                    style("Updated").cyan().bold(),
                    report.name,
                    short_revision(previous),
                    short_revision(&report.new_revision),
                ),
                None => println!(
                    "{} {}: {}",
                    style("Locked").green().bold(),
                    report.name,
                    short_revision(&report.new_revision),
                ),
            }
            print_indented_group("Added skills", &report.added_skills, GroupStyle::Added);
            print_indented_group("Moved skills", &report.moved_skills, GroupStyle::Updated);
            print_indented_group(
                "Removed skills",
                &report.removed_skills,
                GroupStyle::Removed,
            );
            if let Some(reason) = &report.diff_unavailable {
                println!("  {}: {reason}", style("Skill diff unavailable").yellow());
            }
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

enum GroupStyle {
    Added,
    Updated,
    Removed,
}

fn print_group(label: &str, names: &BTreeSet<String>, group_style: GroupStyle) {
    if names.is_empty() {
        return;
    }
    let label = style_label(label, group_style);
    println!(
        "{label}: {}",
        names.iter().cloned().collect::<Vec<_>>().join(", ")
    );
}

fn print_indented_group(label: &str, names: &BTreeSet<String>, group_style: GroupStyle) {
    if names.is_empty() {
        return;
    }
    let label = style_label(label, group_style);
    println!(
        "  {label}: {}",
        names.iter().cloned().collect::<Vec<_>>().join(", ")
    );
}

fn style_label(label: &str, group_style: GroupStyle) -> console::StyledObject<&str> {
    match group_style {
        GroupStyle::Added => style(label).green().bold(),
        GroupStyle::Updated => style(label).cyan().bold(),
        GroupStyle::Removed => style(label).red().bold(),
    }
}
