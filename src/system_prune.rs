//! CLI boundary for cross-workspace pruning.

use crate::system_ls::{format_age, workspace_table, workspace_totals};
use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use eph::{PruneOptions, PruneReport};
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

        if options.dry_run {
            let report = eph::prune(preview_options).await?;
            print_report(&report, true);
            return Ok(());
        }

        let preview = eph::prune(preview_options).await?;
        print_report(&preview, true);

        match eph::confirmation_outcome(
            !preview.totals.is_empty(),
            options.yes,
            io::stdin().is_terminal(),
        ) {
            eph::ConfirmationOutcome::Proceed => {}
            eph::ConfirmationOutcome::RequireYes => {
                anyhow::bail!(
                    "stdin is not a terminal, so system prune cannot prompt for confirmation; pass -y/--yes or --force to remove these resources without asking"
                );
            }
            eph::ConfirmationOutcome::Prompt => {
                print!("\nRemove these resources? [y/N] ");
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
        // The kept table did not change between preview and removal and was
        // just shown, so the completion report leaves it out.
        print_report(&report, false);
        Ok(())
    }
}

fn print_report(report: &PruneReport, with_kept: bool) {
    let title = if report.dry_run {
        "System prune dry run:"
    } else {
        "System prune complete:"
    };
    println!("{title}");

    if report.pruned.is_empty() {
        println!("  No stale workspaces found");
    } else {
        for workspace in &report.pruned {
            let path = workspace
                .workspace_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<workspace metadata unavailable>".to_string());
            println!("  {} ({}) - {}", workspace.short_id, workspace.reason, path);
            println!(
                "    containers: {}, volumes: {}, networks: {}, images: {}, run processes: {}, state dirs: {}",
                workspace.counts.containers,
                workspace.counts.volumes,
                workspace.counts.networks,
                workspace.counts.images,
                workspace.counts.processes,
                workspace.counts.state_dirs
            );
        }
    }

    println!();
    println!("Totals:");
    println!("  Containers: {}", report.totals.containers);
    println!("  Volumes: {}", report.totals.volumes);
    println!("  Networks: {}", report.totals.networks);
    println!("  Images: {}", report.totals.images);
    println!("  Verified run= processes: {}", report.totals.processes);
    println!("  State directories: {}", report.totals.state_dirs);

    if with_kept && !report.kept.is_empty() {
        println!();
        println!("Kept (workspace still exists; oldest first):");
        for line in workspace_table(&report.kept, report.now_unix_secs, false) {
            println!("{line}");
        }
        let oldest = report
            .kept
            .first()
            .map(|workspace| format_age(workspace.idle_secs(report.now_unix_secs)))
            .unwrap_or_default();
        println!(
            "  {}; oldest last seen {oldest} ago. Select with --idle DURATION, --merged, or --force-non-empty.",
            workspace_totals(&report.kept)
        );
    }

    if !report.skipped.is_empty() {
        println!();
        println!("Skipped:");
        for skipped in &report.skipped {
            let path = skipped
                .workspace_path
                .as_ref()
                .map(|p| format!(" ({})", p.display()))
                .unwrap_or_default();
            println!("  {}{} - {}", skipped.short_id, path, skipped.reason);
        }
    }

    if !report.warnings.is_empty() {
        println!();
        println!("Warnings:");
        for warning in &report.warnings {
            println!("  {warning}");
        }
    }
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
    fn individual_overrides_remain_independent() {
        let options = parse(&["--force-non-empty"]).execution_options();

        assert!(!options.compatibility_v042);
        assert!(options.force_non_empty);
        assert!(!options.force_live);
        assert!(!options.yes);
    }
}
