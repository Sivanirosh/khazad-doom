//! Read-only renderer for the daemon-owned status feed.
//!
//! The public seam accepts typed projections plus display constraints. It
//! selects a glance mode, builds generic evidence sections, collapses them by
//! mode priority, and paints one bounded frame without acquiring workflow
//! authority or interpreting raw run/event data.

use crate::domain::{
    StatusAction, StatusFeed, StatusFeedBlock, StatusFeedBlockKind, StatusFeedRole,
    StatusLifecycleProjection, StatusPhaseProjection, TerminalReason,
};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MONITOR_LINE_WIDTH: usize = 96;
pub(crate) const MONITOR_FOOTER: &str = "read-only • Ctrl-C exits this view";

#[derive(Debug, Clone, Copy)]
pub(crate) struct MonitorStyle {
    width: usize,
    rows: Option<usize>,
    color: bool,
}

impl MonitorStyle {
    pub(crate) fn plain() -> Self {
        Self {
            width: MONITOR_LINE_WIDTH,
            rows: None,
            color: false,
        }
    }

    pub(crate) fn detect() -> Self {
        if !stdout_is_terminal() {
            return Self::plain();
        }
        let (width, rows) = terminal_size();
        Self {
            width: width.unwrap_or(MONITOR_LINE_WIDTH).max(1),
            rows,
            color: std::env::var_os("NO_COLOR").is_none()
                && std::env::var("TERM").map_or(true, |term| term != "dumb"),
        }
    }

    #[cfg(test)]
    fn fixed(width: usize, rows: Option<usize>, color: bool) -> Self {
        Self {
            width: width.max(1),
            rows,
            color,
        }
    }

    fn paint(self, text: &str, code: &str) -> String {
        if !self.color || code.is_empty() {
            text.to_string()
        } else {
            format!("\x1b[{code}m{text}\x1b[0m")
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MonitorMode {
    Nominal,
    Attention,
    Terminal,
}

#[derive(Debug)]
struct MonitorSection {
    full: String,
    collapsed: String,
    collapse_rank: Option<u8>,
    collapsed_selected: bool,
}

impl MonitorSection {
    fn fixed(full: String) -> Self {
        Self {
            collapsed: full.clone(),
            full,
            collapse_rank: None,
            collapsed_selected: false,
        }
    }

    fn collapsible(full: String, collapsed: String, collapse_rank: u8) -> Self {
        Self {
            full,
            collapsed,
            collapse_rank: Some(collapse_rank),
            collapsed_selected: false,
        }
    }

    fn selected(&self) -> &str {
        if self.collapsed_selected {
            &self.collapsed
        } else {
            &self.full
        }
    }

    fn selected_rows(&self, width: usize) -> usize {
        physical_rows(self.selected(), width)
    }
}

#[derive(Debug, Clone, Copy)]
struct MonitorLayout {
    compact: bool,
}

pub(crate) fn render_run(
    run_id: &str,
    repo_path: &str,
    feed: &StatusFeed,
    now: DateTime<Utc>,
    style: MonitorStyle,
) -> String {
    let mode = monitor_mode(feed);
    let mut sections = Vec::new();
    sections.push(MonitorSection::fixed(render_glance_header(
        run_id, repo_path, feed, mode, now, style,
    )));

    let resolved_attention_actions = resolve_attention_actions(feed, run_id);
    let mut rendered_commands = HashSet::new();

    match mode {
        MonitorMode::Attention => {
            sections.push(MonitorSection::fixed(render_attention_card(
                feed,
                &resolved_attention_actions,
                style,
            )));
            let actions = if resolved_attention_actions.is_empty() {
                Vec::new()
            } else {
                resolved_attention_actions.clone()
            };
            if !actions.is_empty() {
                rendered_commands.extend(actions.iter().map(|action| action.command.as_str()));
                sections.push(MonitorSection::fixed(render_actions(&actions, style)));
            } else if !feed.operator_commands.is_empty() {
                rendered_commands.extend(feed.operator_commands.iter().map(String::as_str));
                sections.push(MonitorSection::fixed(render_commands(
                    "Commands",
                    &feed.operator_commands,
                    style,
                )));
            }
        }
        MonitorMode::Terminal => {
            sections.push(MonitorSection::fixed(render_terminal_card(
                &feed.lifecycle,
                feed.terminal_reason.as_ref(),
                &feed.summary_line,
                style,
            )));
            let actions = resolve_terminal_actions(feed, run_id);
            if !actions.is_empty() {
                rendered_commands.extend(actions.iter().map(|action| action.command.as_str()));
                sections.push(MonitorSection::fixed(render_actions(&actions, style)));
            } else {
                let commands = feed
                    .terminal_reason
                    .as_ref()
                    .map(|reason| reason.operator_commands.as_slice())
                    .filter(|commands| !commands.is_empty())
                    .unwrap_or(&feed.operator_commands);
                if !commands.is_empty() {
                    rendered_commands.extend(commands.iter().map(String::as_str));
                    sections.push(MonitorSection::fixed(render_commands(
                        "Commands", commands, style,
                    )));
                }
            }
            if let Some(reason) = feed.terminal_reason.as_ref()
                && !reason.evidence_links.is_empty()
            {
                sections.push(MonitorSection::fixed(render_evidence_links(
                    &reason.evidence_links,
                    style,
                )));
            }
        }
        MonitorMode::Nominal => {
            let has_command_block = feed.blocks.iter().any(|block| {
                block.kind == StatusFeedBlockKind::Commands && !block.lines.is_empty()
            });
            if !has_command_block && !feed.operator_commands.is_empty() {
                rendered_commands.extend(feed.operator_commands.iter().map(String::as_str));
                sections.push(MonitorSection::fixed(render_commands(
                    "Commands",
                    &feed.operator_commands,
                    style,
                )));
            }
        }
    }

    if feed.gate.active || feed.repair.active {
        sections.push(section_for_active_checks(feed, mode, style));
    }

    let mut evidence = feed
        .blocks
        .iter()
        .enumerate()
        .filter_map(|(index, block)| {
            evidence_section(block, index, mode, &rendered_commands, style)
        })
        .collect::<Vec<_>>();
    evidence.sort_by_key(|(order, index, _)| (*order, *index));
    sections.extend(evidence.into_iter().map(|(_, _, section)| section));

    let layout = plan_sections(&mut sections, style);
    join_sections(&sections, layout.compact)
}

pub(crate) fn render_waiting(repo_path: &str, style: MonitorStyle) -> String {
    let repo = repo_label(repo_path);
    let mut header = String::new();
    let title = if repo.is_empty() {
        "Khazad-Doom Monitor".to_string()
    } else {
        format!("Khazad-Doom Monitor · {repo}")
    };
    let _ = writeln!(header, "{}", style.paint(&title, "1"));
    let _ = writeln!(header, "waiting for the latest active daemon-owned run");

    let mut hint = String::new();
    render_section_heading(&mut hint, "Hint", "", None, style);
    render_role_line(
        &mut hint,
        "start a run normally; this dashboard will attach when status --latest returns one",
        StatusFeedRole::Dim,
        style,
    );

    let mut sections = vec![MonitorSection::fixed(header), MonitorSection::fixed(hint)];
    let layout = plan_sections(&mut sections, style);
    join_sections(&sections, layout.compact)
}

pub(crate) fn compose_live_frame(content: &str, style: MonitorStyle) -> String {
    let (lines, kept, elided_lines) = bounded_body(content, style);
    let mut frame = String::from("\x1b[?2026h\x1b[H");
    for line in lines.iter().take(kept) {
        frame.push_str(line);
        frame.push_str("\x1b[K\n");
    }
    if elided_lines > 0 {
        let marker = format!("… {elided_lines} more lines — enlarge pane");
        frame.push_str(&style.paint(&truncate_display(&marker, style.width), "2"));
        frame.push_str("\x1b[K\n");
    }
    let footer = live_footer(style);
    if !footer.is_empty() {
        frame.push_str(&style.paint(&footer, "2"));
        frame.push_str("\x1b[K\n");
    }
    frame.push_str("\x1b[0J\x1b[?2026l");
    frame
}

pub(crate) fn compose_static_frame(content: &str, style: MonitorStyle) -> String {
    let (lines, kept, elided_lines) = bounded_body(content, style);
    let mut frame = String::new();
    for line in lines.iter().take(kept) {
        frame.push_str(line);
        frame.push('\n');
    }
    if elided_lines > 0 {
        let marker = format!("… {elided_lines} more lines — enlarge pane");
        frame.push_str(&style.paint(&truncate_display(&marker, style.width), "2"));
        frame.push('\n');
    }
    let footer = live_footer(style);
    if !footer.is_empty() {
        frame.push_str(&style.paint(&footer, "2"));
        frame.push('\n');
    }
    frame
}

fn bounded_body(content: &str, style: MonitorStyle) -> (Vec<&str>, usize, usize) {
    let lines = trimmed_logical_lines(content);
    let body_budget = body_row_budget(style).unwrap_or(usize::MAX);
    let total_rows = logical_lines_rows(&lines, style.width);
    let mut kept = lines.len();
    let mut elided_lines = 0usize;

    if total_rows > body_budget {
        let content_budget = body_budget.saturating_sub(1);
        let mut used = 0usize;
        kept = 0;
        for line in &lines {
            let rows = physical_rows_for_line(line, style.width);
            if used.saturating_add(rows) > content_budget {
                break;
            }
            used += rows;
            kept += 1;
        }
        if body_budget > 0 {
            elided_lines = lines.len().saturating_sub(kept);
        }
    }
    (lines, kept, elided_lines)
}

fn monitor_mode(feed: &StatusFeed) -> MonitorMode {
    if feed.lifecycle.terminal {
        MonitorMode::Terminal
    } else if !feed.attention_items.is_empty() {
        MonitorMode::Attention
    } else {
        MonitorMode::Nominal
    }
}

fn render_glance_header(
    run_id: &str,
    repo_path: &str,
    feed: &StatusFeed,
    mode: MonitorMode,
    now: DateTime<Utc>,
    style: MonitorStyle,
) -> String {
    let mut out = String::new();
    render_state_header(
        &mut out,
        run_id,
        repo_path,
        &feed.lifecycle,
        feed.worker_activity.updated_at,
        now,
        style,
    );
    if !feed.summary_line.trim().is_empty() {
        render_plain_wrapped(&mut out, &feed.summary_line, "", style);
    }
    if mode != MonitorMode::Terminal {
        let mut phase = Vec::new();
        if !feed.worker_activity.phase.trim().is_empty() {
            phase.push(format!("phase {}", feed.worker_activity.phase));
        }
        if !feed.worker_activity.slice_id.trim().is_empty() {
            phase.push(format!("slice {}", feed.worker_activity.slice_id));
        }
        if feed.worker_activity.attempt > 0 {
            phase.push(format!("attempt {}", feed.worker_activity.attempt));
        }
        if !phase.is_empty() {
            render_plain_wrapped(
                &mut out,
                &phase.join(" · "),
                monitor_role_code(StatusFeedRole::Dim),
                style,
            );
        }
    }
    out
}

fn render_state_header(
    out: &mut String,
    run_id: &str,
    repo_path: &str,
    lifecycle: &StatusLifecycleProjection,
    updated_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    style: MonitorStyle,
) {
    let (glyph, chip_code) = lifecycle_presentation(lifecycle);
    let state = if lifecycle.state.trim().is_empty() {
        "UNKNOWN".to_string()
    } else {
        lifecycle.state.trim().to_uppercase()
    };
    let mut chip_plain = format!("{glyph} {state}");
    chip_plain = truncate_display(&chip_plain, style.width.saturating_sub(1).max(1));

    let freshness = updated_at.map(|updated| freshness_labels(updated, now));
    let mut right = freshness.as_ref().map(|labels| labels.0.as_str());
    let leading_width = 1usize;
    if right.is_some_and(|text| {
        leading_width + display_width(&chip_plain) + 1 + display_width(text) > style.width
    }) {
        right = freshness.as_ref().map(|labels| labels.1.as_str());
    }
    if right.is_some_and(|text| {
        leading_width + display_width(&chip_plain) + 1 + display_width(text) > style.width
    }) {
        right = None;
    }

    let right_width = right.map(display_width).unwrap_or(0);
    let left_budget = style
        .width
        .saturating_sub(leading_width + right_width + usize::from(right.is_some()));
    let chip_width = display_width(&chip_plain).min(left_budget);
    let identity_budget = left_budget.saturating_sub(chip_width + 2);
    let identity = monitor_identity(run_id, repo_path, identity_budget);
    let left_width = chip_width
        + if identity.is_empty() {
            0
        } else {
            2 + display_width(&identity)
        };

    out.push(' ');
    out.push_str(&style.paint(&chip_plain, chip_code));
    if !identity.is_empty() {
        out.push_str("  ");
        out.push_str(&identity);
    }
    if let Some(right) = right {
        let spaces = style
            .width
            .saturating_sub(leading_width + left_width + right_width)
            .max(1);
        out.push_str(&" ".repeat(spaces));
        out.push_str(&style.paint(right, "2"));
    }
    out.push('\n');
}

fn lifecycle_presentation(lifecycle: &StatusLifecycleProjection) -> (&'static str, &'static str) {
    if lifecycle.successful {
        return ("✓", "1;7;32");
    }
    match lifecycle.state.as_str() {
        "running" => ("●", "1;7;36"),
        "pending" | "waiting" => ("○", "1;7"),
        "failed" => ("✕", "1;7;31"),
        "blocked" => ("!", "1;7;33"),
        "cancelled" | "interrupted" => ("!", "1;7;33"),
        _ if lifecycle.terminal => ("✕", "1;7;31"),
        _ => ("?", "1;7"),
    }
}

fn monitor_identity(run_id: &str, repo_path: &str, budget: usize) -> String {
    if budget == 0 {
        return String::new();
    }
    let repo = repo_label(repo_path);
    let full = if repo.is_empty() {
        run_id.to_string()
    } else {
        format!("{run_id} · {repo}")
    };
    if display_width(&full) <= budget {
        return full;
    }
    if display_width(run_id) <= budget {
        return run_id.to_string();
    }
    truncate_display(run_id, budget)
}

fn repo_label(repo_path: &str) -> String {
    crate::workflow::short_path(repo_path)
}

fn freshness_labels(updated_at: DateTime<Utc>, now: DateTime<Utc>) -> (String, String) {
    if updated_at <= now {
        let duration = (now - updated_at).to_std().unwrap_or_default();
        if duration.as_secs() == 0 {
            ("updated now".to_string(), "now".to_string())
        } else {
            let value = monitor_duration(duration);
            (format!("updated {value} ago"), format!("{value} ago"))
        }
    } else {
        let duration = (updated_at - now).to_std().unwrap_or_default();
        let value = monitor_duration(duration);
        (format!("clock skew +{value}"), format!("skew +{value}"))
    }
}

fn render_attention_card(
    feed: &StatusFeed,
    actions: &[&StatusAction],
    style: MonitorStyle,
) -> String {
    let actionable = !actions.is_empty();
    let title = if actionable { "NEEDS YOU" } else { "ATTENTION" };
    let count = feed.attention_items.len();
    let title = if count == 0 {
        title.to_string()
    } else {
        format!(
            "{title}  {count} {}",
            if count == 1 { "item" } else { "items" }
        )
    };
    let mut out = String::new();
    render_rail_line(&mut out, &title, "1;33", true, style);

    let action_index = actions
        .iter()
        .map(|action| (action.id.as_str(), *action))
        .collect::<HashMap<_, _>>();
    let mut items = feed.attention_items.iter().collect::<Vec<_>>();
    items.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.id.cmp(&right.id))
    });
    for item in items {
        let target = item
            .action_ids
            .iter()
            .filter_map(|id| action_index.get(id.as_str()).copied())
            .find_map(|action| {
                (!action.target_id.trim().is_empty()).then_some(action.target_id.as_str())
            });
        let kind = humanize_kind(&item.kind);
        let summary = item.summary.trim();
        let text = match (target, summary.is_empty()) {
            (Some(target), false) => format!("{target} · {kind}: {summary}"),
            (Some(target), true) => format!("{target} · {kind}"),
            (None, false) => format!("{kind}: {summary}"),
            (None, true) => kind,
        };
        render_rail_line(&mut out, &text, "33", false, style);
    }
    out
}

fn render_terminal_card(
    lifecycle: &StatusLifecycleProjection,
    reason: Option<&TerminalReason>,
    summary_line: &str,
    style: MonitorStyle,
) -> String {
    let successful = lifecycle.successful;
    let title = if successful {
        "OUTCOME"
    } else {
        "WHAT STOPPED IT"
    };
    let rail_code = if successful { "1;32" } else { "1;31" };
    let mut out = String::new();
    render_rail_line(&mut out, title, rail_code, true, style);

    if let Some(reason) = reason {
        let retry = if reason.retryable {
            "retryable"
        } else {
            "not retryable"
        };
        let mut facts = vec![if reason.kind.trim().is_empty() {
            "unknown reason".to_string()
        } else {
            reason.kind.clone()
        }];
        facts.push(retry.to_string());
        if !reason.resolution_owner.trim().is_empty() {
            facts.push(format!("resolution owner: {}", reason.resolution_owner));
        }
        if reason.operator_action_required {
            facts.push("operator action required".to_string());
        }
        render_rail_line(&mut out, &facts.join(" · "), rail_code, false, style);
        if !reason.summary.trim().is_empty() && reason.summary.trim() != summary_line.trim() {
            render_rail_line(&mut out, &reason.summary, rail_code, false, style);
        }
        if !reason.remediation.trim().is_empty() {
            render_rail_line(&mut out, &reason.remediation, rail_code, false, style);
        }
        if !reason.disposition.trim().is_empty() {
            render_rail_line(&mut out, &reason.disposition, rail_code, false, style);
        }
    } else {
        let fallback = if summary_line.trim().is_empty() {
            format!("run is {}", lifecycle.state)
        } else {
            summary_line.to_string()
        };
        render_rail_line(&mut out, &fallback, rail_code, false, style);
    }
    out
}

fn render_rail_line(out: &mut String, text: &str, code: &str, heading: bool, style: MonitorStyle) {
    let rail = if style.color { "▌" } else { "!" };
    let prefix = format!("{rail} ");
    let width = style.width.saturating_sub(display_width(&prefix)).max(1);
    let code = if heading {
        code
    } else {
        code.strip_prefix("1;").unwrap_or(code)
    };
    for segment in wrap_display(text, width) {
        let line = format!("{prefix}{segment}");
        let _ = writeln!(out, "{}", style.paint(&line, code));
    }
}

fn resolve_attention_actions<'a>(feed: &'a StatusFeed, run_id: &str) -> Vec<&'a StatusAction> {
    let index = feed
        .actions
        .iter()
        .map(|action| (action.id.as_str(), action))
        .collect::<HashMap<_, _>>();
    let mut items = feed.attention_items.iter().collect::<Vec<_>>();
    items.sort_by(|left, right| {
        right
            .priority
            .cmp(&left.priority)
            .then_with(|| left.id.cmp(&right.id))
    });
    let mut seen = HashSet::new();
    let mut actions = Vec::new();
    for item in items {
        for action_id in &item.action_ids {
            let Some(action) = index.get(action_id.as_str()).copied() else {
                continue;
            };
            if !action_matches_run(action, run_id) || !seen.insert(action.id.as_str()) {
                continue;
            }
            actions.push(action);
        }
    }
    actions
}

fn resolve_terminal_actions<'a>(feed: &'a StatusFeed, run_id: &str) -> Vec<&'a StatusAction> {
    let mut actions = resolve_attention_actions(feed, run_id);
    if feed.lifecycle.successful {
        let mut seen = actions
            .iter()
            .map(|action| action.id.as_str())
            .collect::<HashSet<_>>();
        let mut handoffs = feed
            .actions
            .iter()
            .filter(|action| action_matches_run(action, run_id) && action.kind == "handoff")
            .filter(|action| seen.insert(action.id.as_str()))
            .collect::<Vec<_>>();
        handoffs.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.id.cmp(&right.id))
        });
        actions.extend(handoffs);
    }
    actions
}

fn action_matches_run(action: &StatusAction, run_id: &str) -> bool {
    action.run_id == run_id
}

fn render_actions(actions: &[&StatusAction], style: MonitorStyle) -> String {
    let mut out = String::new();
    render_section_heading(&mut out, "Actions", "exact daemon commands", None, style);
    for (index, action) in actions.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        if !action.label.trim().is_empty() && action.label.trim() != action.command.trim() {
            let _ = writeln!(out, "   {}", style.paint(action.label.trim(), "2"));
        }
        let _ = writeln!(out, "   {}", action.command);
    }
    out
}

fn render_commands(label: &str, commands: &[String], style: MonitorStyle) -> String {
    let mut out = String::new();
    render_section_heading(&mut out, label, "compatibility commands", None, style);
    for command in commands {
        let _ = writeln!(out, "   {command}");
    }
    out
}

fn render_evidence_links(links: &[String], style: MonitorStyle) -> String {
    let mut out = String::new();
    render_section_heading(&mut out, "Evidence", "", None, style);
    for link in links {
        render_role_line(&mut out, link, StatusFeedRole::Info, style);
    }
    out
}

fn section_for_active_checks(
    feed: &StatusFeed,
    mode: MonitorMode,
    style: MonitorStyle,
) -> MonitorSection {
    let mut phases = Vec::new();
    if feed.gate.active {
        phases.push(("gate", &feed.gate));
    }
    if feed.repair.active {
        phases.push(("repair", &feed.repair));
    }
    let meta = phases
        .iter()
        .map(|(label, phase)| format!("{label} {}", display_or_unknown(&phase.state)))
        .collect::<Vec<_>>()
        .join(" · ");
    let mut full = String::new();
    render_section_heading(&mut full, "Active checks", &meta, None, style);
    let mut line_count = 0usize;
    for (label, phase) in phases {
        render_phase(&mut full, label, phase, style);
        line_count += 1
            + usize::from(!phase.command.trim().is_empty())
            + phase.output_tail.lines().count()
            + usize::from(phase.finding_count > 0);
    }
    let mut collapsed = String::new();
    render_section_heading(
        &mut collapsed,
        "Active checks",
        &meta,
        Some(line_count.max(1)),
        style,
    );
    MonitorSection::collapsible(
        full,
        collapsed,
        collapse_rank(mode, StatusFeedBlockKind::Gate),
    )
}

fn render_phase(out: &mut String, label: &str, phase: &StatusPhaseProjection, style: MonitorStyle) {
    let summary = if phase.summary.trim().is_empty() {
        format!("{label} {}", display_or_unknown(&phase.state))
    } else {
        format!(
            "{label} {} · {}",
            display_or_unknown(&phase.state),
            phase.summary
        )
    };
    render_role_line(out, &summary, StatusFeedRole::Info, style);
    if !phase.command.trim().is_empty() {
        render_role_line(
            out,
            &format!("command {}", phase.command),
            StatusFeedRole::Dim,
            style,
        );
    }
    for tail in phase
        .output_tail
        .lines()
        .filter(|line| !line.trim().is_empty())
    {
        render_role_line(out, &format!("tail {tail}"), StatusFeedRole::Info, style);
    }
    if phase.finding_count > 0 {
        render_role_line(
            out,
            &format!("{} findings", phase.finding_count),
            StatusFeedRole::Warning,
            style,
        );
    }
}

fn evidence_section(
    block: &StatusFeedBlock,
    index: usize,
    mode: MonitorMode,
    rendered_commands: &HashSet<&str>,
    style: MonitorStyle,
) -> Option<(u8, usize, MonitorSection)> {
    if block.kind == StatusFeedBlockKind::Lifecycle {
        return None;
    }
    if mode == MonitorMode::Nominal
        && block.kind == StatusFeedBlockKind::Attention
        && !block.lines.iter().any(|line| {
            matches!(
                line.role,
                StatusFeedRole::Attention | StatusFeedRole::Warning | StatusFeedRole::Error
            )
        })
    {
        return None;
    }
    if block.kind == StatusFeedBlockKind::Commands
        && !block.lines.is_empty()
        && block
            .lines
            .iter()
            .all(|line| rendered_commands.contains(line.text.as_str()))
    {
        return None;
    }

    let full = if block.kind == StatusFeedBlockKind::Attention {
        render_attention_evidence(block, mode, style)
    } else if block.kind == StatusFeedBlockKind::Commands {
        render_command_block(block, style)
    } else {
        render_generic_block(block, style)
    };
    let mut collapsed = String::new();
    render_section_heading(
        &mut collapsed,
        &block.label,
        &block.meta,
        Some(block.lines.len()),
        style,
    );
    let section = if block.kind == StatusFeedBlockKind::Attention
        || block.kind == StatusFeedBlockKind::Commands
    {
        MonitorSection::fixed(full)
    } else {
        MonitorSection::collapsible(full, collapsed, collapse_rank(mode, block.kind))
    };
    Some((evidence_order(mode, block.kind), index, section))
}

fn render_generic_block(block: &StatusFeedBlock, style: MonitorStyle) -> String {
    let mut out = String::new();
    render_section_heading(&mut out, &block.label, &block.meta, None, style);
    if block.lines.is_empty() {
        render_role_line(&mut out, "-", StatusFeedRole::Dim, style);
    } else {
        for line in &block.lines {
            render_role_line(&mut out, &line.text, line.role, style);
        }
    }
    out
}

fn render_attention_evidence(
    block: &StatusFeedBlock,
    mode: MonitorMode,
    style: MonitorStyle,
) -> String {
    let mut out = String::new();
    let code = if mode == MonitorMode::Terminal {
        "1;31"
    } else {
        "1;33"
    };
    render_rail_line(&mut out, &block.label.to_uppercase(), code, true, style);
    for line in &block.lines {
        render_rail_line(&mut out, &line.text, code, false, style);
    }
    out
}

fn render_command_block(block: &StatusFeedBlock, style: MonitorStyle) -> String {
    let mut out = String::new();
    render_section_heading(&mut out, &block.label, &block.meta, None, style);
    for line in &block.lines {
        let _ = writeln!(out, "   {}", line.text);
    }
    out
}

fn render_role_line(out: &mut String, text: &str, role: StatusFeedRole, style: MonitorStyle) {
    let marker = role_marker(role, text);
    let prefix = if marker.is_empty() {
        " ".to_string()
    } else {
        format!("{marker} ")
    };
    let continuation = " ".repeat(display_width(&prefix));
    let width = style.width.saturating_sub(display_width(&prefix)).max(1);
    let code = monitor_role_code(role);
    let mut wrote = false;
    for source_line in text.lines() {
        for (position, segment) in wrap_display(source_line, width).into_iter().enumerate() {
            let prefix = if !wrote && position == 0 {
                &prefix
            } else {
                &continuation
            };
            let _ = writeln!(out, "{prefix}{}", style.paint(&segment, code));
            wrote = true;
        }
    }
    if !wrote {
        let _ = writeln!(out, "{prefix}");
    }
}

fn role_marker(role: StatusFeedRole, text: &str) -> &'static str {
    if starts_with_semantic_glyph(text) {
        return "";
    }
    match role {
        StatusFeedRole::Success => "✓",
        StatusFeedRole::Warning => "!",
        StatusFeedRole::Error => "✕",
        StatusFeedRole::Attention => "!",
        StatusFeedRole::Unknown => "?",
        StatusFeedRole::Heading | StatusFeedRole::Info | StatusFeedRole::Dim => "",
    }
}

fn starts_with_semantic_glyph(text: &str) -> bool {
    matches!(
        text.trim_start().chars().next(),
        Some('✓' | '✕' | '✗' | '!' | '?' | '●' | '○' | '◐' | '☒' | '☐' | '×' | '▌')
    )
}

fn render_section_heading(
    out: &mut String,
    label: &str,
    meta: &str,
    collapsed_lines: Option<usize>,
    style: MonitorStyle,
) {
    let label = if label.trim().is_empty() {
        "SECTION".to_string()
    } else {
        label.trim().to_uppercase()
    };
    let mut descriptor = label.clone();
    if !meta.trim().is_empty() {
        descriptor.push_str("  ");
        descriptor.push_str(meta.trim());
    }

    let ending = collapsed_lines.map(|count| {
        format!(
            "── {count} {} collapsed",
            if count == 1 { "line" } else { "lines" }
        )
    });
    let ending_width = ending.as_deref().map(display_width).unwrap_or(0);
    let descriptor_budget = if ending.is_some() {
        style.width.saturating_sub(ending_width + 1).max(1)
    } else {
        style.width.saturating_sub(4).max(1)
    };
    descriptor = truncate_display(&descriptor, descriptor_budget);

    out.push_str(&style.paint(&descriptor, "1"));
    if let Some(ending) = ending {
        out.push(' ');
        out.push_str(&style.paint(&ending, "2"));
    } else {
        let used = display_width(&descriptor);
        if used + 2 <= style.width {
            out.push(' ');
            let rule = "─".repeat(style.width.saturating_sub(used + 1));
            out.push_str(&style.paint(&rule, "2"));
        }
    }
    out.push('\n');
}

fn render_plain_wrapped(out: &mut String, text: &str, code: &str, style: MonitorStyle) {
    for source_line in text.lines() {
        for segment in wrap_display(source_line, style.width) {
            let _ = writeln!(out, "{}", style.paint(&segment, code));
        }
    }
}

fn plan_sections(sections: &mut [MonitorSection], style: MonitorStyle) -> MonitorLayout {
    let Some(budget) = body_row_budget(style) else {
        return MonitorLayout { compact: false };
    };
    if sections_rows(sections, style.width, false) <= budget {
        return MonitorLayout { compact: false };
    }

    // Remove decorative inter-section whitespace before hiding evidence.
    let compact = true;
    let mut candidates = sections
        .iter()
        .enumerate()
        .filter_map(|(index, section)| section.collapse_rank.map(|rank| (rank, index)))
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(rank, index)| (*rank, *index));

    for (_, index) in candidates {
        if sections_rows(sections, style.width, compact) <= budget {
            break;
        }
        sections[index].collapsed_selected = true;
    }
    MonitorLayout { compact }
}

fn sections_rows(sections: &[MonitorSection], width: usize, compact: bool) -> usize {
    let content = sections
        .iter()
        .map(|section| section.selected_rows(width))
        .sum::<usize>();
    if compact {
        content
    } else {
        content + sections.len().saturating_sub(1)
    }
}

fn join_sections(sections: &[MonitorSection], compact: bool) -> String {
    let mut out = String::new();
    for (index, section) in sections.iter().enumerate() {
        if index > 0 && !compact {
            out.push('\n');
        }
        out.push_str(section.selected().trim_end_matches('\n'));
        out.push('\n');
    }
    out
}

fn evidence_order(mode: MonitorMode, kind: StatusFeedBlockKind) -> u8 {
    match mode {
        MonitorMode::Nominal => match kind {
            StatusFeedBlockKind::WorkerActivity => 10,
            StatusFeedBlockKind::Gate | StatusFeedBlockKind::Repair => 20,
            StatusFeedBlockKind::Mission => 30,
            StatusFeedBlockKind::Economics => 40,
            StatusFeedBlockKind::Commands => 5,
            StatusFeedBlockKind::Attention => 6,
            StatusFeedBlockKind::Unknown | StatusFeedBlockKind::Lifecycle => 50,
        },
        MonitorMode::Attention => match kind {
            StatusFeedBlockKind::Attention => 5,
            StatusFeedBlockKind::Commands => 6,
            StatusFeedBlockKind::WorkerActivity => 10,
            StatusFeedBlockKind::Gate | StatusFeedBlockKind::Repair => 20,
            StatusFeedBlockKind::Mission => 30,
            StatusFeedBlockKind::Economics => 40,
            StatusFeedBlockKind::Unknown | StatusFeedBlockKind::Lifecycle => 50,
        },
        MonitorMode::Terminal => match kind {
            StatusFeedBlockKind::Attention => 5,
            StatusFeedBlockKind::Commands => 6,
            StatusFeedBlockKind::WorkerActivity => 10,
            StatusFeedBlockKind::Gate | StatusFeedBlockKind::Repair => 20,
            StatusFeedBlockKind::Economics => 30,
            StatusFeedBlockKind::Mission => 40,
            StatusFeedBlockKind::Unknown | StatusFeedBlockKind::Lifecycle => 50,
        },
    }
}

fn collapse_rank(mode: MonitorMode, kind: StatusFeedBlockKind) -> u8 {
    match mode {
        MonitorMode::Nominal => match kind {
            StatusFeedBlockKind::Mission => 10,
            StatusFeedBlockKind::Economics => 20,
            StatusFeedBlockKind::Unknown => 30,
            StatusFeedBlockKind::Gate | StatusFeedBlockKind::Repair => 40,
            StatusFeedBlockKind::WorkerActivity => 50,
            StatusFeedBlockKind::Lifecycle
            | StatusFeedBlockKind::Attention
            | StatusFeedBlockKind::Commands => 60,
        },
        MonitorMode::Attention => match kind {
            StatusFeedBlockKind::Mission => 10,
            StatusFeedBlockKind::Economics => 20,
            StatusFeedBlockKind::Gate | StatusFeedBlockKind::Repair => 30,
            StatusFeedBlockKind::WorkerActivity => 40,
            StatusFeedBlockKind::Unknown => 50,
            StatusFeedBlockKind::Lifecycle
            | StatusFeedBlockKind::Attention
            | StatusFeedBlockKind::Commands => 60,
        },
        MonitorMode::Terminal => match kind {
            StatusFeedBlockKind::Mission => 10,
            StatusFeedBlockKind::Gate | StatusFeedBlockKind::Repair => 20,
            StatusFeedBlockKind::Unknown => 30,
            StatusFeedBlockKind::Economics => 40,
            StatusFeedBlockKind::WorkerActivity => 50,
            StatusFeedBlockKind::Lifecycle
            | StatusFeedBlockKind::Attention
            | StatusFeedBlockKind::Commands => 60,
        },
    }
}

fn humanize_kind(kind: &str) -> String {
    let text = kind.trim().replace('_', " ");
    if text.is_empty() {
        "attention".to_string()
    } else {
        text
    }
}

fn display_or_unknown(value: &str) -> &str {
    if value.trim().is_empty() {
        "unknown"
    } else {
        value
    }
}

fn monitor_role_code(role: StatusFeedRole) -> &'static str {
    match role {
        StatusFeedRole::Heading => "1",
        StatusFeedRole::Info => "",
        StatusFeedRole::Dim => "2",
        StatusFeedRole::Success => "32",
        StatusFeedRole::Warning => "33",
        StatusFeedRole::Error => "31",
        StatusFeedRole::Attention => "1;33",
        StatusFeedRole::Unknown => "",
    }
}

fn body_row_budget(style: MonitorStyle) -> Option<usize> {
    style.rows.map(|rows| {
        let footer = live_footer(style);
        let footer_rows = if footer.is_empty() {
            0
        } else {
            physical_rows_for_line(&footer, style.width)
        };
        rows.saturating_sub(footer_rows + 1)
    })
}

fn live_footer(style: MonitorStyle) -> String {
    let Some(rows) = style.rows else {
        return MONITOR_FOOTER.to_string();
    };
    let available_rows = rows.saturating_sub(1);
    if available_rows == 0 {
        return String::new();
    }
    if physical_rows_for_line(MONITOR_FOOTER, style.width) <= available_rows {
        return MONITOR_FOOTER.to_string();
    }
    truncate_display("read-only", style.width)
}

fn physical_rows(text: &str, width: usize) -> usize {
    trimmed_logical_lines(text)
        .iter()
        .map(|line| physical_rows_for_line(line, width))
        .sum()
}

fn logical_lines_rows(lines: &[&str], width: usize) -> usize {
    lines
        .iter()
        .map(|line| physical_rows_for_line(line, width))
        .sum()
}

fn physical_rows_for_line(line: &str, width: usize) -> usize {
    let cells = ansi_display_width(line);
    cells.max(1).div_ceil(width.max(1))
}

fn trimmed_logical_lines(content: &str) -> Vec<&str> {
    let mut lines = content.lines().collect::<Vec<_>>();
    while lines.last().is_some_and(|line| ansi_text_is_blank(line)) {
        lines.pop();
    }
    lines
}

fn ansi_text_is_blank(text: &str) -> bool {
    strip_ansi(text).trim().is_empty()
}

fn ansi_display_width(text: &str) -> usize {
    display_width(&strip_ansi(text))
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            let _ = chars.next();
            for code in chars.by_ref() {
                if code.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn wrap_display(text: &str, max_width: usize) -> Vec<String> {
    let max_width = max_width.max(1);
    if display_width(text) <= max_width {
        return vec![text.to_string()];
    }
    let mut pieces = Vec::new();
    for word in text.split_whitespace() {
        if display_width(word) <= max_width {
            pieces.push(word.to_string());
        } else {
            pieces.extend(split_display_word(word, max_width));
        }
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for piece in pieces {
        let piece_width = display_width(&piece);
        if current_width > 0 && current_width + 1 + piece_width > max_width {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if current_width > 0 {
            current.push(' ');
            current_width += 1;
        }
        current.push_str(&piece);
        current_width += piece_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn split_display_word(word: &str, max_width: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut width = 0usize;
    for ch in word.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if !current.is_empty() && width + char_width > max_width {
            pieces.push(std::mem::take(&mut current));
            width = 0;
        }
        current.push(ch);
        width += char_width;
    }
    if !current.is_empty() {
        pieces.push(current);
    }
    pieces
}

fn truncate_display(value: &str, max_width: usize) -> String {
    if display_width(value) <= max_width {
        return value.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let target = max_width - 1;
    let mut width = 0usize;
    let mut truncated = String::new();
    for ch in value.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > target {
            break;
        }
        truncated.push(ch);
        width += char_width;
    }
    truncated.push('…');
    truncated
}

fn monitor_duration(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h{minutes:02}m{seconds:02}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

fn terminal_size() -> (Option<usize>, Option<usize>) {
    let mut ws = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    let env_dimension = |name: &str| {
        std::env::var(name)
            .ok()?
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|value| *value > 0)
    };
    let width = if ok && ws.ws_col > 0 {
        Some(ws.ws_col as usize)
    } else {
        env_dimension("COLUMNS")
    };
    let rows = if ok && ws.ws_row > 0 {
        Some(ws.ws_row as usize)
    } else {
        env_dimension("LINES")
    };
    (width, rows)
}

fn stdout_is_terminal() -> bool {
    unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        StatusAttentionItem, StatusFeedBlock, StatusFeedLine, StatusWorkerActivityProjection,
    };

    fn fixed_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-07-14T08:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    fn line(text: &str, role: StatusFeedRole) -> StatusFeedLine {
        StatusFeedLine {
            text: text.to_string(),
            role,
        }
    }

    fn block(
        kind: StatusFeedBlockKind,
        label: &str,
        meta: &str,
        lines: Vec<StatusFeedLine>,
    ) -> StatusFeedBlock {
        StatusFeedBlock {
            kind,
            label: label.to_string(),
            meta: meta.to_string(),
            lines,
        }
    }

    fn nominal_feed() -> StatusFeed {
        StatusFeed {
            feed_version: 2,
            summary_line: "implementing retry logic for slice-002".to_string(),
            lifecycle: StatusLifecycleProjection {
                state: "running".to_string(),
                terminal: false,
                successful: false,
                exit_code: None,
            },
            worker_activity: StatusWorkerActivityProjection {
                state: "running".to_string(),
                active: true,
                phase: "worker_running".to_string(),
                slice_id: "slice-002".to_string(),
                attempt: 1,
                launch_id: Some(7),
                command: String::new(),
                summary: "implementing retry logic for slice-002".to_string(),
                updated_at: Some(fixed_time() - chrono::Duration::seconds(8)),
            },
            gate: StatusPhaseProjection::default(),
            repair: StatusPhaseProjection::default(),
            terminal_reason: None,
            actions: Vec::new(),
            operator_commands: Vec::new(),
            attention_items: Vec::new(),
            attention: Vec::new(),
            blocks: vec![
                block(
                    StatusFeedBlockKind::Lifecycle,
                    "Run",
                    "● running • kd-test",
                    vec![line("legacy lifecycle evidence", StatusFeedRole::Info)],
                ),
                block(
                    StatusFeedBlockKind::WorkerActivity,
                    "Workers",
                    "(2 active / 5 total)",
                    vec![
                        line("◐ slice-002  running • attempt 1", StatusFeedRole::Info),
                        line("☒ slice-001  merged", StatusFeedRole::Success),
                        line("… 3 more", StatusFeedRole::Dim),
                    ],
                ),
                block(
                    StatusFeedBlockKind::Gate,
                    "Checks",
                    "verify rust-unit",
                    vec![
                        line("gate running", StatusFeedRole::Info),
                        line("unit verification done", StatusFeedRole::Success),
                    ],
                ),
                block(
                    StatusFeedBlockKind::Mission,
                    "Mission",
                    "envelope recorded",
                    vec![
                        line(
                            "goal reconcile the pre-release ledger",
                            StatusFeedRole::Info,
                        ),
                        line(
                            "budgets auto_promotions 0/2 • generated_slices 1/4",
                            StatusFeedRole::Info,
                        ),
                    ],
                ),
                block(
                    StatusFeedBlockKind::Economics,
                    "Economics",
                    "active",
                    vec![line("agents 2 • cmds 6 • cache 3/4", StatusFeedRole::Info)],
                ),
                block(
                    StatusFeedBlockKind::Attention,
                    "Attention",
                    "",
                    vec![line("no operator attention", StatusFeedRole::Dim)],
                ),
            ],
        }
    }

    fn attention_feed() -> StatusFeed {
        let mut feed = nominal_feed();
        feed.summary_line = "slice-002 blocked on operator question".to_string();
        feed.worker_activity.updated_at = Some(fixed_time() - chrono::Duration::seconds(252));
        let action = StatusAction {
            id: "answer-q-42".to_string(),
            kind: "answer_question".to_string(),
            label: "khazad-doom answer kd-test q-42 <answer>".to_string(),
            command: "khazad-doom answer kd-test q-42 <answer>".to_string(),
            priority: 100,
            run_id: "kd-test".to_string(),
            target_id: "q-42".to_string(),
        };
        feed.actions = vec![action];
        feed.operator_commands = vec!["khazad-doom answer kd-test q-42 <answer>".to_string()];
        feed.attention_items = vec![StatusAttentionItem {
            id: "question:q-42".to_string(),
            kind: "worker_question".to_string(),
            priority: 90,
            summary: "Deploy the preview environment now?".to_string(),
            action_ids: vec!["answer-q-42".to_string()],
        }];
        feed.attention = vec![line(
            "Question: Deploy the preview environment now?",
            StatusFeedRole::Attention,
        )];
        feed.blocks.retain(|block| {
            block.kind != StatusFeedBlockKind::Attention
                && block.kind != StatusFeedBlockKind::Commands
        });
        feed.blocks.push(block(
            StatusFeedBlockKind::Attention,
            "Attention",
            "",
            vec![
                line(
                    "Question: Deploy the preview environment now?",
                    StatusFeedRole::Attention,
                ),
                line("Option 1: Wait", StatusFeedRole::Attention),
                line("Option 2: Deploy", StatusFeedRole::Attention),
                line("Eligible timeout fallback: Wait", StatusFeedRole::Attention),
                line(
                    "Deadline: 2026-07-14T08:00:42Z (remaining 42s)",
                    StatusFeedRole::Attention,
                ),
            ],
        ));
        feed.blocks.push(block(
            StatusFeedBlockKind::Commands,
            "Commands",
            "",
            vec![line(
                "khazad-doom answer kd-test q-42 <answer>",
                StatusFeedRole::Attention,
            )],
        ));
        feed
    }

    fn terminal_feed() -> StatusFeed {
        let mut feed = nominal_feed();
        feed.summary_line = "integration gate failed twice; repair budget exhausted".to_string();
        feed.lifecycle = StatusLifecycleProjection {
            state: "failed".to_string(),
            terminal: true,
            successful: false,
            exit_code: Some(1),
        };
        feed.terminal_reason = Some(TerminalReason {
            kind: "gate_failure".to_string(),
            resolution_owner: "operator".to_string(),
            retryable: true,
            operator_action_required: true,
            summary: feed.summary_line.clone(),
            evidence_links: vec![".workflow/runs/kd-test/gate.log".to_string()],
            remediation: "fix the failing tests, then resume".to_string(),
            disposition: "repairs 2/2 used".to_string(),
            operator_commands: vec!["khazad-doom resume --run kd-test".to_string()],
        });
        feed.actions = vec![StatusAction {
            id: "resume-kd-test".to_string(),
            kind: "resume_run".to_string(),
            label: "khazad-doom resume --run kd-test".to_string(),
            command: "khazad-doom resume --run kd-test".to_string(),
            priority: 100,
            run_id: "kd-test".to_string(),
            target_id: String::new(),
        }];
        feed.attention_items = vec![StatusAttentionItem {
            id: "terminal-reason".to_string(),
            kind: "gate_failure".to_string(),
            priority: 100,
            summary: feed.summary_line.clone(),
            action_ids: vec!["resume-kd-test".to_string()],
        }];
        feed.blocks
            .retain(|block| block.kind != StatusFeedBlockKind::Attention);
        feed
    }

    #[test]
    fn nominal_uses_typed_glance_zone_and_generic_evidence() {
        let rendered = render_run(
            "kd-test",
            "/tmp/khazad-doom",
            &nominal_feed(),
            fixed_time(),
            MonitorStyle::fixed(96, None, false),
        );
        assert!(rendered.starts_with(" ● RUNNING"), "{rendered}");
        assert!(rendered.lines().next().unwrap().ends_with("updated 8s ago"));
        assert!(rendered.contains("implementing retry logic for slice-002"));
        assert!(rendered.contains("phase worker_running · slice slice-002 · attempt 1"));
        assert!(rendered.contains("WORKERS  (2 active / 5 total)"));
        assert!(!rendered.contains("legacy lifecycle evidence"));
        assert!(!rendered.contains("no operator attention"));
    }

    #[test]
    fn attention_joins_typed_actions_and_keeps_verbatim_evidence() {
        let rendered = render_run(
            "kd-test",
            "/tmp/khazad-doom",
            &attention_feed(),
            fixed_time(),
            MonitorStyle::fixed(80, None, false),
        );
        assert!(rendered.contains("! NEEDS YOU  1 item"), "{rendered}");
        assert!(rendered.contains("q-42 · worker question: Deploy the preview environment now?"));
        assert!(rendered.contains("   khazad-doom answer kd-test q-42 <answer>"));
        assert_eq!(
            rendered
                .lines()
                .filter(|line| *line == "   khazad-doom answer kd-test q-42 <answer>")
                .count(),
            1
        );
        assert!(rendered.contains("! Option 1: Wait"));
        assert!(rendered.contains("! Deadline: 2026-07-14T08:00:42Z (remaining 42s)"));
    }

    #[test]
    fn terminal_mode_wins_and_paints_reason_actions_and_evidence() {
        let rendered = render_run(
            "kd-test",
            "/tmp/khazad-doom",
            &terminal_feed(),
            fixed_time(),
            MonitorStyle::fixed(96, None, false),
        );
        assert!(rendered.starts_with(" ✕ FAILED"), "{rendered}");
        assert!(rendered.contains("! WHAT STOPPED IT"));
        assert!(rendered.contains("gate_failure · retryable · resolution owner: operator"));
        assert!(rendered.contains("fix the failing tests, then resume"));
        assert!(rendered.contains("   khazad-doom resume --run kd-test"));
        assert!(rendered.contains(".workflow/runs/kd-test/gate.log"));
        assert!(!rendered.contains("NEEDS YOU"));
        assert!(!rendered.contains("phase worker_running"));
    }

    #[test]
    fn attention_without_a_linked_action_uses_neutral_attention() {
        let mut feed = attention_feed();
        feed.attention_items[0].action_ids = vec!["missing-action".to_string()];
        feed.actions.clear();
        feed.blocks
            .retain(|block| block.kind != StatusFeedBlockKind::Commands);
        let rendered = render_run(
            "kd-test",
            "/tmp/repo",
            &feed,
            fixed_time(),
            MonitorStyle::fixed(80, None, false),
        );
        assert!(rendered.contains("! ATTENTION  1 item"), "{rendered}");
        assert!(!rendered.contains("NEEDS YOU"));
        assert!(rendered.contains("   khazad-doom answer kd-test q-42 <answer>"));
    }

    #[test]
    fn empty_and_cross_run_actions_cannot_make_attention_actionable() {
        for action_run_id in ["", "kd-other"] {
            let mut feed = attention_feed();
            feed.actions[0].run_id = action_run_id.to_string();
            feed.operator_commands.clear();
            feed.blocks
                .retain(|block| block.kind != StatusFeedBlockKind::Commands);
            let rendered = render_run(
                "kd-test",
                "/tmp/repo",
                &feed,
                fixed_time(),
                MonitorStyle::fixed(80, None, false),
            );
            assert!(rendered.contains("! ATTENTION  1 item"), "{rendered}");
            assert!(!rendered.contains("NEEDS YOU"));
            assert!(!rendered.contains("ACTIONS"));
            assert!(!rendered.contains("khazad-doom answer kd-test"));
            assert!(!rendered.contains("q-42 · worker question"));
        }
    }

    #[test]
    fn attention_target_comes_from_the_same_resolved_action_set() {
        let mut feed = attention_feed();
        feed.actions.insert(
            0,
            StatusAction {
                id: "cross-run".to_string(),
                kind: "answer_question".to_string(),
                label: "cross-run".to_string(),
                command: "khazad-doom answer kd-other q-other <answer>".to_string(),
                priority: 200,
                run_id: "kd-other".to_string(),
                target_id: "q-other".to_string(),
            },
        );
        feed.attention_items[0].action_ids =
            vec!["cross-run".to_string(), "answer-q-42".to_string()];
        let rendered = render_run(
            "kd-test",
            "/tmp/repo",
            &feed,
            fixed_time(),
            MonitorStyle::fixed(80, None, false),
        );
        assert!(rendered.contains("q-42 · worker question"), "{rendered}");
        assert!(!rendered.contains("q-other · worker question"));
        assert!(!rendered.contains("khazad-doom answer kd-other"));
    }

    #[test]
    fn legacy_attention_evidence_does_not_change_typed_mode() {
        let mut feed = nominal_feed();
        feed.blocks
            .retain(|block| block.kind != StatusFeedBlockKind::Attention);
        feed.blocks.push(block(
            StatusFeedBlockKind::Attention,
            "Attention",
            "legacy",
            vec![line("legacy operator warning", StatusFeedRole::Attention)],
        ));
        let rendered = render_run(
            "kd-test",
            "/tmp/repo",
            &feed,
            fixed_time(),
            MonitorStyle::fixed(80, None, false),
        );
        assert!(!rendered.contains("NEEDS YOU"));
        assert!(!rendered.contains("ATTENTION  1 item"));
        assert!(rendered.contains("phase worker_running"));
        assert!(rendered.contains("! legacy operator warning"));
    }

    #[test]
    fn successful_terminal_without_reason_uses_outcome_and_summary_fallback() {
        let mut feed = nominal_feed();
        feed.lifecycle = StatusLifecycleProjection {
            state: "completed".to_string(),
            terminal: true,
            successful: true,
            exit_code: Some(0),
        };
        feed.summary_line = "all requested slices completed".to_string();
        feed.terminal_reason = None;
        let rendered = render_run(
            "kd-test",
            "/tmp/repo",
            &feed,
            fixed_time(),
            MonitorStyle::fixed(110, None, false),
        );
        assert!(rendered.starts_with(" ✓ COMPLETED"), "{rendered}");
        assert!(rendered.contains("! OUTCOME"));
        assert!(rendered.contains("! all requested slices completed"));
        assert!(!rendered.contains("WHAT STOPPED IT"));
    }

    #[test]
    fn terminal_operator_commands_are_a_v2_compatibility_fallback() {
        let mut feed = terminal_feed();
        feed.actions.clear();
        feed.blocks
            .retain(|block| block.kind != StatusFeedBlockKind::Commands);
        let rendered = render_run(
            "kd-test",
            "/tmp/repo",
            &feed,
            fixed_time(),
            MonitorStyle::fixed(80, None, false),
        );
        assert!(rendered.contains("COMMANDS  compatibility commands"));
        assert!(rendered.contains("   khazad-doom resume --run kd-test"));
    }

    #[test]
    fn unlinked_terminal_action_uses_reason_command_as_compatibility_evidence() {
        let mut feed = terminal_feed();
        feed.attention_items.clear();
        feed.blocks
            .retain(|block| block.kind != StatusFeedBlockKind::Commands);
        let rendered = render_run(
            "kd-test",
            "/tmp/repo",
            &feed,
            fixed_time(),
            MonitorStyle::fixed(80, None, false),
        );
        assert!(!rendered.contains("ACTIONS  exact daemon commands"));
        assert!(rendered.contains("COMMANDS  compatibility commands"));
        assert!(rendered.contains("   khazad-doom resume --run kd-test"));
    }

    #[test]
    fn future_freshness_is_labeled_as_clock_skew() {
        let mut feed = nominal_feed();
        feed.worker_activity.updated_at = Some(fixed_time() + chrono::Duration::seconds(12));
        let rendered = render_run(
            "kd-test",
            "/tmp/repo",
            &feed,
            fixed_time(),
            MonitorStyle::fixed(110, None, false),
        );
        assert!(rendered.lines().next().unwrap().contains("clock skew +12s"));
        assert!(!rendered.contains("updated in"));
    }

    #[test]
    fn height_pressure_removes_spacing_before_collapsing_evidence() {
        let mut sections = vec![
            MonitorSection::fixed("header\n".to_string()),
            MonitorSection::collapsible(
                "evidence one\nevidence two\n".to_string(),
                "evidence summary\n".to_string(),
                10,
            ),
        ];
        let layout = plan_sections(&mut sections, MonitorStyle::fixed(80, Some(5), false));
        assert!(layout.compact);
        assert!(!sections[1].collapsed_selected);
    }

    #[test]
    fn short_attention_collapses_evidence_before_focus_and_keeps_footer() {
        let style = MonitorStyle::fixed(44, Some(18), false);
        let content = render_run(
            "kd-test",
            "/tmp/khazad-doom",
            &attention_feed(),
            fixed_time(),
            style,
        );
        assert!(content.contains("! NEEDS YOU"), "{content}");
        assert!(content.contains("khazad-doom answer kd-test q-42 <answer>"));
        assert!(content.contains("MISSION"), "{content}");
        assert!(content.contains("lines collapsed"));
        let frame = compose_live_frame(&content, style);
        assert!(frame.contains(MONITOR_FOOTER));
    }

    #[test]
    fn exact_command_is_one_logical_line_and_budget_counts_soft_wraps() {
        let mut feed = attention_feed();
        let command = "khazad-doom answer kd-test q-42 this-is-a-very-long-exact-answer-token";
        feed.actions[0].label = command.to_string();
        feed.actions[0].command = command.to_string();
        feed.operator_commands = vec![command.to_string()];
        feed.blocks
            .retain(|block| block.kind != StatusFeedBlockKind::Commands);
        let style = MonitorStyle::fixed(44, Some(24), false);
        let rendered = render_run("kd-test", "/tmp/repo", &feed, fixed_time(), style);
        assert!(rendered.lines().any(|line| line == format!("   {command}")));
        assert_eq!(physical_rows_for_line(&format!("   {command}"), 44), 2);
    }

    #[test]
    fn unknown_state_block_and_role_remain_paintable() {
        let mut feed = nominal_feed();
        feed.lifecycle.state = "future_paused".to_string();
        feed.blocks.push(block(
            StatusFeedBlockKind::Unknown,
            "Future",
            "v3",
            vec![line("future evidence", StatusFeedRole::Unknown)],
        ));
        let rendered = render_run(
            "kd-future",
            "/tmp/repo",
            &feed,
            fixed_time(),
            MonitorStyle::fixed(72, None, false),
        );
        assert!(rendered.contains("? FUTURE_PAUSED"));
        assert!(rendered.contains("FUTURE  v3"));
        assert!(rendered.contains("? future evidence"));
        assert!(!rendered.contains('\x1b'));
    }

    #[test]
    fn unicode_width_wrapping_and_truncation_use_terminal_cells() {
        assert_eq!(display_width("界a"), 3);
        assert_eq!(wrap_display("界界a", 4), vec!["界界", "a"]);
        assert_eq!(display_width(&truncate_display("界界a", 4)), 3);
    }

    #[test]
    fn color_uses_inverse_state_chip_and_attention_rail() {
        let rendered = render_run(
            "kd-test",
            "/tmp/repo",
            &attention_feed(),
            fixed_time(),
            MonitorStyle::fixed(80, None, true),
        );
        assert!(rendered.contains("\x1b[1;7;36m● RUNNING\x1b[0m"));
        assert!(rendered.contains("\x1b[1;33m▌ NEEDS YOU  1 item\x1b[0m"));
    }

    #[test]
    fn live_frame_uses_actual_subminimum_terminal_dimensions() {
        let content = (0..20).map(|i| format!("line-{i}\n")).collect::<String>();
        let tiny = MonitorStyle::fixed(10, Some(4), false);
        let frame = compose_live_frame(&content, tiny);
        assert!(frame.contains("read-only"));
        assert!(frame.matches("\x1b[K\n").count() <= 3, "{frame:?}");

        let one_row = compose_live_frame(&content, MonitorStyle::fixed(4, Some(1), false));
        assert_eq!(one_row.matches("\x1b[K\n").count(), 0);
        assert!(!one_row.contains("line-0"));
        assert!(!one_row.contains("read-only"));
    }

    #[test]
    fn static_frame_is_also_bounded_for_a_tiny_tty() {
        let content = (0..20).map(|i| format!("line-{i}\n")).collect::<String>();
        let style = MonitorStyle::fixed(10, Some(4), false);
        let frame = compose_static_frame(&content, style);
        let lines = trimmed_logical_lines(&frame);
        assert!(logical_lines_rows(&lines, style.width) <= 3, "{frame:?}");
        assert!(frame.contains("read-only"));
        assert!(frame.contains("more"));
        assert!(!frame.contains("\x1b["));

        let one_row = compose_static_frame(&content, MonitorStyle::fixed(4, Some(1), false));
        assert!(one_row.is_empty(), "{one_row:?}");
    }

    #[test]
    fn live_frame_reserves_wrapped_footer_and_never_tail_chops_without_marker() {
        let style = MonitorStyle::fixed(24, Some(10), false);
        let content = (0..20).map(|i| format!("line-{i}\n")).collect::<String>();
        let frame = compose_live_frame(&content, style);
        assert!(frame.starts_with("\x1b[?2026h\x1b[H"));
        assert!(frame.ends_with("\x1b[0J\x1b[?2026l"));
        assert!(frame.contains("… "));
        assert!(frame.contains(MONITOR_FOOTER));
        let footer_rows = physical_rows_for_line(MONITOR_FOOTER, 24);
        assert_eq!(footer_rows, 2);
    }
}
