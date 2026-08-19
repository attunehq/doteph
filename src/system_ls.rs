//! CLI boundary for `eph system ls`, plus the workspace table shared with
//! `eph system prune`'s "Kept" section.

use anyhow::Result;
use clap::Args as ClapArgs;
use eph::{PathState, WorkspaceSummary};

/// Arguments accepted by `eph system ls`.
#[derive(Debug, ClapArgs)]
pub(crate) struct Args {}

impl Args {
    /// Print every known workspace with the signals prune uses to select it.
    pub(crate) async fn run(self) -> Result<()> {
        let workspaces = eph::list_workspaces().await?;
        if workspaces.is_empty() {
            println!("No eph workspaces recorded");
            return Ok(());
        }
        let now = eph::current_unix_secs();
        for line in workspace_table(&workspaces, now, true) {
            println!("{line}");
        }
        println!();
        println!("{}", workspace_totals(&workspaces));
        Ok(())
    }
}

/// Render workspaces as an aligned table, one line per row plus a header.
/// `with_state` adds the path-state column, which `prune` omits because every
/// kept workspace is present by definition.
pub(crate) fn workspace_table(
    workspaces: &[WorkspaceSummary],
    now: u64,
    with_state: bool,
) -> Vec<String> {
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(workspaces.len() + 1);
    let mut header = vec!["ID".to_string()];
    if with_state {
        header.push("PATH".to_string());
    }
    header.extend(
        [
            "LAST SEEN",
            "PROCS",
            "CONTAINERS",
            "VOLUMES",
            "BRANCH",
            "WORKSPACE",
        ]
        .iter()
        .map(ToString::to_string),
    );
    rows.push(header);
    for workspace in workspaces {
        let mut row = vec![workspace.short_id.clone()];
        if with_state {
            row.push(workspace.path_state.label().to_string());
        }
        row.extend([
            format_age(workspace.idle_secs(now)),
            workspace.live_processes.to_string(),
            format!("{}/{}", workspace.running_containers, workspace.containers),
            workspace.volumes.to_string(),
            workspace.merge.label().to_string(),
            workspace.workspace_path.display().to_string(),
        ]);
        rows.push(row);
    }
    render_table(&rows)
}

/// Align `rows` into columns, two spaces apart, each line indented by two
/// spaces. Every row must have the same number of cells; the last cell is not
/// padded, so long paths in a final column do not drag trailing whitespace.
pub(crate) fn render_table(rows: &[Vec<String>]) -> Vec<String> {
    let columns = rows.first().map_or(0, Vec::len);
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            rows.iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    rows.iter()
        .map(|row| {
            let mut line = String::from("  ");
            for (column, cell) in row.iter().enumerate() {
                if column + 1 == columns {
                    line.push_str(cell);
                } else {
                    line.push_str(&format!("{cell:<width$}  ", width = widths[column]));
                }
            }
            line
        })
        .collect()
}

/// One-line summary of the live footprint across `workspaces`. The present
/// count appears only when some workspace is not present, so a list where
/// every path still exists (prune's "Kept" table) does not repeat itself.
pub(crate) fn workspace_totals(workspaces: &[WorkspaceSummary]) -> String {
    let present = workspaces
        .iter()
        .filter(|workspace| workspace.path_state == PathState::Present)
        .count();
    let present = if present == workspaces.len() {
        String::new()
    } else {
        format!(" ({present} present)")
    };
    let processes: usize = workspaces.iter().map(|w| w.live_processes).sum();
    let running: usize = workspaces.iter().map(|w| w.running_containers).sum();
    let volumes: usize = workspaces.iter().map(|w| w.volumes).sum();
    format!(
        "{}{present}, {processes} live run= process{}, {running} running container{}, {}",
        count_noun(workspaces.len(), "workspace"),
        if processes == 1 { "" } else { "es" },
        plural(running),
        count_noun(volumes, "volume"),
    )
}

/// `1 container`, `2 containers`: a count with its regularly pluralized noun.
pub(crate) fn count_noun(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", plural(count))
}

pub(crate) fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Render an age in the largest whole unit: `45s`, `12m`, `14h`, `19d`.
pub(crate) fn format_age(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eph::MergeStatus;
    use std::path::PathBuf;

    fn summary(short_id: &str, last_seen: u64) -> WorkspaceSummary {
        WorkspaceSummary {
            short_id: short_id.to_string(),
            workspace_path: PathBuf::from("/work/app"),
            path_state: PathState::Present,
            last_seen_unix_secs: last_seen,
            merge: MergeStatus::Merged,
            live_processes: 2,
            running_containers: 1,
            containers: 3,
            volumes: 1,
        }
    }

    #[test]
    fn ages_use_the_largest_whole_unit() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(59), "59s");
        assert_eq!(format_age(60), "1m");
        assert_eq!(format_age(3599), "59m");
        assert_eq!(format_age(3600), "1h");
        assert_eq!(format_age(86_399), "23h");
        assert_eq!(format_age(86_400), "1d");
        assert_eq!(format_age(86_400 * 19 + 3600), "19d");
    }

    #[test]
    fn table_aligns_columns_and_includes_every_signal() {
        let lines = workspace_table(
            &[summary("a1b2c3d4e5f60718", 1000)],
            1000 + 86_400 * 2,
            false,
        );
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with(
            "  ID                LAST SEEN  PROCS  CONTAINERS  VOLUMES  BRANCH  WORKSPACE"
        ));
        assert_eq!(
            lines[1],
            "  a1b2c3d4e5f60718  2d         2      1/3         1        merged  /work/app"
        );
    }

    #[test]
    fn table_with_state_adds_the_path_column() {
        let lines = workspace_table(&[summary("a1b2c3d4e5f60718", 0)], 0, true);
        assert!(lines[0].contains("  PATH     "));
        assert!(lines[1].contains("  present  "));
    }

    #[test]
    fn totals_pluralize_each_count() {
        let mut one = summary("a1b2c3d4e5f60718", 0);
        one.live_processes = 1;
        one.running_containers = 1;
        one.volumes = 1;
        assert_eq!(
            workspace_totals(&[one]),
            "1 workspace, 1 live run= process, 1 running container, 1 volume"
        );
        let mut missing = summary("b", 0);
        missing.path_state = PathState::Missing;
        let lines = workspace_totals(&[summary("a", 0), missing]);
        assert_eq!(
            lines,
            "2 workspaces (1 present), 4 live run= processes, 2 running containers, 2 volumes"
        );
    }
}
