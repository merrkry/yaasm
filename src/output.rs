use std::collections::BTreeSet;

use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillChange {
    Added,
    Updated,
    Unchanged,
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
    let label = match group_style {
        GroupStyle::Added => style(label).green().bold(),
        GroupStyle::Updated => style(label).cyan().bold(),
        GroupStyle::Removed => style(label).red().bold(),
    };
    println!(
        "{label}: {}",
        names.iter().cloned().collect::<Vec<_>>().join(", ")
    );
}
