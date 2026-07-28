use std::io;
use std::time::Duration;
use ratatui::{
    backend::CrosstermBackend,
    Terminal,
    widgets::{Block, Borders, Paragraph, Table, Row, Cell, Gauge, List, ListItem},
    layout::{Layout, Constraint, Direction, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    Frame,
};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use anyhow::Result;

use crate::state::SharedState;
use crate::types::*;

/// TUI application state
struct App {
    state: SharedState,
    selected_node: usize,
    tab: usize,
    should_quit: bool,
}

impl App {
    fn new(state: SharedState) -> Self {
        Self {
            state,
            selected_node: 0,
            tab: 0,
            should_quit: false,
        }
    }

    async fn tick(&self) {}

    fn on_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Tab => self.tab = (self.tab + 1) % 3,
            KeyCode::BackTab => self.tab = if self.tab == 0 { 2 } else { self.tab - 1 },
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected_node = self.selected_node.saturating_add(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected_node = self.selected_node.saturating_sub(1);
            }
            _ => {}
        }
    }
}

pub async fn run_tui(state: SharedState, refresh_ms: u64) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(state);
    let tick_rate = Duration::from_millis(refresh_ms);

    loop {
        // Gather owned snapshot data asynchronously before the synchronous draw closure.
        // terminal.draw takes a sync closure, so no async work can happen inside it.
        let (snapshots, active_alerts): (Vec<NodeSnapshot>, Vec<Alert>) = {
            let s = app.state.read().await;
            let snaps = s.node_snapshots();
            let alerts = s.active_alerts().into_iter().cloned().collect();
            (snaps, alerts)
        };

        terminal.draw(|f| render(f, &app, &snapshots, &active_alerts))?;

        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.on_key(key.code);
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    // Restore terminal
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

fn render(f: &mut Frame, app: &App, snapshots: &[NodeSnapshot], active_alerts: &[Alert]) {
    let size = f.area();

    // Main layout: title bar + content + status bar
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Title + tabs
            Constraint::Min(0),     // Content
            Constraint::Length(1),  // Status bar
        ])
        .split(size);

    render_title(f, chunks[0]);

    match app.tab {
        0 => render_overview(f, chunks[1], snapshots, app.selected_node),
        1 => render_node_detail(f, chunks[1], snapshots, app.selected_node),
        2 => render_alerts(f, chunks[1], active_alerts),
        _ => {}
    }

    render_status_bar(f, chunks[2], app.tab);
}

fn render_title(f: &mut Frame, area: Rect) {
    let tabs = vec!["[1] Overview", "[2] Node Detail", "[3] Alerts"];
    let tab_line: Vec<Span> = tabs.iter().enumerate().flat_map(|(i, t)| {
        let style = if i == 0 {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        vec![Span::styled(*t, style), Span::raw("  ")]
    }).collect();

    let block = Block::default()
        .title(Span::styled(
            " os-watcher — Decentralized Host Monitor ",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(Line::from(tab_line)).block(block);
    f.render_widget(para, area);
}

fn render_overview(f: &mut Frame, area: Rect, snapshots: &[NodeSnapshot], selected: usize) {
    let rows: Vec<Row> = snapshots.iter().enumerate().map(|(i, snap)| {
        let status_sym = match snap.info.status {
            NodeStatus::Online => Span::styled("●", Style::default().fg(Color::Green)),
            NodeStatus::Offline => Span::styled("●", Style::default().fg(Color::Red)),
            NodeStatus::Unknown => Span::styled("●", Style::default().fg(Color::Yellow)),
        };

        let (cpu, mem, disk, uptime) = if let Some(m) = &snap.metrics {
            let max_disk = m.disks.iter()
                .map(|d| d.usage_percent)
                .fold(0.0f32, f32::max);
            let uptime_str = format_duration(m.uptime_seconds);
            (
                format!("{:.1}%", m.cpu.usage_percent),
                format!("{:.1}%", m.memory.usage_percent),
                format!("{:.1}%", max_disk),
                uptime_str,
            )
        } else {
            ("N/A".to_string(), "N/A".to_string(), "N/A".to_string(), "N/A".to_string())
        };

        let style = if i == selected {
            Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        Row::new(vec![
            Cell::from(status_sym),
            Cell::from(snap.info.hostname.clone()),
            Cell::from(snap.info.api_addr.clone()),
            Cell::from(cpu),
            Cell::from(mem),
            Cell::from(disk),
            Cell::from(uptime),
        ]).style(style)
    }).collect();

    let table = Table::new(
        rows,
        [
            Constraint::Length(3),
            Constraint::Min(15),
            Constraint::Min(20),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(12),
        ],
    )
    .header(
        Row::new(vec!["", "Hostname", "API Address", "CPU", "Memory", "Disk", "Uptime"])
            .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
    )
    .block(
        Block::default()
            .title(" Nodes ")
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan)),
    );

    f.render_widget(table, area);
}

fn render_node_detail(f: &mut Frame, area: Rect, snapshots: &[NodeSnapshot], selected: usize) {
    let Some(snap) = snapshots.get(selected) else {
        f.render_widget(
            Paragraph::new("No node selected").block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    let Some(metrics) = &snap.metrics else {
        f.render_widget(
            Paragraph::new(format!("No metrics for {}", snap.info.hostname))
                .block(Block::default().borders(Borders::ALL)),
            area,
        );
        return;
    };

    // Layout: top row (cpu, mem) + bottom row (disks, processes)
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(0)])
        .split(area);

    let top_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);

    // CPU gauge
    let cpu_pct = metrics.cpu.usage_percent as u16;
    let cpu_color = gauge_color(metrics.cpu.usage_percent);
    let cpu_gauge = Gauge::default()
        .block(Block::default().title(format!(" CPU ({} cores) ", metrics.cpu.core_count)).borders(Borders::ALL))
        .gauge_style(Style::default().fg(cpu_color))
        .percent(cpu_pct.min(100))
        .label(format!("{:.1}%", metrics.cpu.usage_percent));
    f.render_widget(cpu_gauge, top_cols[0]);

    // Memory gauge
    let mem_pct = metrics.memory.usage_percent as u16;
    let mem_color = gauge_color(metrics.memory.usage_percent);
    let mem_used_gb = metrics.memory.used_bytes as f64 / 1_073_741_824.0;
    let mem_total_gb = metrics.memory.total_bytes as f64 / 1_073_741_824.0;
    let mem_gauge = Gauge::default()
        .block(Block::default().title(" Memory ").borders(Borders::ALL))
        .gauge_style(Style::default().fg(mem_color))
        .percent(mem_pct.min(100))
        .label(format!("{:.1}/{:.1} GB ({:.1}%)", mem_used_gb, mem_total_gb, metrics.memory.usage_percent));
    f.render_widget(mem_gauge, top_cols[1]);

    // Bottom: disks + processes
    let bottom_cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(rows[1]);

    // Disk list
    let disk_items: Vec<ListItem> = metrics.disks.iter().map(|d| {
        let color = gauge_color(d.usage_percent);
        let used_gb = d.used_bytes as f64 / 1_073_741_824.0;
        let total_gb = d.total_bytes as f64 / 1_073_741_824.0;
        ListItem::new(Line::from(vec![
            Span::styled(
                format!("{:15}", truncate(&d.mount_point, 15)),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                format!(" {:.1}/{:.1}GB", used_gb, total_gb),
                Style::default().fg(color),
            ),
            Span::styled(
                format!(" ({:.0}%)", d.usage_percent),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]))
    }).collect();

    let disk_list = List::new(disk_items)
        .block(Block::default().title(" Disks ").borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(disk_list, bottom_cols[0]);

    // Process list
    let proc_rows: Vec<Row> = metrics.top_processes.iter().map(|p| {
        let cpu_color = if p.cpu_usage > 50.0 { Color::Red } else if p.cpu_usage > 20.0 { Color::Yellow } else { Color::Green };
        Row::new(vec![
            Cell::from(p.pid.to_string()),
            Cell::from(truncate(&p.name, 20)),
            Cell::from(Span::styled(format!("{:.1}%", p.cpu_usage), Style::default().fg(cpu_color))),
            Cell::from(format_bytes(p.memory_bytes)),
        ])
    }).collect();

    let proc_table = Table::new(
        proc_rows,
        [Constraint::Length(7), Constraint::Min(15), Constraint::Length(8), Constraint::Length(10)],
    )
    .header(
        Row::new(vec!["PID", "Name", "CPU", "Memory"])
            .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
    )
    .block(Block::default().title(" Top Processes ").borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(proc_table, bottom_cols[1]);
}

fn render_alerts(f: &mut Frame, area: Rect, alerts: &[Alert]) {
    let items: Vec<ListItem> = if alerts.is_empty() {
        vec![ListItem::new(Span::styled("No active alerts", Style::default().fg(Color::Green)))]
    } else {
        alerts.iter().map(|a| {
            let color = match a.severity {
                AlertSeverity::Critical => Color::Red,
                AlertSeverity::Warning => Color::Yellow,
                AlertSeverity::Info => Color::Cyan,
            };
            let severity_str = match a.severity {
                AlertSeverity::Critical => "CRITICAL",
                AlertSeverity::Warning => "WARNING ",
                AlertSeverity::Info => "INFO    ",
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("[{}] ", severity_str), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                Span::styled(&a.rule_name, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                Span::raw(format!(" — {}", a.message)),
                Span::styled(
                    format!("  ({})", a.triggered_at.format("%H:%M:%S")),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        }).collect()
    };

    let list = List::new(items)
        .block(Block::default().title(" Active Alerts ").borders(Borders::ALL).border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(list, area);
}

fn render_status_bar(f: &mut Frame, area: Rect, tab: usize) {
    let help = match tab {
        0 => " ↑/↓: Select node  Tab: Next view  q: Quit",
        1 => " ↑/↓: Change node  Tab: Next view  q: Quit",
        _ => " Tab: Next view  q: Quit",
    };
    let bar = Paragraph::new(help)
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(bar, area);
}

// Helpers

fn gauge_color(pct: f32) -> Color {
    if pct >= 90.0 { Color::Red }
    else if pct >= 70.0 { Color::Yellow }
    else { Color::Green }
}

fn format_duration(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let minutes = (secs % 3600) / 60;
    if days > 0 {
        format!("{}d {}h", days, hours)
    } else if hours > 0 {
        format!("{}h {}m", hours, minutes)
    } else {
        format!("{}m", minutes)
    }
}

fn format_bytes(bytes: u64) -> String {
    const MB: u64 = 1_048_576;
    const GB: u64 = 1_073_741_824;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{} KB", bytes / 1024)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max.saturating_sub(1)])
    }
}
