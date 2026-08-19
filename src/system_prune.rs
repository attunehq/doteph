//! CLI boundary for cross-workspace pruning.

use crate::system_ls::{count_noun, render_table, workspace_table, workspace_totals};
use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use eph::prune::PruneCounts;
use eph::{PruneOptions, PruneReport, PruneWarning};
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

/// Arguments accepted by `eph system prune`.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {
    /// Print what would be removed without deleting anything.
    #[arg(long)]
    dry_run: bool,

    /// Enable every destructive override and skip confirmation.
    #[arg(long)]
    force: bool,

    /// Also prune state directories written by eph v0.4.2 and earlier.
    #[arg(long)]
    compatibility_v042: bool,

    /// Remove resources for recorded workspace paths that still contain
    /// files. Live resources still require --force-live.
    #[arg(long)]
    force_non_empty: bool,

    /// Remove a stale workspace's resources even if it still has a running
    /// container (or, for --force-non-empty, a live `run=` process). Without
    /// this, a workspace whose recorded path is gone only because it was moved
    /// or renamed is reported and left alone.
    #[arg(long)]
    force_live: bool,

    /// Also prune workspaces no eph command has touched for this long, e.g.
    /// 2d, 12h, 30m. Their `run=` processes are terminated with the rest.
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    idle: Option<Duration>,

    /// Also prune workspaces whose git branch is merged into the repository's
    /// default branch and whose working tree is clean.
    #[arg(long)]
    merged: bool,

    /// Skip the removal confirmation prompt.
    #[arg(short = 'y', long)]
    yes: bool,
}

/// Parse `<n><unit>` with unit `s`, `m`, `h`, or `d`.
fn parse_duration(text: &str) -> Result<Duration, String> {
    let text = text.trim();
    let (digits, unit) = text.split_at(
        text.find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len()),
    );
    let amount: u64 = digits
        .parse()
        .map_err(|_| format!("expected a duration like 2d, 12h, 30m, or 90s, got {text:?}"))?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(format!("expected a unit of s, m, h, or d, got {text:?}")),
    };
    Ok(Duration::from_secs(amount.saturating_mul(secs)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecutionOptions {
    dry_run: bool,
    compatibility_v042: bool,
    force_non_empty: bool,
    force_live: bool,
    idle: Option<Duration>,
    merged: bool,
    yes: bool,
}

impl Args {
    /// Collapse CLI aliases before any prune decisions are made, so `--force`
    /// cannot drift from a newly added destructive override in one execution
    /// path while remaining correct in another.
    fn execution_options(&self) -> ExecutionOptions {
        ExecutionOptions {
            dry_run: self.dry_run,
            compatibility_v042: self.force || self.compatibility_v042,
            force_non_empty: self.force || self.force_non_empty,
            force_live: self.force || self.force_live,
            idle: self.idle,
            merged: self.merged,
            yes: self.force || self.yes,
        }
    }

    /// Report what would be torn down, then confirm and perform the removal.
    pub(crate) async fn run(self) -> Result<()> {
        let options = self.execution_options();
        let preview_options = PruneOptions {
            dry_run: true,
            compatibility_v042: options.compatibility_v042,
            force_non_empty: options.force_non_empty,
            force_live: options.force_live,
            idle: options.idle,
            merged: options.merged,
        };

        let preview = eph::prune(preview_options).await?;
        print_preview(&preview);
        if options.dry_run || preview.pruned.is_empty() {
            return Ok(());
        }

        match eph::confirmation_outcome(true, options.yes, io::stdin().is_terminal()) {
            eph::ConfirmationOutcome::Proceed => {}
            eph::ConfirmationOutcome::RequireYes => {
                anyhow::bail!(
                    "stdin is not a terminal, so system prune cannot prompt for confirmation; pass -y/--yes or --force to remove these resources without asking"
                );
            }
            eph::ConfirmationOutcome::Prompt => {
                print!(
                    "\nRemove resources for {}? [y/N] ",
                    count_noun(preview.pruned.len(), "workspace")
                );
                io::stdout()
                    .flush()
                    .context("failed to write the prune confirmation prompt")?;

                let mut answer = String::new();
                io::stdin()
                    .read_line(&mut answer)
                    .context("failed to read the prune confirmation")?;
                let answer = answer.trim();
                if !answer.eq_ignore_ascii_case("y") && !answer.eq_ignore_ascii_case("yes") {
                    println!("Aborted; nothing removed.");
                    return Ok(());
                }
            }
        }

        let report = eph::prune(PruneOptions {
            dry_run: false,
            compatibility_v042: options.compatibility_v042,
            force_non_empty: options.force_non_empty,
            force_live: options.force_live,
            idle: options.idle,
            merged: options.merged,
        })
        .await?;
        println!();
        print_completion(&report);
        Ok(())
    }
}

/// The selection report: every workspace that would be removed and why,
/// followed by the workspaces left alone. Printed for `--dry-run` and as the
/// preview before the confirmation prompt.
fn print_preview(report: &PruneReport) {
    if report.pruned.is_empty() {
        println!("Nothing to prune.");
    } else {
        println!(
            "Would remove {} ({}):",
            count_noun(report.pruned.len(), "workspace"),
            counts_summary(&report.totals, true)
        );
        let mut rows = vec![
            ["ID", "REASON", "RESOURCES", "WORKSPACE"]
                .map(str::to_string)
                .to_vec(),
        ];
        rows.extend(report.pruned.iter().map(|workspace| {
            vec![
                workspace.short_id.clone(),
                workspace.reason.to_string(),
                counts_summary(&workspace.counts, false),
                workspace
                    .workspace_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "(no workspace metadata)".to_string()),
            ]
        }));
        for line in render_table(&rows) {
            println!("{line}");
        }
    }

    if !report.kept.is_empty() {
        println!();
        println!(
            "Kept {} (path still exists; oldest first):",
            count_noun(report.kept.len(), "workspace")
        );
        for line in workspace_table(&report.kept, report.now_unix_secs, false) {
            println!("{line}");
        }
        println!("  {}", workspace_totals(&report.kept));
        println!("  Select these with --idle DURATION, --merged, or --force-non-empty.");
    }

    print_skipped_and_warnings(report);
}

/// The outcome of a real removal. The preview already listed each workspace,
/// so this repeats only the totals plus anything new the removal turned up.
fn print_completion(report: &PruneReport) {
    println!(
        "Removed {} ({}).",
        count_noun(report.pruned.len(), "workspace"),
        counts_summary(&report.totals, true)
    );
    print_skipped_and_warnings(report);
}

fn print_skipped_and_warnings(report: &PruneReport) {
    if !report.skipped.is_empty() {
        println!();
        println!("Skipped {}:", count_noun(report.skipped.len(), "workspace"));
        let mut rows = Vec::new();
        for skipped in &report.skipped {
            rows.push(vec![skipped.short_id.clone(), skipped.reason.clone()]);
            if let Some(path) = &skipped.workspace_path {
                rows.push(vec![String::new(), path.display().to_string()]);
            }
        }
        for line in render_table(&rows) {
            println!("{line}");
        }
    }

    if !report.warnings.is_empty() {
        println!();
        println!("{}:", count_noun(report.warnings.len(), "warning"));
        for line in grouped_warnings(&report.warnings) {
            println!("  {line}");
        }
    }
}

/// One line per distinct warning message. A message shared by several
/// workspaces (typically a whole batch of state written by an older eph)
/// appears once with the workspace count in place of a single ID.
fn grouped_warnings(warnings: &[PruneWarning]) -> Vec<String> {
    let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
    for warning in warnings {
        match groups
            .iter_mut()
            .find(|(message, _)| *message == warning.message)
        {
            Some((_, ids)) => ids.push(&warning.short_id),
            None => groups.push((&warning.message, vec![&warning.short_id])),
        }
    }
    groups
        .into_iter()
        .map(|(message, ids)| {
            let who = match ids.as_slice() {
                [id] => (*id).to_string(),
                ids => count_noun(ids.len(), "workspace"),
            };
            format!("{who:16}  {message}")
        })
        .collect()
}

/// `2 containers, 1 volume`: the non-zero counts in `counts`. State
/// directories are listed only `with_state`; each pruned workspace has exactly
/// one, so a per-workspace row that removes nothing else reads `state only`.
fn counts_summary(counts: &PruneCounts, with_state: bool) -> String {
    let mut parts = Vec::new();
    for (count, noun) in [
        (counts.containers, "container"),
        (counts.volumes, "volume"),
        (counts.networks, "network"),
        (counts.images, "image"),
        (counts.processes, "run= process"),
    ] {
        if count > 0 {
            parts.push(count_noun(count, noun));
        }
    }
    if with_state && counts.state_dirs > 0 {
        parts.push(format!(
            "{} state director{}",
            counts.state_dirs,
            if counts.state_dirs == 1 { "y" } else { "ies" }
        ));
    }
    if parts.is_empty() {
        return if with_state {
            "nothing".to_string()
        } else {
            "state only".to_string()
        };
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct TestCli {
        #[command(flatten)]
        args: Args,
    }

    fn parse(args: &[&str]) -> Args {
        TestCli::try_parse_from(std::iter::once("prune").chain(args.iter().copied()))
            .expect("system prune arguments should parse")
            .args
    }

    #[test]
    fn force_enables_every_destructive_override_and_confirmation_bypass() {
        let options = parse(&["--force"]).execution_options();

        assert_eq!(
            options,
            ExecutionOptions {
                dry_run: false,
                compatibility_v042: true,
                force_non_empty: true,
                force_live: true,
                idle: None,
                merged: false,
                yes: true,
            }
        );
    }

    #[test]
    fn idle_and_merged_are_selection_flags_not_overrides() {
        let options = parse(&["--idle", "2d", "--merged"]).execution_options();

        assert_eq!(options.idle, Some(Duration::from_secs(2 * 86_400)));
        assert!(options.merged);
        assert!(!options.force_non_empty);
        assert!(!options.force_live);
        assert!(!options.yes);

        let forced = parse(&["--force"]).execution_options();
        assert_eq!(forced.idle, None);
        assert!(!forced.merged);
    }

    #[test]
    fn durations_parse_whole_units_only() {
        assert_eq!(parse_duration("90s"), Ok(Duration::from_secs(90)));
        assert_eq!(parse_duration("30m"), Ok(Duration::from_secs(1800)));
        assert_eq!(parse_duration("12h"), Ok(Duration::from_secs(43_200)));
        assert_eq!(parse_duration(" 2d "), Ok(Duration::from_secs(172_800)));
        assert!(parse_duration("2").is_err());
        assert!(parse_duration("2w").is_err());
        assert!(parse_duration("1.5h").is_err());
        assert!(parse_duration("h").is_err());
    }

    #[test]
    fn force_can_preview_the_complete_destructive_scope() {
        let options = parse(&["--force", "--dry-run"]).execution_options();

        assert!(options.dry_run);
        assert!(options.compatibility_v042);
        assert!(options.force_non_empty);
        assert!(options.force_live);
        assert!(options.yes);
    }

    #[test]
    fn counts_summary_lists_only_non_zero_resources() {
        let counts = PruneCounts {
            containers: 2,
            volumes: 1,
            state_dirs: 1,
            ..PruneCounts::default()
        };
        assert_eq!(counts_summary(&counts, false), "2 containers, 1 volume");
        assert_eq!(
            counts_summary(&counts, true),
            "2 containers, 1 volume, 1 state directory"
        );
        let state_only = PruneCounts {
            state_dirs: 3,
            ..PruneCounts::default()
        };
        assert_eq!(counts_summary(&state_only, false), "state only");
        assert_eq!(counts_summary(&state_only, true), "3 state directories");
    }

    #[test]
    fn identical_warnings_collapse_to_one_line_with_a_count() {
        let warning = |short_id: &str, message: &str| PruneWarning {
            short_id: short_id.to_string(),
            message: message.to_string(),
        };
        let lines = grouped_warnings(&[
            warning("a1b2c3d4e5f60718", "state.json has no saved hook snapshot"),
            warning(
                "0123456789abcdef",
                "web post-stop hook failed: exit status 1",
            ),
            warning("e5f60718293a4b5c", "state.json has no saved hook snapshot"),
        ]);
        assert_eq!(
            lines,
            [
                "2 workspaces      state.json has no saved hook snapshot",
                "0123456789abcdef  web post-stop hook failed: exit status 1",
            ]
        );
    }

    #[test]
    fn individual_overrides_remain_independent() {
        let options = parse(&["--force-non-empty"]).execution_options();

        assert!(!options.compatibility_v042);
        assert!(options.force_non_empty);
        assert!(!options.force_live);
        assert!(!options.yes);
    }
}
