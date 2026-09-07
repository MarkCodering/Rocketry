use super::*;

pub(super) struct Command {
    pub name: &'static str,
    pub description: &'static str,
}
pub(super) const COMMANDS: &[Command] = &[
    Command {
        name: "new",
        description: "Start a new mission",
    },
    Command {
        name: "agent",
        description: "Choose agent / model profile",
    },
    Command {
        name: "providers",
        description: "Environment credentials and models",
    },
    Command {
        name: "context",
        description: "Context budget and session usage",
    },
    Command {
        name: "memory",
        description: "Session notes and long-term memory",
    },
    Command {
        name: "tools",
        description: "Available tools and approval policies",
    },
    Command {
        name: "approve",
        description: "Review pending tool approval",
    },
    Command {
        name: "resume",
        description: "Resume the selected run",
    },
    Command {
        name: "cancel",
        description: "Cancel the active run",
    },
    Command {
        name: "inspect",
        description: "Toggle the run inspector",
    },
    Command {
        name: "expand",
        description: "Expand / collapse tool results",
    },
    Command {
        name: "search",
        description: "Search the flight log",
    },
    Command {
        name: "motion",
        description: "Toggle reduced motion",
    },
    Command {
        name: "help",
        description: "Show the flight manual",
    },
    Command {
        name: "quit",
        description: "Leave Mission Control",
    },
];
pub(super) fn suggestions(app: &App) -> Vec<&'static Command> {
    if app.focus != 1
        || app.slash_dismissed
        || !app.composer.starts_with('/')
        || app.composer.starts_with("//")
        || app.composer.chars().any(char::is_whitespace)
    {
        return vec![];
    }
    let query = &app.composer[1..];
    COMMANDS
        .iter()
        .filter(|c| c.name.starts_with(query))
        .collect()
}
pub(super) fn switch_agent(app: &mut App, name: &str, tx: &mpsc::Sender<Action>) {
    if app.active() || app.dispatching {
        app.notice = "Wait for this run or use /new before switching profiles".into();
        return;
    }
    if let Some(agent) = app.agents.get(name).cloned() {
        app.reset();
        app.agent = agent.name;
        app.demo = agent.provider == "demo";
        send(tx, Action::Refresh, app);
        app.notice = format!("Selected {} · new session", app.agent);
    } else {
        app.notice = format!("Unknown profile: {} · /agent lists profiles", safe(name));
    }
}
pub(super) fn execute_command(app: &mut App, input: &str, tx: &mpsc::Sender<Action>) -> bool {
    let input = input.trim().trim_start_matches('/');
    let (name, arg) = input
        .split_once(char::is_whitespace)
        .map(|(n, a)| (n, a.trim()))
        .unwrap_or((input, ""));
    if !arg.is_empty() && !matches!(name, "agent" | "model" | "search") {
        app.notice = format!("/{name} takes no arguments · /help lists commands");
        return false;
    }
    app.overlay_scroll = 0;
    match name {
        "new" => {
            if app.dispatching {
                app.notice = "Wait for mission dispatch to finish".into();
            } else {
                app.reset();
                send(tx, Action::Refresh, app);
                app.notice = "New mission · previous sessions are saved".into();
            }
        }
        "agent" | "model" => {
            if arg.is_empty() {
                app.overlay = Some(Overlay::Agents);
                app.menu = 0;
            } else {
                switch_agent(app, arg, tx);
            }
        }
        "providers" => app.overlay = Some(Overlay::Providers),
        "context" | "memory" | "tools" => {
            app.info_title = name.to_uppercase();
            app.info = vec![Line::from("Loading runtime state…")];
            app.overlay = Some(Overlay::Info);
            send(
                tx,
                Action::Inspect {
                    kind: name.into(),
                    agent: app.agent.clone(),
                    session: app.session_id.clone(),
                },
                app,
            );
        }
        "approve" => app.overlay = Some(Overlay::Approval),
        "resume" => {
            if let Some(run) = &app.selected {
                if matches!(
                    run.status,
                    RunStatus::Interrupted | RunStatus::Failed | RunStatus::Cancelled
                ) {
                    send(tx, Action::Resume(run.id.clone()), app);
                } else {
                    app.notice = "Select an interrupted, failed, or cancelled run to resume".into();
                }
            } else {
                app.notice = "Select a saved run first".into();
            }
        }
        "cancel" => {
            if app.active() {
                app.overlay = Some(Overlay::Cancel);
            } else {
                app.notice = "No active run selected".into();
            }
        }
        "inspect" => {
            app.inspector = !app.inspector;
            app.focus = if app.inspector { 2 } else { 1 };
        }
        "expand" => {
            let expand = app.cards.iter().any(|c| c.collapsed);
            for card in &mut app.cards {
                if card.label != "YOU" && card.label != "ROCKETRY" {
                    card.collapsed = !expand;
                }
            }
        }
        "search" => {
            app.search = arg.into();
            app.overlay = Some(Overlay::Search);
        }
        "motion" => {
            app.reduced_motion = !app.reduced_motion;
            app.notice = format!(
                "Reduced motion {}",
                if app.reduced_motion { "on" } else { "off" }
            );
        }
        "help" => app.overlay = Some(Overlay::Help),
        "quit" | "exit" => {
            if app.any_active() || app.dispatching {
                app.overlay = Some(Overlay::Quit);
            } else {
                return true;
            }
        }
        _ => {
            app.notice = format!(
                "Unknown command /{} · /help lists commands; // sends a literal slash",
                safe(name)
            )
        }
    }
    false
}
pub(super) fn inspection_lines(kind: &str, report: &Value) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::styled(
            format!(
                "{} · {}",
                report["agent"].as_str().unwrap_or(""),
                report["session"].as_str().unwrap_or("new session")
            ),
            Style::default().fg(CYAN),
        ),
        Line::from(""),
    ];
    match kind {
        "memory" => {
            for (scope, label) in [
                ("short_term", "SESSION WORKING NOTES"),
                ("long_term", "LONG-TERM AGENT MEMORY"),
            ] {
                lines.push(Line::styled(label, Style::default().fg(CYAN)));
                let entries = report["memory"][scope].as_array();
                if entries.is_none_or(Vec::is_empty) {
                    lines.push(Line::from("No saved entries (or read access disabled)."));
                }
                for entry in entries.into_iter().flatten() {
                    lines.push(Line::styled(
                        safe(entry["key"].as_str().unwrap_or("")),
                        Style::default().fg(FG).bold(),
                    ));
                    lines.extend(markdown(entry["value"].as_str().unwrap_or("")));
                    if entry["truncated"] == true {
                        lines.push(Line::from("[value preview truncated]"));
                    }
                    lines.push(Line::from(""));
                }
            }
            lines.push(Line::from(
                "Ask the agent to remember, update, or forget a fact.",
            ));
            lines.push(Line::from(
                "Writes follow tool approvals. Listing: 64 newest per scope.",
            ));
        }
        "tools" => {
            for tool in report["tools"].as_array().into_iter().flatten() {
                lines.push(Line::styled(
                    format!(
                        "{} · {} · {}",
                        tool["name"].as_str().unwrap_or(""),
                        tool["effect"].as_str().unwrap_or(""),
                        tool["policy"].as_str().unwrap_or("")
                    ),
                    Style::default().fg(CYAN),
                ));
                lines.push(Line::from(safe(tool["description"].as_str().unwrap_or(""))));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Filesystem and shell tools require --backend host or Docker.",
            ));
        }
        _ => {
            let c = &report["context"];
            for (label, value) in [
                ("Saved messages", &report["messages"]),
                ("Transcript bytes", &c["transcript_bytes"]),
                ("Prepared context bytes", &c["total_bytes"]),
                ("Context byte limit", &report["limit_bytes"]),
                ("Recalled memory bytes", &c["memory_bytes"]),
                ("Instructions / schemas / allowance", &c["overhead_bytes"]),
                ("Output token limit", &report["output_tokens"]),
                ("Compaction needed", &c["compacted"]),
            ] {
                lines.push(Line::from(format!("{label}: {value}")));
            }
            if let Some(error) = report["context_error"].as_str() {
                lines.push(Line::styled(safe(error), Style::default().fg(AMBER)));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Compaction is automatic; the full transcript stays on disk.",
            ));
            lines.push(Line::from(
                "Byte budgets are conservative, not model token counts.",
            ));
        }
    }
    lines
}
