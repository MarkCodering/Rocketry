//! Mission Control terminal UI. Rendering is pure; I/O runs in a separate task.
pub mod client;
mod commands;
use anyhow::Result;
use client::Client;
use commands::*;
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, KeyCode, KeyEvent,
        KeyModifiers,
    },
    execute,
};
use futures::StreamExt;
use ratatui::{prelude::*, widgets::*};
use rocketry_core::*;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    io,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
const BG: Color = Color::Rgb(15, 18, 23);
const PANEL: Color = Color::Rgb(20, 25, 32);
const FG: Color = Color::Rgb(218, 225, 235);
const MUTED: Color = Color::Rgb(122, 140, 159);
const CYAN: Color = Color::Rgb(98, 218, 234);
const AMBER: Color = Color::Rgb(244, 186, 98);
const GREEN: Color = Color::Rgb(130, 208, 166);
const BORDER: Color = Color::Rgb(45, 57, 70);
#[derive(Clone)]
pub struct Card {
    pub label: String,
    pub text: String,
    pub tone: Color,
    pub collapsed: bool,
}
#[derive(Clone, PartialEq)]
pub enum Overlay {
    Palette,
    Providers,
    Model,
    Info,
    Agents,
    Search,
    Cancel,
    Quit,
    Help,
    Approval,
}
#[derive(Default)]
struct CachedCard {
    fingerprint: u64,
    lines: Vec<Line<'static>>,
}
#[derive(Clone)]
pub struct ProviderStatus {
    pub name: String,
    pub model: String,
    pub status: String,
}
pub struct App {
    pub model: Option<String>,
    pub model_input: String,
    pub providers: Vec<ProviderStatus>,
    pub info_title: String,
    pub info: Vec<Line<'static>>,
    pub slash_menu: usize,
    pub slash_dismissed: bool,
    pub dispatching: bool,
    cache: std::cell::RefCell<Vec<CachedCard>>,
    pub agents: BTreeMap<String, Agent>,
    pub agent: String,
    pub sessions: Vec<Session>,
    pub runs: Vec<Run>,
    pub selected: Option<Run>,
    pub session_id: Option<String>,
    pub cards: Vec<Card>,
    pub composer: String,
    pub notice: String,
    pub focus: usize,
    pub selected_session: usize,
    pub overlay: Option<Overlay>,
    pub menu: usize,
    pub search: String,
    pub inspector: bool,
    pub reduced_motion: bool,
    pub true_color: bool,
    pub no_color: bool,
    pub overlay_scroll: u16,
    pub scroll: usize,
    pub approval: Option<Approval>,
    pub tool: Option<(ToolCall, Option<Value>)>,
    pub usage: Usage,
    pub connected: bool,
    pub demo: bool,
    pub frame: usize,
    pub elapsed_ms: u64,
    pub first_token_ms: Option<u64>,
    pub backend: String,
}
impl App {
    pub fn new(agent: String, backend: String) -> Self {
        Self {
            model: None,
            model_input: String::new(),
            providers: vec![],
            info_title: String::new(),
            info: vec![],
            slash_menu: 0,
            slash_dismissed: false,
            dispatching: false,
            cache: Default::default(),
            agents: BTreeMap::new(),
            agent,
            sessions: vec![],
            runs: vec![],
            selected: None,
            session_id: None,
            cards: vec![],
            composer: String::new(),
            notice: "Ready for your next mission".into(),
            focus: 1,
            selected_session: 0,
            overlay: None,
            menu: 0,
            search: String::new(),
            inspector: false,
            reduced_motion: std::env::var_os("NO_COLOR").is_some(),
            true_color: std::env::var("COLORTERM").is_ok_and(|s| s == "truecolor" || s == "24bit"),
            no_color: std::env::var_os("NO_COLOR").is_some(),
            overlay_scroll: 0,
            scroll: 0,
            approval: None,
            tool: None,
            usage: Usage::default(),
            connected: true,
            demo: false,
            frame: 0,
            elapsed_ms: 0,
            first_token_ms: None,
            backend,
        }
    }
    fn reset(&mut self) {
        self.selected = None;
        self.session_id = None;
        self.cards.clear();
        self.composer.clear();
        self.search.clear();
        self.slash_dismissed = false;
        self.slash_menu = 0;
        self.tool = None;
        self.approval = None;
        self.usage = Usage::default();
        self.scroll = 0;
        self.elapsed_ms = 0;
        self.first_token_ms = None;
        self.focus = 1;
    }
    fn any_active(&self) -> bool {
        self.active() || self.runs.iter().any(|r| !r.status.terminal())
    }
    fn active(&self) -> bool {
        self.selected.as_ref().is_some_and(|r| !r.status.terminal())
    }
    fn apply(&mut self, e: Event) {
        match e.kind {
            EventKind::Text(text) => {
                if self.first_token_ms.is_none() {
                    self.first_token_ms = Some(
                        self.selected
                            .as_ref()
                            .map(|r| e.timestamp.saturating_sub(r.created_at))
                            .unwrap_or(0),
                    );
                }
                if let Some(card) = self.cards.last_mut().filter(|c| c.label == "ROCKETRY") {
                    card.text.push_str(&text);
                } else {
                    self.cards.push(Card {
                        label: "ROCKETRY".into(),
                        text,
                        tone: CYAN,
                        collapsed: false,
                    });
                }
            }
            EventKind::Status(s) => {
                if let Some(r) = self.selected.as_mut() {
                    r.status = s.clone();
                }
                self.notice = format!("Run {}", status_name(&s));
            }
            EventKind::ToolStarted(c) => {
                self.tool = Some((c.clone(), None));
                self.cards.push(Card {
                    label: format!("TOOL · {}", c.name),
                    text: tool_details(&c),
                    tone: AMBER,
                    collapsed: true,
                });
            }
            EventKind::ToolFinished {
                call,
                output,
                error,
            } => {
                self.tool = Some((call.clone(), Some(output.clone())));
                self.cards.push(Card {
                    label: format!("{} · {}", if error { "ERROR" } else { "RESULT" }, call.name),
                    text: serde_json::to_string_pretty(&output).unwrap_or_default(),
                    tone: if error { AMBER } else { GREEN },
                    collapsed: true,
                });
            }
            EventKind::Approval(a) => {
                if a.decision.is_some() {
                    if self.approval.as_ref().is_some_and(|p| p.id == a.id) {
                        self.approval = None;
                    }
                } else {
                    self.approval = Some(a);
                    self.notice = "Approval required · press Ctrl+K to review".into();
                }
            }
            EventKind::Usage(u) => {
                self.usage.input_tokens = add(self.usage.input_tokens, u.input_tokens);
                self.usage.output_tokens = add(self.usage.output_tokens, u.output_tokens);
                self.usage.estimated_cost_usd =
                    match (self.usage.estimated_cost_usd, u.estimated_cost_usd) {
                        (None, Some(v)) => Some(v),
                        (Some(a), Some(b)) => Some(a + b),
                        _ => None,
                    };
            }
            EventKind::Error(e) => self.cards.push(Card {
                label: "RUN ERROR".into(),
                text: e,
                tone: AMBER,
                collapsed: false,
            }),
            EventKind::Child { id, name } => self.cards.push(Card {
                label: "DELEGATION".into(),
                text: format!("{name}  →  {id}"),
                tone: CYAN,
                collapsed: true,
            }),
            EventKind::Compacted { before, after } => {
                self.notice = format!("Context compacted: {before} → {after} bytes")
            }
            EventKind::ModelStarted => {}
        }
    }
}
fn add(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    b.map(|b| a.unwrap_or(0) + b)
}
fn status_name(s: &RunStatus) -> &'static str {
    match s {
        RunStatus::Queued => "queued",
        RunStatus::Running => "running",
        RunStatus::AwaitingApproval => "awaiting approval",
        RunStatus::NeedsReconciliation => "needs reconciliation",
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "cancelled",
        RunStatus::Interrupted => "interrupted",
    }
}
fn block(title: &str, focus: bool) -> Block<'_> {
    Block::bordered()
        .title(format!(" {title} "))
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(if focus { CYAN } else { BORDER }))
        .style(Style::default().bg(PANEL).fg(FG))
        .padding(Padding::horizontal(1))
}
fn safe(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .collect()
}
fn markdown(text: &str) -> Vec<Line<'static>> {
    let mut code = false;
    text.lines()
        .map(|raw| {
            let raw = safe(raw);
            if raw.starts_with("```") {
                code = !code;
                return Line::styled(
                    if code {
                        format!("  ┌─ {}", raw.trim_start_matches('`'))
                    } else {
                        "  └─".into()
                    },
                    Style::default().fg(MUTED),
                );
            }
            if code {
                return Line::styled(
                    format!("  {raw}"),
                    Style::default().fg(
                        if ["fn ", "pub ", "let ", "def ", "import "]
                            .iter()
                            .any(|s| raw.trim_start().starts_with(s))
                        {
                            CYAN
                        } else {
                            GREEN
                        },
                    ),
                );
            }
            if raw.starts_with('#') {
                return Line::styled(
                    raw.trim_start_matches('#').trim().to_string(),
                    Style::default().fg(FG).bold(),
                );
            }
            let mut spans = vec![];
            for (i, p) in raw.split("**").enumerate() {
                spans.push(Span::styled(
                    p.to_string(),
                    if i % 2 == 1 {
                        Style::default().fg(FG).bold()
                    } else {
                        Style::default().fg(FG)
                    },
                ));
            }
            Line::from(spans)
        })
        .collect()
}
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    frame.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), area);
    if area.width < 40 || area.height < 12 {
        frame.render_widget(
            Paragraph::new("Rocketry needs a terminal of at least 40×12.")
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let layout = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(5),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    let header = Layout::horizontal([
        Constraint::Min(25),
        Constraint::Length(38.min(area.width / 2)),
    ])
    .split(layout[0]);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("  ◈  R O C K E T R Y", Style::default().fg(CYAN).bold()),
            Span::styled("   /   MISSION CONTROL", Style::default().fg(MUTED)),
        ]))
        .block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(BORDER)),
        ),
        header[0],
    );
    let state = if app.demo {
        "DEMO · NO API CALLS"
    } else if app.connected {
        "CONNECTED"
    } else {
        "RECONNECTING"
    };
    frame.render_widget(
        Paragraph::new(format!("{state}  ·  {}  ", app.backend))
            .alignment(Alignment::Right)
            .style(Style::default().fg(if app.demo { AMBER } else { GREEN }))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(BORDER)),
            ),
        header[1],
    );
    if area.width >= 140 {
        let cols = Layout::horizontal([
            Constraint::Length(25),
            Constraint::Min(45),
            Constraint::Length(33),
        ])
        .spacing(1)
        .split(layout[1]);
        sidebar(frame, cols[0], app);
        conversation(frame, cols[1], app);
        inspector(frame, cols[2], app);
    } else if area.width >= 100 {
        let cols = Layout::horizontal([Constraint::Length(24), Constraint::Min(40)])
            .spacing(1)
            .split(layout[1]);
        sidebar(frame, cols[0], app);
        if app.inspector {
            inspector(frame, cols[1], app);
        } else {
            conversation(frame, cols[1], app);
        }
    } else {
        match app.focus {
            0 => sidebar(frame, layout[1], app),
            2 => inspector(frame, layout[1], app),
            _ => conversation(frame, layout[1], app),
        }
    }
    frame.render_widget(
        Paragraph::new(format!("  {}", safe(&app.notice)))
            .style(Style::default().fg(if app.approval.is_some() { AMBER } else { MUTED })),
        layout[2],
    );
    frame.render_widget(
        Paragraph::new("  / commands   ^K menu   Tab pane   ^N new   ^F find   ^Q exit")
            .style(Style::default().fg(MUTED).bg(PANEL)),
        layout[3],
    );
    if let Some(overlay) = &app.overlay {
        overlay_render(frame, app, overlay);
    }
    if app.no_color || !app.true_color {
        for cell in &mut frame.buffer_mut().content {
            if app.no_color {
                cell.fg = Color::Reset;
                cell.bg = Color::Reset;
            } else {
                cell.fg = indexed(cell.fg);
                cell.bg = indexed(cell.bg);
            }
        }
    }
}
fn indexed(color: Color) -> Color {
    let Color::Rgb(r, g, b) = color else {
        return color;
    };
    let levels = [0i32, 95, 135, 175, 215, 255];
    let nearest = |v: u8| {
        levels
            .iter()
            .enumerate()
            .min_by_key(|(_, n)| (**n - v as i32).abs())
            .unwrap()
            .0
    };
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let distance = (levels[ri] - r as i32).pow(2)
        + (levels[gi] - g as i32).pow(2)
        + (levels[bi] - b as i32).pow(2);
    let gray = ((r as i32 + g as i32 + b as i32) / 3 - 8).clamp(0, 230) / 10;
    let gv = 8 + gray * 10;
    if (gv - r as i32).pow(2) + (gv - g as i32).pow(2) + (gv - b as i32).pow(2) < distance {
        Color::Indexed((232 + gray) as u8)
    } else {
        Color::Indexed((16 + 36 * ri + 6 * gi + bi) as u8)
    }
}
fn sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let b = block("MISSIONS", app.focus == 0);
    let inner = b.inner(area);
    frame.render_widget(b, area);
    let mut lines = vec![
        Line::styled("WORKSPACE", Style::default().fg(MUTED)),
        Line::from(Span::styled(
            "＋ New mission  ^N",
            Style::default().fg(CYAN),
        )),
        Line::from(""),
    ];
    let start = app.selected_session.saturating_sub(8);
    for (i, s) in app.sessions.iter().enumerate().skip(start).take(10) {
        let selected = i == app.selected_session;
        lines.push(Line::styled(
            format!("{} {}", if selected { "▸" } else { " " }, safe(&s.title)),
            if selected {
                Style::default().bg(Color::Rgb(31, 53, 65)).fg(CYAN)
            } else {
                Style::default().fg(FG)
            },
        ));
        if let Some(r) = app.runs.iter().find(|r| r.session_id == s.id) {
            lines.push(Line::styled(
                format!("   {}", status_name(&r.status)),
                Style::default().fg(MUTED),
            ));
        }
    }
    if app.sessions.is_empty() {
        lines.push(Line::styled("No missions yet", Style::default().fg(MUTED)));
    }
    lines.push(Line::from(""));
    lines.push(Line::styled("AGENT ACTIVITY", Style::default().fg(MUTED)));
    for r in app
        .runs
        .iter()
        .filter(|r| !r.status.terminal() || r.parent_id.is_some())
        .take(8)
    {
        lines.push(Line::styled(
            format!(
                "{} {}",
                if r.parent_id.is_some() {
                    " └"
                } else {
                    " ◆"
                },
                r.agent.name
            ),
            Style::default().fg(CYAN),
        ));
        lines.push(Line::styled(
            format!("    {}", status_name(&r.status)),
            Style::default().fg(MUTED),
        ));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}
fn wrap_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for line in lines {
        let mut row = Vec::new();
        let mut used = 0;
        for span in line.spans {
            for token in span.content.split_inclusive(char::is_whitespace) {
                let token_width = unicode_width::UnicodeWidthStr::width(token);
                if used > 0 && used + token_width > width && token_width <= width {
                    out.push(Line::from(std::mem::take(&mut row)));
                    used = 0;
                }
                let mut chunk = String::new();
                for ch in token.chars() {
                    let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if used + w > width {
                        if !chunk.is_empty() {
                            row.push(Span::styled(std::mem::take(&mut chunk), span.style));
                        }
                        out.push(Line::from(std::mem::take(&mut row)));
                        used = 0;
                    }
                    chunk.push(ch);
                    used += w;
                }
                if !chunk.is_empty() {
                    row.push(Span::styled(chunk, span.style));
                }
            }
        }
        out.push(Line::from(row));
    }
    out
}

fn conversation(frame: &mut Frame, area: Rect, app: &App) {
    let parts = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length((app.composer.lines().count() as u16 + 3).clamp(4, 8)),
    ])
    .spacing(1)
    .split(area);
    let b = block(
        if app.selected.is_some() {
            "FLIGHT LOG"
        } else {
            "READY FOR LAUNCH"
        },
        false,
    );
    let inner = b.inner(parts[0]);
    frame.render_widget(b, parts[0]);
    if app.cards.is_empty() {
        let lines = vec![
            Line::from(""),
            Line::styled("       /\\", Style::default().fg(CYAN)),
            Line::styled(
                "      /  \\     YOUR IDEAS. IN MOTION.",
                Style::default().fg(CYAN).bold(),
            ),
            Line::styled("     | ◇  |", Style::default().fg(CYAN)),
            Line::styled(
                "    /|    |\\   A durable runtime for ambitious agents.",
                Style::default().fg(MUTED),
            ),
            Line::styled("   /_|____|_\\", Style::default().fg(CYAN)),
            Line::styled(
                "      /\\       Think · act · collaborate · deliver",
                Style::default().fg(AMBER),
            ),
            Line::from(""),
            Line::styled("  01  Describe a mission below", Style::default().fg(FG)),
            Line::styled(
                "  02  Follow the agent and inspect its tools",
                Style::default().fg(FG),
            ),
            Line::styled(
                "  03  Review actions, then resume with confidence",
                Style::default().fg(FG),
            ),
            Line::from(""),
            Line::styled(
                format!(
                    "  Active agent: {} · {}",
                    app.agent,
                    app.model
                        .as_deref()
                        .or_else(|| app
                            .agents
                            .get(&app.agent)
                            .and_then(|a| app.providers.iter().find(|p| p.name == a.provider))
                            .map(|p| p.model.as_str()))
                        .unwrap_or(if app.demo {
                            "demo"
                        } else {
                            "configured profile"
                        })
                ),
                Style::default().fg(CYAN),
            ),
            Line::styled(
                if app.demo {
                    "  Local demo selected · no credentials required"
                } else {
                    "  /agent choose a profile · /providers check credentials"
                },
                Style::default().fg(MUTED),
            ),
        ];
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    } else {
        use std::hash::{Hash, Hasher};
        let mut cache = app.cache.borrow_mut();
        cache.resize_with(app.cards.len(), CachedCard::default);
        let mut visible_cards = Vec::new();
        let mut total = 0;
        for (i, card) in app.cards.iter().enumerate() {
            if !app.search.is_empty()
                && !card
                    .text
                    .to_lowercase()
                    .contains(&app.search.to_lowercase())
            {
                continue;
            }
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (inner.width, &card.text, &card.label, card.collapsed).hash(&mut h);
            let fingerprint = h.finish();
            if cache[i].fingerprint != fingerprint {
                let mut lines = vec![Line::styled(
                    format!("{} {}", if card.collapsed { "▸" } else { "◆" }, card.label),
                    Style::default().fg(card.tone).bold(),
                )];
                if card.collapsed {
                    lines.push(Line::styled(
                        safe(
                            &card
                                .text
                                .lines()
                                .next()
                                .unwrap_or("")
                                .chars()
                                .take(120)
                                .collect::<String>(),
                        ),
                        Style::default().fg(MUTED),
                    ));
                } else {
                    lines.extend(markdown(&card.text));
                }
                lines.push(Line::from(""));
                cache[i] = CachedCard {
                    fingerprint,
                    lines: wrap_lines(lines, inner.width.max(1) as usize),
                };
            }
            total += cache[i].lines.len();
            visible_cards.push(i);
        }
        let end = total.saturating_sub(app.scroll);
        let start = end.saturating_sub(inner.height as usize);
        let mut offset = 0;
        let mut visible = Vec::new();
        for i in visible_cards {
            let lines = &cache[i].lines;
            let next = offset + lines.len();
            if next > start && offset < end {
                visible.extend(
                    lines
                        .iter()
                        .skip(start.saturating_sub(offset))
                        .take(end.saturating_sub(offset))
                        .cloned(),
                );
            }
            offset = next;
            if offset >= end {
                break;
            }
        }
        frame.render_widget(Paragraph::new(visible), inner);
    }

    let title = if app.active() {
        "COMPOSER · run active"
    } else {
        "COMPOSER · Enter send · / commands"
    };
    let editor = block(title, app.focus == 1);
    let input = editor.inner(parts[1]);
    frame.render_widget(editor, parts[1]);
    let display = if app.composer.is_empty() {
        "Describe a mission, or type / for commands…"
    } else {
        &app.composer
    };
    frame.render_widget(
        Paragraph::new(safe(display))
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(if app.composer.is_empty() { MUTED } else { FG })),
        input,
    );
    if app.focus == 1 && app.overlay.is_none() {
        let last = app.composer.lines().last().unwrap_or("");
        let x = unicode_width::UnicodeWidthStr::width(last)
            .min(input.width.saturating_sub(1) as usize) as u16;
        let y = app
            .composer
            .lines()
            .count()
            .saturating_sub(1)
            .min(input.height.saturating_sub(1) as usize) as u16;
        frame.set_cursor_position((input.x + x, input.y + y));
    }
    let matches = suggestions(app);
    if !matches.is_empty() && app.overlay.is_none() {
        let selected = app.slash_menu.min(matches.len() - 1);
        let start = selected.saturating_sub(4);
        let height = (matches.len().min(5) as u16 + 2).min(parts[0].height);
        let rect = Rect::new(
            area.x,
            parts[1].y.saturating_sub(height),
            area.width,
            height,
        );
        let lines: Vec<_> = matches
            .iter()
            .enumerate()
            .skip(start)
            .take(5)
            .map(|(i, c)| {
                Line::styled(
                    format!(
                        "{} /{:<10} {}",
                        if i == selected { "▸" } else { " " },
                        c.name,
                        c.description
                    ),
                    Style::default()
                        .fg(if i == selected { CYAN } else { FG })
                        .bg(if i == selected {
                            Color::Rgb(31, 53, 65)
                        } else {
                            PANEL
                        }),
                )
            })
            .collect();
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(lines).block(block("/ COMMANDS · ↑↓ select · Tab complete", true)),
            rect,
        );
    }
}
fn tool_details(call: &ToolCall) -> String {
    if call.name == "patch_file"
        && let (Some(path), Some(old), Some(new)) = (
            call.arguments["path"].as_str(),
            call.arguments["old"].as_str(),
            call.arguments["new"].as_str(),
        )
    {
        let mut diff = format!("```diff\n--- {path}\n+++ {path}\n@@ replacement @@\n");
        for line in old.lines() {
            diff.push_str(&format!("-{line}\n"));
        }
        for line in new.lines() {
            diff.push_str(&format!("+{line}\n"));
        }
        diff.push_str("```");
        return diff;
    }
    serde_json::to_string_pretty(&call.arguments).unwrap_or_default()
}

fn inspector(frame: &mut Frame, area: Rect, app: &App) {
    let b = block("TELEMETRY", app.focus == 2);
    let inner = b.inner(area);
    frame.render_widget(b, area);
    let status = app
        .selected
        .as_ref()
        .map(|r| status_name(&r.status))
        .unwrap_or("standby");
    let model = app
        .agents
        .get(&app.agent)
        .map(|a| a.provider.as_str())
        .unwrap_or("—");
    let mut lines = vec![
        Line::styled("EXECUTION", Style::default().fg(MUTED)),
        Line::from(format!("State    {status}")),
        Line::from(format!("Agent    {}", app.agent)),
        Line::from(format!("Profile  {model}")),
        Line::from(format!("Elapsed  {:.1}s", app.elapsed_ms as f64 / 1000.0)),
        Line::from(format!(
            "First Δ  {}",
            app.first_token_ms
                .map(|n| format!("{n} ms"))
                .unwrap_or("—".into())
        )),
        Line::from(""),
        Line::styled("MODEL USAGE", Style::default().fg(MUTED)),
        Line::from(format!(
            "Input    {}",
            app.usage
                .input_tokens
                .map(|n| n.to_string())
                .unwrap_or("unavailable".into())
        )),
        Line::from(format!(
            "Output   {}",
            app.usage
                .output_tokens
                .map(|n| n.to_string())
                .unwrap_or("unavailable".into())
        )),
        Line::from(format!(
            "Estimate {}",
            app.usage
                .estimated_cost_usd
                .map(|n| format!("${n:.5}"))
                .unwrap_or("unavailable".into())
        )),
        Line::from(""),
        Line::styled("LATEST TOOL", Style::default().fg(MUTED)),
    ];
    if let Some((call, output)) = &app.tool {
        lines.push(Line::styled(call.name.clone(), Style::default().fg(AMBER)));
        lines.extend(markdown(&tool_details(call)));
        if let Some(output) = output {
            lines.push(Line::from(""));
            lines.push(Line::styled("RESULT", Style::default().fg(GREEN)));
            lines.extend(markdown(
                &serde_json::to_string_pretty(output).unwrap_or_default(),
            ));
        }
    } else {
        lines.push(Line::styled(
            "Waiting for tool activity",
            Style::default().fg(MUTED),
        ));
    }
    if app.approval.is_some() {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            "! ACTION NEEDS APPROVAL",
            Style::default().fg(AMBER).bold(),
        ));
        lines.push(Line::from("Ctrl+K → Review approval"));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
fn overlay_render(frame: &mut Frame, app: &App, overlay: &Overlay) {
    let area = frame.area();
    let width = area.width.saturating_sub(4).min(76);
    let height = area.height.saturating_sub(4).min(22);
    let rect = Rect::new(
        (area.width - width) / 2,
        (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let (title, lines) = match overlay {
        Overlay::Palette => (
            "COMMAND PALETTE",
            COMMANDS
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    Line::styled(
                        format!(
                            "{} /{:<10} {}",
                            if i == app.menu { "▸" } else { " " },
                            s.name,
                            s.description
                        ),
                        Style::default().fg(if i == app.menu { CYAN } else { FG }),
                    )
                })
                .collect(),
        ),
        Overlay::Agents => (
            "AGENT / MODEL PROFILE",
            app.agents
                .values()
                .enumerate()
                .map(|(i, a)| {
                    Line::styled(
                        format!(
                            "{} {}  /  {}",
                            if i == app.menu { "▸" } else { " " },
                            a.name,
                            app.providers
                                .iter()
                                .find(|p| p.name == a.provider)
                                .map(|p| format!("{} · {}", a.provider, p.model))
                                .unwrap_or_else(|| a.provider.clone())
                        ),
                        Style::default().fg(if i == app.menu { CYAN } else { FG }),
                    )
                })
                .collect(),
        ),
        Overlay::Providers => {
            let mut lines = vec![
                Line::styled("CREDENTIAL DISCOVERY", Style::default().fg(CYAN)),
                Line::from(""),
            ];
            if app.backend == "remote" {
                lines.push(Line::from(
                    "Credentials are managed by the connected server.",
                ));
                lines.push(Line::from(
                    "Use /agent for server profiles; run doctor on the server.",
                ));
            } else {
                for p in &app.providers {
                    lines.push(Line::styled(
                        format!("{} · {}", safe(&p.name), safe(&p.model)),
                        Style::default().fg(CYAN),
                    ));
                    lines.push(Line::from(safe(&p.status)));
                    lines.push(Line::from(""));
                }
                lines.push(Line::from(
                    "Keys are never displayed. Detection does not validate access.",
                ));
                lines.push(Line::from(
                    "Set environment variables before launch, then restart.",
                ));
                lines.push(Line::from(
                    "/agent selects a profile · local Ollama needs no API key.",
                ));
            }
            ("PROVIDERS", lines)
        }
        Overlay::Model => (
            "CHANGE MODEL",
            vec![
                Line::styled(
                    format!("Agent: {}", safe(&app.agent)),
                    Style::default().fg(CYAN),
                ),
                Line::from("Enter a model ID supported by this provider."),
                Line::from(""),
                Line::styled(
                    format!("> {}▏", safe(&app.model_input)),
                    Style::default().fg(CYAN).bold(),
                ),
                Line::from(""),
                Line::from("Enter selects · blank restores profile default · Esc cancels"),
                Line::from("Changing models starts a new session. /agent changes provider."),
            ],
        ),
        Overlay::Info => (app.info_title.as_str(), app.info.clone()),
        Overlay::Search => (
            "SEARCH FLIGHT LOG",
            vec![
                Line::from(app.search.clone()),
                Line::from(""),
                Line::styled(
                    "Type to filter · Enter to apply · Esc to clear",
                    Style::default().fg(MUTED),
                ),
            ],
        ),
        Overlay::Cancel => (
            "CANCEL ACTIVE RUN?",
            vec![
                Line::from("Cancellation propagates to child agents."),
                Line::from("Uncertain external actions require reconciliation."),
                Line::from(""),
                Line::styled(
                    "Enter  Cancel run     Esc  Keep running",
                    Style::default().fg(AMBER),
                ),
            ],
        ),
        Overlay::Quit => (
            "LEAVE MISSION CONTROL?",
            vec![
                Line::from(if app.backend == "remote" {
                    "Remote runs continue after disconnecting."
                } else {
                    "Active local runs will be cancelled and saved."
                }),
                Line::from(""),
                Line::styled("Enter  Exit     Esc  Stay", Style::default().fg(AMBER)),
            ],
        ),
        Overlay::Help => (
            "FLIGHT MANUAL",
            vec![
                Line::from("Ctrl+K  Command palette     Ctrl+N  New mission"),
                Line::from("Tab     Switch pane         Ctrl+F  Search"),
                Line::from("Enter   Send / select       Alt+Enter  Newline"),
                Line::from("PgUp/Dn Scroll transcript   Home/End  Top / tail"),
                Line::from("Ctrl+C  Cancel controls     Ctrl+Q  Exit"),
                Line::from(""),
                Line::from("Policies are enforced by the runtime, including demo."),
                Line::from("Tool content is data; terminal controls are stripped."),
                Line::from("/ opens commands · ↑↓ select · Tab complete · Enter run"),
                Line::from("/agent [profile] changes provider · /model [id] changes model"),
                Line::from("/providers /memory /context /tools inspect runtime state"),
                Line::from("// sends a literal leading slash; unknown commands stay local"),
                Line::from("Esc closes this panel."),
            ],
        ),
        Overlay::Approval => {
            let mut lines = vec![Line::styled(
                "Review this exact action before allowing it.",
                Style::default().fg(AMBER),
            )];
            if let Some(a) = &app.approval {
                lines.push(Line::from(format!("Tool: {}", a.call.name)));
                lines.extend(markdown(
                    &serde_json::to_string_pretty(&a.call.arguments).unwrap_or_default(),
                ));
            } else {
                lines.push(Line::from("No pending approval."));
            }
            lines.push(Line::from(""));
            lines.push(Line::styled(
                "Y  Allow once     N  Deny     Esc  Close",
                Style::default().fg(AMBER),
            ));
            ("ACTION APPROVAL", lines)
        }
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(block(title, true).title_bottom(" PgUp/PgDn scroll · Esc close "))
            .scroll((
                if matches!(overlay, Overlay::Palette | Overlay::Agents) {
                    app.overlay_scroll
                        .max(app.menu.saturating_sub(height.saturating_sub(5) as usize) as u16)
                } else {
                    app.overlay_scroll
                },
                0,
            ))
            .wrap(Wrap { trim: false }),
        rect,
    );
}
enum Action {
    Start {
        model: Option<String>,
        agent: String,
        input: String,
        session: Option<String>,
    },
    Select(String),
    Approve(String, bool),
    Resume(String),
    Cancel(String),
    Refresh,
    Inspect {
        kind: String,
        agent: String,
        session: Option<String>,
    },
}
enum Update {
    Catalog(Vec<Session>, Vec<Run>),
    Selected(Box<Run>, Vec<Message>),
    Events(Vec<Event>),
    Error(String),
    Approved,
    Inspection {
        kind: String,
        agent: String,
        session: Option<String>,
        report: Value,
    },
}
async fn worker(
    client: Client,
    mut actions: mpsc::Receiver<Action>,
    updates: mpsc::Sender<Update>,
) {
    let mut selected: Option<String> = None;
    let mut cursor = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(150));
    let mut refresh = 0;
    let mut catalog = String::new();
    loop {
        let result:Result<()>=async {
            tokio::select! {
                action=actions.recv()=>{
                    let Some(action)=action else{return Err(anyhow::anyhow!("closed"));};
                    let run=match action {
                        Action::Start{agent,input,session,model}=>Some(client.start_with_model(&agent,&input,session,model).await?),
                        Action::Select(id)=>Some(client.run(&id).await?),
                        Action::Approve(id,allow)=>{client.approve(&id,allow).await?;updates.send(Update::Approved).await?;None},
                        Action::Resume(id)=>{client.resume(&id).await?;selected=Some(id);None},
                        Action::Cancel(id)=>{client.cancel(&id).await?;None},
                        Action::Refresh=>{selected=None;cursor=0;None},
                        Action::Inspect{kind,agent,session}=>{
                            let report=client.inspect(&agent,session.as_deref()).await?;
                            updates.send(Update::Inspection{kind,agent,session,report}).await?;None
                        }
                    };
                    if let Some(run)=run {
                        let messages=client.messages(&run.session_id).await?;
                        let prefix=messages.iter().rposition(|m|m.role=="user").map(|i|messages[..=i].to_vec()).unwrap_or_default();
                        selected=Some(run.id.clone());cursor=0;
                        updates.send(Update::Selected(Box::new(run),prefix)).await?;
                    }
                    refresh=0;
                },
                _=tick.tick()=>{
                    if let Some(id)=&selected {
                        let events=client.events(id,cursor).await?;
                        if let Some(last)=events.last(){cursor=last.sequence;}
                        if !events.is_empty(){updates.send(Update::Events(events)).await?;}
                    }
                }
            }
            if refresh==0 {
                let sessions=client.sessions().await?;let runs=client.runs().await?;
                let fingerprint=serde_json::to_string(&(&sessions,&runs))?;
                if fingerprint!=catalog {catalog=fingerprint;updates.send(Update::Catalog(sessions,runs)).await?;}
            }
            refresh=(refresh+1)%5;
            Ok(())
        }.await;
        if let Err(error) = result {
            if actions.is_closed() || updates.is_closed() {
                break;
            }
            let _ = updates.send(Update::Error(error.to_string())).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

fn send(tx: &mpsc::Sender<Action>, action: Action, app: &mut App) {
    if tx.try_send(action).is_err() {
        app.notice = "Control queue is busy; try again".into();
    }
}
fn key(app: &mut App, k: KeyEvent, tx: &mpsc::Sender<Action>) -> bool {
    if k.kind != event::KeyEventKind::Press {
        return false;
    }
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        match k.code {
            KeyCode::Char('k') => {
                app.overlay = Some(Overlay::Palette);
                app.menu = 0;
                return false;
            }
            KeyCode::Char('q') => {
                if app.any_active() || app.dispatching {
                    app.overlay = Some(Overlay::Quit);
                    return false;
                }
                return true;
            }
            KeyCode::Char('c') => {
                if app.active() {
                    app.overlay = Some(Overlay::Cancel);
                } else {
                    app.composer.clear();
                }
                return false;
            }
            KeyCode::Char('n') => return execute_command(app, "new", tx),
            KeyCode::Char('f') => {
                app.overlay = Some(Overlay::Search);
                return false;
            }
            _ => {}
        }
    }
    if let Some(overlay) = app.overlay.clone() {
        if k.code == KeyCode::PageDown {
            app.overlay_scroll = app.overlay_scroll.saturating_add(10);
            return false;
        }
        if k.code == KeyCode::PageUp {
            app.overlay_scroll = app.overlay_scroll.saturating_sub(10);
            return false;
        }
        if k.code == KeyCode::Esc {
            if overlay == Overlay::Search {
                app.search.clear();
            }
            app.overlay = None;
            app.overlay_scroll = 0;
            return false;
        }
        match overlay {
            Overlay::Model => match k.code {
                KeyCode::Char(c) if app.model_input.len() < 256 && !c.is_whitespace() => {
                    app.model_input.push(c)
                }
                KeyCode::Backspace => {
                    app.model_input.pop();
                }
                KeyCode::Enter => {
                    let model = app.model_input.trim().to_string();
                    app.overlay = None;
                    set_model(app, &model, tx);
                }
                _ => {}
            },
            Overlay::Search => match k.code {
                KeyCode::Char(c) => app.search.push(c),
                KeyCode::Backspace => {
                    app.search.pop();
                }
                KeyCode::Enter => app.overlay = None,
                _ => {}
            },
            Overlay::Cancel => {
                if k.code == KeyCode::Enter {
                    if let Some(r) = &app.selected {
                        send(tx, Action::Cancel(r.id.clone()), app);
                    }
                    app.overlay = None;
                }
            }
            Overlay::Quit => {
                if k.code == KeyCode::Enter {
                    return true;
                }
            }
            Overlay::Approval => {
                if let KeyCode::Char(c) = k.code
                    && ['y', 'n'].contains(&c)
                {
                    if let Some(a) = &app.approval {
                        send(tx, Action::Approve(a.id.clone(), c == 'y'), app);
                    }
                    app.overlay = None;
                }
            }
            Overlay::Palette | Overlay::Agents => {
                let len = if overlay == Overlay::Palette {
                    COMMANDS.len()
                } else {
                    app.agents.len()
                };
                match k.code {
                    KeyCode::Up => app.menu = app.menu.saturating_sub(1),
                    KeyCode::Down => app.menu = (app.menu + 1).min(len.saturating_sub(1)),
                    KeyCode::Enter => {
                        app.overlay = None;
                        if overlay == Overlay::Agents {
                            if let Some(name) = app.agents.keys().nth(app.menu).cloned() {
                                switch_agent(app, &name, tx);
                            }
                        } else if let Some(command) = COMMANDS.get(app.menu) {
                            return execute_command(app, command.name, tx);
                        }
                    }
                    _ => {}
                }
            }
            Overlay::Help | Overlay::Providers | Overlay::Info => {}
        }
        return false;
    }
    if app.focus == 1 && app.composer.starts_with('/') && !app.composer.starts_with("//") {
        let matches = suggestions(app);
        match k.code {
            KeyCode::Up if !matches.is_empty() => {
                app.slash_menu = app.slash_menu.saturating_sub(1);
                return false;
            }
            KeyCode::Down if !matches.is_empty() => {
                app.slash_menu = (app.slash_menu + 1).min(matches.len() - 1);
                return false;
            }
            KeyCode::Tab if !matches.is_empty() => {
                app.composer = format!("/{} ", matches[app.slash_menu.min(matches.len() - 1)].name);
                return false;
            }
            KeyCode::Esc => {
                app.slash_dismissed = true;
                return false;
            }
            KeyCode::Enter if !k.modifiers.contains(KeyModifiers::ALT) => {
                let input = if !matches.is_empty() {
                    format!("/{}", matches[app.slash_menu.min(matches.len() - 1)].name)
                } else {
                    app.composer.clone()
                };
                app.composer.clear();
                app.slash_menu = 0;
                app.slash_dismissed = false;
                return execute_command(app, &input, tx);
            }
            _ => {}
        }
    }
    match k.code {
        KeyCode::Tab => {
            app.focus = (app.focus + 1) % 3;
            app.inspector = app.focus == 2;
        }
        KeyCode::BackTab => {
            app.focus = (app.focus + 2) % 3;
            app.inspector = app.focus == 2;
        }
        KeyCode::PageUp => app.scroll = app.scroll.saturating_add(10),
        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(10),
        KeyCode::End => app.scroll = 0,
        KeyCode::Home => {
            app.scroll = app
                .cards
                .iter()
                .map(|c| c.text.lines().count() + 2)
                .sum::<usize>()
                .saturating_sub(10)
        }
        KeyCode::Up if app.focus == 0 => {
            app.selected_session = app.selected_session.saturating_sub(1)
        }
        KeyCode::Down if app.focus == 0 => {
            app.selected_session =
                (app.selected_session + 1).min(app.sessions.len().saturating_sub(1))
        }
        KeyCode::Enter if app.focus == 0 => {
            if let Some(s) = app.sessions.get(app.selected_session)
                && let Some(r) = app.runs.iter().find(|r| r.session_id == s.id)
            {
                send(tx, Action::Select(r.id.clone()), app);
                app.focus = 1;
            }
        }
        KeyCode::Enter if app.focus == 1 && k.modifiers.contains(KeyModifiers::ALT) => {
            app.composer.push('\n')
        }
        KeyCode::Enter if app.focus == 1 => {
            if !app.active() && !app.dispatching && !app.composer.trim().is_empty() {
                let input = app
                    .composer
                    .strip_prefix('/')
                    .filter(|_| app.composer.starts_with("//"))
                    .unwrap_or(&app.composer)
                    .to_owned();
                if tx
                    .try_send(Action::Start {
                        model: app.model.clone(),
                        agent: app.agent.clone(),
                        input,
                        session: app.session_id.clone(),
                    })
                    .is_ok()
                {
                    app.composer.clear();
                    app.notice = "Dispatching mission…".into();
                    app.dispatching = true;
                } else {
                    app.notice = "Control queue is busy; your draft is preserved".into();
                }
            }
        }
        KeyCode::Backspace if app.focus == 1 => {
            app.composer.pop();
            app.slash_menu = 0;
            app.slash_dismissed = false;
        }
        KeyCode::Char(c) if app.focus == 1 && app.composer.len() < 65536 => {
            app.composer.push(c);
            app.slash_menu = 0;
            app.slash_dismissed = false;
        }
        _ => {}
    }
    false
}
struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), DisableBracketedPaste);
        ratatui::restore();
    }
}
pub async fn run(client: Client, agent: String) -> Result<()> {
    run_with_providers(client, agent, vec![]).await
}
pub async fn run_with_providers(
    client: Client,
    agent: String,
    providers: Vec<ProviderStatus>,
) -> Result<()> {
    run_with_model(client, agent, providers, None).await
}
pub async fn run_with_model(
    client: Client,
    agent: String,
    providers: Vec<ProviderStatus>,
    model: Option<String>,
) -> Result<()> {
    let backend = if matches!(client, Client::Remote { .. }) {
        "remote"
    } else {
        "local"
    };
    let mut app = App::new(agent, backend.into());
    app.providers = providers;
    app.model = model;
    app.agents = client.agents().await?;
    if !app.agents.contains_key(&app.agent) {
        app.agent = app.agents.keys().next().cloned().unwrap_or_default();
    }
    app.demo = app
        .agents
        .get(&app.agent)
        .is_some_and(|a| a.provider == "demo");
    app.sessions = client.sessions().await?;
    app.runs = client.runs().await?;
    let (tx, rx) = mpsc::channel(32);
    let (updates, mut inbox) = mpsc::channel(64);
    let job = tokio::spawn(worker(client.clone(), rx, updates));
    let mut terminal = ratatui::init();
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnableBracketedPaste)?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        previous(info);
    }));
    let mut input = event::EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(34));
    let mut dirty = true;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let result:Result<()>=async{loop{tokio::select!{event=input.next()=>match event{Some(Ok(TermEvent::Key(k)))=>{if key(&mut app,k,&tx){break;}dirty=true;},Some(Ok(TermEvent::Paste(text)))=>{let room=65536usize.saturating_sub(app.composer.len());if app.overlay.is_none(){app.composer.extend(safe(&text).chars().take(room));app.slash_menu=0;app.slash_dismissed=false;}dirty=true;},Some(Ok(TermEvent::Resize(..)))=>dirty=true,Some(Err(e))=>return Err(e.into()),None=>break,_=>{}},update=inbox.recv()=>{match update{Some(Update::Catalog(s,r))=>{app.sessions=s;app.runs=r;app.connected=true;},Some(Update::Selected(r,messages))=>{app.dispatching=false;app.cards=messages.into_iter().filter(|m|!m.text.is_empty()).map(|m|Card{label:match m.role.as_str(){"user"=>"YOU","tool"=>"TOOL RESULT",_=>"ROCKETRY"}.into(),text:m.text,tone:if m.role=="user"{MUTED}else if m.role=="tool"{GREEN}else{CYAN},collapsed:m.role=="tool"}).collect();app.session_id=Some(r.session_id.clone());app.agent=r.agent.name.clone();app.model=r.model.clone();app.demo=r.agent.provider=="demo";app.selected=Some(*r);app.usage=Usage::default();app.first_token_ms=None;app.scroll=0;app.approval=None;},Some(Update::Events(events))=>{for e in events{if app.selected.as_ref().is_some_and(|r|r.id==e.run_id){app.apply(e);}}app.connected=true;},Some(Update::Approved)=>{app.approval=None;app.notice="Approval decision saved".into();},Some(Update::Error(e))=>{app.dispatching=false;if app.overlay==Some(Overlay::Info){app.info=vec![Line::from(safe(&e))];}app.notice=e;app.connected=false;},Some(Update::Inspection{kind,agent,session,report})=>{if app.agent==agent&&app.session_id==session&&app.info_title==kind.to_uppercase(){app.info=inspection_lines(&kind,&report);}},None=>break}dirty=true;},_=tick.tick()=>{if app.active(){app.elapsed_ms=app.selected.as_ref().map(|r|now().saturating_sub(r.created_at)).unwrap_or(0);app.frame+=1;if !app.reduced_motion&&app.frame.is_multiple_of(6){dirty=true;}}}}if dirty&&last_draw.elapsed()>=Duration::from_millis(33){terminal.draw(|frame|render(frame,&app))?;dirty=false;last_draw=Instant::now();}}Ok(())}.await;
    drop(tx);
    job.abort();
    client.shutdown().await;
    result
}
/// Deterministic showcase used by visual tests and `rocketry tui-preview`.
pub fn showcase() -> App {
    let mut app = App::new("navigator".into(), "local".into());
    app.demo = true;
    app.true_color = true;
    app.no_color = false;
    app.notice = "Visual fixture · sample layout for deterministic rendering tests".into();
    app.agents.insert(
        "navigator".into(),
        Agent {
            name: "navigator".into(),
            provider: "demo".into(),
            instructions: String::new(),
            tools: vec![],
            output_schema: None,
        },
    );
    app.sessions = vec![
        Session {
            id: "demo".into(),
            title: "Map the launch sequence".into(),
            created_at: 0,
        },
        Session {
            id: "previous".into(),
            title: "Review workspace architecture".into(),
            created_at: 0,
        },
    ];
    let run = Run {
        model: None,
        id: "demo-run".into(),
        session_id: "demo".into(),
        parent_id: None,
        agent: app.agents["navigator"].clone(),
        status: RunStatus::Completed,
        created_at: 0,
        error: None,
        workspace: ".".into(),
    };
    app.runs.push(run.clone());
    app.selected = Some(run);
    app.elapsed_ms = 2840;
    app.first_token_ms = Some(38);
    app.cards=vec![Card{label:"YOU".into(),text:"Inspect the workspace and explain how Rocketry executes a mission.".into(),tone:MUTED,collapsed:false},Card{label:"ROCKETRY".into(),text:"## Mission accepted\n\nI'll inspect the workspace, trace the execution loop, and bring back a concise report.\n\n**Execution plan**\n1. Inspect the workspace and registered tools\n2. Follow a model turn through durable execution\n3. Summarize the result and verification evidence".into(),tone:CYAN,collapsed:false},Card{label:"RESULT · list_dir".into(),text:"8 crates · core, providers, store, tools, runtime, server, tui, cli".into(),tone:GREEN,collapsed:true},Card{label:"ROCKETRY".into(),text:"## Ready for the next mission\n\nA single event-driven runtime powers the **Rust SDK**, **HTTP API**, and this **Mission Control** interface.\n\nTools run behind explicit permission and execution boundaries. Sessions persist across restarts, and uncertain actions are surfaced for review.\n\n```rust\nlet mut run = harness.start(\n    \"navigator\", \"Inspect this workspace\", None\n).await?;\nlet status = run.wait().await;\n```".into(),tone:CYAN,collapsed:false}];
    app.tool = Some((
        ToolCall {
            id: "call-01".into(),
            name: "list_dir".into(),
            arguments: serde_json::json!({"path":"."}),
        },
        Some(serde_json::json!({"entries":8,"status":"complete"})),
    ));
    app
}

pub fn preview_svg(width: u16, height: u16) -> Result<String> {
    let mut t = Terminal::new(ratatui::backend::TestBackend::new(width, height))?;
    t.draw(|f| render(f, &showcase()))?;
    let mut svg = format!(
        "<svg xmlns='http://www.w3.org/2000/svg' width='{}' height='{}'><rect width='100%' height='100%' fill='#0f1217'/><g font-family='Menlo,DejaVu Sans Mono,monospace' font-size='13'>",
        width as usize * 8 + 24,
        height as usize * 18 + 24
    );
    fn color(c: Color) -> String {
        match c {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            _ => "#dae1eb".into(),
        }
    }
    for y in 0..height {
        for x in 0..width {
            let cell = &t.backend().buffer()[(x, y)];
            let text = cell
                .symbol()
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;");
            if text != " " {
                svg.push_str(&format!(
                    "<text x='{}' y='{}' fill='{}'>{}</text>",
                    x as usize * 8 + 12,
                    y as usize * 18 + 26,
                    color(cell.fg),
                    text
                ));
            }
        }
    }
    svg.push_str("</g></svg>");
    Ok(svg)
}

/// Stable text projection of the actual terminal buffer for visual regression tests.
pub fn snapshot_text(width: u16, height: u16) -> Result<String> {
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, height))?;
    terminal.draw(|frame| render(frame, &showcase()))?;
    let mut text = String::new();
    for y in 0..height {
        let mut line = String::new();
        for x in 0..width {
            line.push_str(terminal.backend().buffer()[(x, y)].symbol());
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn layout_snapshots() {
        for (w, h, expected) in [
            (80, 24, include_str!("snapshots/80x24.txt")),
            (120, 40, include_str!("snapshots/120x40.txt")),
            (180, 50, include_str!("snapshots/180x50.txt")),
        ] {
            assert_eq!(snapshot_text(w, h).unwrap(), expected);
        }
    }
    #[test]
    fn layouts_and_unicode() {
        for (width, height) in [(80, 24), (120, 40), (180, 50)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            let mut app = showcase();
            app.composer = "火箭 🛰️ café".into();
            terminal.draw(|f| render(f, &app)).unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect::<String>();
            assert!(text.contains("R O C K E T R Y"));
            assert!(text.contains("COMPOSER"));
        }
    }
    #[test]
    fn controls_do_not_auto_approve() {
        let (tx, _) = mpsc::channel(2);
        let mut app = showcase();
        app.overlay = Some(Overlay::Approval);
        key(
            &mut app,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &tx,
        );
        assert_eq!(tx.capacity(), 2);
    }
    #[test]
    fn strips_terminal_escape() {
        assert!(!safe("\x1b]52;evil\x07").contains('\x1b'));
    }
}
