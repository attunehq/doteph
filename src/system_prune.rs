//! CLI boundary for cross-workspace pruning.

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use eph::{PruneOptions, PruneReport};
use std::io::{self, IsTerminal, Write};

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

    /// Remove a stale workspace's resources even if it still has running
    /// containers or a live `run=` process. Without this, a workspace whose
    /// recorded path is gone only because it was moved or renamed is reported
    /// and left alone.
    #[arg(long)]
    force_live: bool,

    /// Skip the removal confirmation prompt.
    #[arg(short = 'y', long)]
    yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExecutionOptions {
    dry_run: bool,
    compatibility_v042: bool,
    force_non_empty: bool,
    force_live: bool,
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
        };

        if options.dry_run {
            let report = eph::prune(preview_options).await?;
            print_report(&report);
            return Ok(());
        }

        let preview = eph::prune(preview_options).await?;
        print_report(&preview);

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
        })
        .await?;
        print_report(&report);
        Ok(())
    }
}

fn print_report(report: &PruneReport) {
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
                yes: true,
            }
        );
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
