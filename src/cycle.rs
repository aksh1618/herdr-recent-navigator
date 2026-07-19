//! Alt-tab style MRU pane cycling.
//!
//! Terminals cannot observe modifier release, so "keep walking deeper vs.
//! start a new cycle" is decided by a timeout-based session: `cycle`
//! invocations within `cycle_timeout_ms` of the previous press continue the
//! same session over a frozen snapshot of the MRU order; after the timeout
//! the next press starts fresh from the live MRU state.
//!
//! Press semantics (mirroring GUI alt-tab, which commits on modifier
//! release — the timeout stands in for the release):
//! 1. First press: instantly focus the MRU-previous pane, no popup.
//! 2. Second press within the window: open the navigator popup in cycle
//!    mode with the selection advanced one step. Focus does NOT move.
//! 3. Further presses (or Tab/arrows inside the popup) move the highlight.
//! 4. Timeout expiry — or Enter — focuses the highlighted pane and closes
//!    the popup. Esc cancels back to the pane the cycle started from.
//!
//! While a session is active, `track` events are absorbed into the session
//! instead of `mru.json` so panes that are merely hopped *through* never
//! pollute recency order. When the session ends, only the pane the cycle
//! landed on — plus any panes the user focused by other means during the
//! window — are reconciled into the MRU store, in chronological order.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::stdout;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use fs2::FileExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use serde::{Deserialize, Serialize};

use crate::ipc::{self, FocusedPaneInfo};
use crate::models::NavigationNode;
use crate::tracker::{self, MruKind};

const DEFAULT_TIMEOUT_MS: u64 = 800;

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct CyclePane {
    pub pane_id: String,
    pub workspace_id: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CycleSession {
    pub started_at: u64,
    pub last_press_at: u64,
    /// Frozen cycle order; index 0 is the pane focused at session start.
    pub order: Vec<CyclePane>,
    /// Index in `order` of the pane the session currently sits on.
    pub position: usize,
    /// Pane the cycle last landed on (echo suppression + reconcile record).
    #[serde(default)]
    pub landed: Option<CyclePane>,
    /// Panes the user focused by other means during the session window.
    #[serde(default)]
    pub post_focus: Vec<CyclePane>,
    /// Whether the cycle popup is showing (presses move the highlight
    /// instead of focusing panes).
    #[serde(default)]
    pub popup_open: bool,
    /// The popup's own pane id, excluded from post_focus absorption.
    #[serde(default)]
    pub popup_pane_id: Option<String>,
}

/// Outcome of consulting the cycle session for an incoming track event.
#[derive(Debug, PartialEq, Eq)]
pub enum TrackDisposition {
    /// Event belongs to an active cycle session; do not record to MRU.
    Absorbed,
    /// No active session (any stale one has been reconciled); record normally.
    Proceed,
}

fn session_path() -> PathBuf {
    tracker::state_dir_or_default().join("cycle-session.json")
}

fn lock_path() -> PathBuf {
    tracker::state_dir_or_default().join("cycle.lock")
}

/// Exclusive lock guarding the session file. Never held across calls that
/// take the MRU lock in the *other* order (tracker never takes this lock),
/// so lock ordering is consistent: cycle.lock → mru.lock.
fn acquire_lock() -> Result<File> {
    let path = lock_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("Failed to open cycle lock file: {}", path.display()))?;
    file.lock_exclusive()?;
    Ok(file)
}

fn load_session() -> Option<CycleSession> {
    let content = fs::read_to_string(session_path()).ok()?;
    serde_json::from_str(&content).ok()
}

fn save_session(s: &CycleSession) -> Result<()> {
    let path = session_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(s)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn delete_session() {
    let _ = fs::remove_file(session_path());
}

pub fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis() as u64
}

/// Parse the plugin's own manifest (`herdr-plugin.toml` under
/// `HERDR_PLUGIN_ROOT`). Mirrors the manifest `theme` fallback in `main.rs`.
fn manifest_value() -> Option<toml::Value> {
    let root = std::env::var("HERDR_PLUGIN_ROOT").ok()?;
    let content = fs::read_to_string(PathBuf::from(root).join("herdr-plugin.toml")).ok()?;
    content.parse::<toml::Value>().ok()
}

/// Read `cycle_timeout_ms` from the plugin manifest, falling back to the
/// default.
pub fn timeout_ms() -> u64 {
    manifest_value()
        .and_then(|v| v.get("cycle_timeout_ms")?.as_integer())
        .map(|n| n.max(0) as u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
}

/// Whether the popup opens on the FIRST press (every subsequent press is a
/// bare Tab inside the popup) instead of after an instant headless hop.
fn popup_on_first() -> bool {
    manifest_value()
        .and_then(|v| v.get("cycle_popup_on_first")?.as_bool())
        .unwrap_or(false)
}

fn fresh(s: &CycleSession, now: u64, timeout: u64) -> bool {
    now.saturating_sub(s.last_press_at) <= timeout
}

fn step(pos: usize, len: usize, reverse: bool) -> usize {
    if reverse {
        (pos + len - 1) % len
    } else {
        (pos + 1) % len
    }
}

/// Flush a finished session's outcome into the MRU store: the landed pane
/// first, then any panes the user focused during the window, chronologically
/// (each `record_event` bumps its entry to the top, so the last recorded
/// ends up most-recent — matching real focus order).
fn reconcile_records(s: &CycleSession) -> Result<()> {
    let mut records: Vec<&CyclePane> = Vec::new();
    if let Some(landed) = &s.landed {
        records.push(landed);
    }
    for p in &s.post_focus {
        if records.last() != Some(&p) {
            records.push(p);
        }
    }
    for p in records {
        tracker::record_event(MruKind::Pane, &p.pane_id, &p.workspace_id)?;
        tracker::record_event(MruKind::Workspace, &p.workspace_id, &p.workspace_id)?;
    }
    Ok(())
}

/// Consult the session for an incoming track event. Pane focus events that
/// are not the cycle's own echo (landed pane or the popup pane itself) are
/// remembered for reconciliation; all events during an active session are
/// absorbed (kept out of `mru.json`).
pub fn on_track_event(pane_event: Option<(&str, &str)>) -> Result<TrackDisposition> {
    let now = now_ms();
    let timeout = timeout_ms();
    let _lock = acquire_lock()?;
    match load_session() {
        Some(mut s) if fresh(&s, now, timeout) => {
            if let Some((pane_id, ws_id)) = pane_event {
                let is_own = s.landed.as_ref().is_some_and(|l| l.pane_id == pane_id)
                    || s.popup_pane_id.as_deref() == Some(pane_id);
                if !is_own {
                    let p = CyclePane {
                        pane_id: pane_id.to_string(),
                        workspace_id: ws_id.to_string(),
                    };
                    if s.post_focus.last() != Some(&p) {
                        s.post_focus.push(p);
                        save_session(&s)?;
                    }
                }
            }
            Ok(TrackDisposition::Absorbed)
        }
        Some(s) if s.popup_open => {
            // The popup owns an expired session's lifecycle (it commits on
            // expiry itself); don't reconcile out from under it.
            let _ = s;
            Ok(TrackDisposition::Absorbed)
        }
        Some(s) => {
            delete_session();
            reconcile_records(&s)?;
            Ok(TrackDisposition::Proceed)
        }
        None => Ok(TrackDisposition::Proceed),
    }
}

/// End any session (fresh or stale) and reconcile it. Called when the
/// navigator TUI opens normally: opening the switcher is a deliberate
/// action that terminates a cycle.
pub fn end_session_now() -> Result<()> {
    let _lock = acquire_lock()?;
    if let Some(s) = load_session() {
        delete_session();
        reconcile_records(&s)?;
    }
    Ok(())
}

/// Build the frozen cycle order: current pane first, then live panes by MRU
/// timestamp (desc), then never-focused live panes in layout order.
fn build_order(
    nodes: &[NavigationNode],
    focused: Option<&FocusedPaneInfo>,
    pane_ts: &HashMap<String, u64>,
) -> Vec<CyclePane> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut order: Vec<CyclePane> = Vec::new();

    if let Some(f) = focused {
        seen.insert(f.pane_id.as_str());
        order.push(CyclePane {
            pane_id: f.pane_id.clone(),
            workspace_id: f.workspace_id.clone(),
        });
    }

    let mut with_ts: Vec<(&NavigationNode, u64)> = nodes
        .iter()
        .filter(|n| !seen.contains(n.pane_id.as_str()))
        .filter_map(|n| pane_ts.get(&n.pane_id).map(|ts| (n, *ts)))
        .collect();
    with_ts.sort_by(|a, b| b.1.cmp(&a.1));
    for (n, _) in with_ts {
        if seen.insert(n.pane_id.as_str()) {
            order.push(CyclePane {
                pane_id: n.pane_id.clone(),
                workspace_id: n.workspace_id.clone(),
            });
        }
    }

    for n in nodes {
        if seen.insert(n.pane_id.as_str()) {
            order.push(CyclePane {
                pane_id: n.pane_id.clone(),
                workspace_id: n.workspace_id.clone(),
            });
        }
    }

    order
}

/// Open the navigator pane in cycle-popup mode. Returns the popup pane id.
fn open_cycle_popup() -> Result<Option<String>> {
    let herdr_bin = std::env::var("HERDR_BIN_PATH")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "herdr".to_string());
    let plugin_id = std::env::var("HERDR_PLUGIN_ID")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "beyondlex.herdr-recent-navigator".to_string());
    let output = Command::new(&herdr_bin)
        .args([
            "plugin",
            "pane",
            "open",
            "--plugin",
            &plugin_id,
            "--entrypoint",
            "navigator",
            "--placement",
            "popup",
            "--focus",
        ])
        .output()
        .context("Failed to run herdr plugin pane open for cycle popup")?;
    if !output.status.success() {
        anyhow::bail!(
            "herdr plugin pane open failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let pane_id = crate::extract_pane_id(&stdout_str);
    if let Some(id) = &pane_id {
        // Keep the prefix+u toggle coherent with this popup.
        let lock = crate::pane_lock_path();
        if let Some(parent) = lock.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&lock, id);
    }
    Ok(pane_id)
}

/// The `cycle` subcommand: one alt-tab step through MRU pane order.
pub fn run_cycle(reverse: bool) -> Result<()> {
    let now = now_ms();
    let timeout = timeout_ms();
    let _lock = acquire_lock()?;

    // Continue an active session.
    if let Some(mut s) = load_session().filter(|s| fresh(s, now, timeout)) {
        if s.order.is_empty() {
            delete_session();
            return Ok(());
        }
        if s.popup_open {
            // The popup is showing: presses only move the highlight.
            s.position = step(s.position, s.order.len(), reverse);
            s.last_press_at = now;
            return save_session(&s);
        }
        // Second press within the window: open the popup with the selection
        // advanced one step. Focus does not move until commit.
        s.position = step(s.position, s.order.len(), reverse);
        s.last_press_at = now;
        match open_cycle_popup() {
            Ok(pane_id) => {
                s.popup_open = true;
                s.popup_pane_id = pane_id;
                return save_session(&s);
            }
            Err(e) => {
                // Popup unavailable: fall back to headless hopping.
                log::error!("Cycle popup failed ({e}); falling back to headless hop");
                let cand = s.order[s.position].clone();
                if ipc::focus_pane(&cand.pane_id).is_ok() {
                    s.landed = Some(cand);
                }
                return save_session(&s);
            }
        }
    }

    // Reconcile a stale session before starting over. A stale popup session
    // is orphaned (its popup crashed or never committed): reconcile it too.
    if let Some(stale) = load_session() {
        delete_session();
        reconcile_records(&stale)?;
    }

    // First press: build a fresh order.
    let (nodes, focused) = ipc::fetch_all_nodes()?;
    let mru = tracker::load_mru();
    let (pane_ts, _, _) = tracker::build_timestamp_maps(&mru);
    let order = build_order(&nodes, focused.as_ref(), &pane_ts);
    if order.len() < 2 {
        return Ok(()); // nothing to cycle to
    }

    let mut s = CycleSession {
        started_at: now,
        last_press_at: now,
        order,
        position: 0,
        landed: None,
        post_focus: Vec::new(),
        popup_open: false,
        popup_pane_id: None,
    };

    // Popup-first mode: open the popup right away with the selection on the
    // MRU-previous pane; every subsequent press is a bare Tab in the popup.
    if popup_on_first() {
        s.position = step(0, s.order.len(), reverse);
        match open_cycle_popup() {
            Ok(pane_id) => {
                s.popup_open = true;
                s.popup_pane_id = pane_id;
                return save_session(&s);
            }
            Err(e) => {
                log::error!("Cycle popup failed ({e}); falling back to headless hop");
                s.position = 0;
            }
        }
    }

    // Headless first hop: straight to MRU-previous.
    let len = s.order.len();
    let mut pos = s.position;
    for _ in 0..len {
        pos = step(pos, len, reverse);
        let cand = s.order[pos].clone();
        if ipc::focus_pane(&cand.pane_id).is_ok() {
            s.position = pos;
            s.landed = Some(cand);
            save_session(&s)?;
            return Ok(());
        }
    }
    Ok(())
}

// ── Cycle popup mode ──

/// If a cycle session with an open popup exists, return it. Called by the
/// pane entrypoint to decide between normal navigator and cycle popup mode.
/// Freshness is not required: an expired session is committed immediately.
pub fn active_popup_session() -> Option<CycleSession> {
    let _lock = acquire_lock().ok()?;
    load_session().filter(|s| s.popup_open && !s.order.is_empty())
}

enum PopupOutcome {
    /// Focus this pane (commit or cancel target).
    Focus(String),
    /// Session was superseded or vanished; just exit.
    Quit,
}

/// Run the cycle popup: a minimal MRU pane list that follows the session
/// file. Commits (focuses the highlighted pane) when the session expires or
/// on Enter; Esc cancels back to the session's origin pane.
pub fn run_popup(initial: CycleSession) -> Result<()> {
    // Label lookup for rendering; popup still works if the fetch fails.
    let labels: HashMap<String, (String, String)> = ipc::fetch_all_nodes()
        .map(|(nodes, _)| {
            nodes
                .into_iter()
                .map(|n| {
                    let pane = n.pane_name.unwrap_or_else(|| "untitled".into());
                    (n.pane_id, (n.workspace_name, pane))
                })
                .collect()
        })
        .unwrap_or_default();

    enable_raw_mode()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;
    let outcome = popup_loop(&mut terminal, &initial, &labels);

    // Terminal cleanup mirrors the normal navigator popup.
    {
        use std::io::Write;
        let mut out = stdout();
        let _ = write!(out, "\x1b[?25h\x1b[0m");
        let _ = out.flush();
    }
    disable_raw_mode()?;
    let _ = fs::remove_file(crate::pane_lock_path());

    if let Ok(PopupOutcome::Focus(pane_id)) = &outcome {
        if let Err(e) = ipc::focus_pane(pane_id) {
            log::error!("Cycle popup: failed to focus {pane_id}: {e}");
        }
    }
    outcome.map(|_| ())
}

fn popup_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    initial: &CycleSession,
    labels: &HashMap<String, (String, String)>,
) -> Result<PopupOutcome> {
    let timeout = timeout_ms();
    loop {
        // ── Follow the session file (under lock, small critical section) ──
        let session = {
            let _lock = acquire_lock()?;
            match load_session() {
                None => return Ok(PopupOutcome::Quit),
                Some(s) if s.started_at != initial.started_at => {
                    return Ok(PopupOutcome::Quit);
                }
                Some(s) => {
                    if now_ms().saturating_sub(s.last_press_at) > timeout {
                        // Timeout: commit the highlighted pane.
                        let target = s.order[s.position].pane_id.clone();
                        delete_session();
                        return Ok(PopupOutcome::Focus(target));
                    }
                    s
                }
            }
        };

        terminal.draw(|frame| render_popup(frame, &session, labels))?;

        if !event::poll(Duration::from_millis(30))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        let advance = |reverse: bool| -> Result<()> {
            let _lock = acquire_lock()?;
            if let Some(mut s) = load_session() {
                if s.started_at == initial.started_at && !s.order.is_empty() {
                    s.position = step(s.position, s.order.len(), reverse);
                    s.last_press_at = now_ms();
                    save_session(&s)?;
                }
            }
            Ok(())
        };

        match key.code {
            KeyCode::Enter => {
                let _lock = acquire_lock()?;
                if let Some(s) = load_session().filter(|s| s.started_at == initial.started_at) {
                    let target = s.order[s.position].pane_id.clone();
                    delete_session();
                    return Ok(PopupOutcome::Focus(target));
                }
                return Ok(PopupOutcome::Quit);
            }
            KeyCode::Esc => {
                let _lock = acquire_lock()?;
                if let Some(s) = load_session().filter(|s| s.started_at == initial.started_at) {
                    // Cancel: back to the pane the cycle started from.
                    let origin = s.order[0].pane_id.clone();
                    delete_session();
                    return Ok(PopupOutcome::Focus(origin));
                }
                return Ok(PopupOutcome::Quit);
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                let _lock = acquire_lock()?;
                delete_session();
                return Ok(PopupOutcome::Quit);
            }
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => advance(false)?,
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') => advance(true)?,
            _ => {}
        }
    }
}

fn render_popup(
    frame: &mut ratatui::Frame,
    session: &CycleSession,
    labels: &HashMap<String, (String, String)>,
) {
    let area = frame.area();
    let accent = ratatui::style::Color::Cyan;
    let dim = ratatui::style::Color::DarkGray;

    let items: Vec<ListItem> = session
        .order
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let (ws, pane) = labels
                .get(&p.pane_id)
                .cloned()
                .unwrap_or_else(|| (p.workspace_id.clone(), p.pane_id.clone()));
            let marker = if i == 0 { " (current)" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {ws} "), Style::default().fg(dim)),
                Span::raw(pane),
                Span::styled(marker, Style::default().fg(dim)),
            ]))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(accent))
                .title(" Cycling panes (MRU) "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD))
        .highlight_symbol("▶ ");

    let list_area = Rect {
        height: area.height.saturating_sub(1),
        ..area
    };
    let mut state = ListState::default();
    state.select(Some(session.position.min(session.order.len().saturating_sub(1))));
    frame.render_stateful_widget(list, list_area, &mut state);

    if area.height > 1 {
        let footer = Rect {
            y: area.y + area.height - 1,
            height: 1,
            ..area
        };
        frame.render_widget(
            Line::from(Span::styled(
                " tab next · enter switch · esc cancel ",
                Style::default().fg(dim),
            )),
            footer,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::AgentStatus;
    use crate::test_helpers::with_temp_dir;

    fn node(pane_id: &str, ws_id: &str) -> NavigationNode {
        NavigationNode {
            workspace_id: ws_id.into(),
            workspace_name: format!("{ws_id}-name"),
            tab_id: format!("{ws_id}:t1"),
            tab_name: "tab".into(),
            pane_id: pane_id.into(),
            pane_name: Some(pane_id.into()),
            agent_id: None,
            agent_status: AgentStatus::None,
            last_accessed_at: 0,
        }
    }

    fn focused(pane_id: &str, ws_id: &str) -> FocusedPaneInfo {
        FocusedPaneInfo {
            pane_id: pane_id.into(),
            tab_id: format!("{ws_id}:t1"),
            workspace_id: ws_id.into(),
            label: pane_id.into(),
        }
    }

    fn session(order: &[(&str, &str)], position: usize, last_press_at: u64) -> CycleSession {
        CycleSession {
            started_at: last_press_at,
            last_press_at,
            order: order
                .iter()
                .map(|(p, w)| CyclePane {
                    pane_id: (*p).into(),
                    workspace_id: (*w).into(),
                })
                .collect(),
            position,
            landed: None,
            post_focus: Vec::new(),
            popup_open: false,
            popup_pane_id: None,
        }
    }

    // ── step ──

    #[test]
    fn test_step_forward_wraps() {
        assert_eq!(step(0, 3, false), 1);
        assert_eq!(step(2, 3, false), 0);
    }

    #[test]
    fn test_step_reverse_wraps() {
        assert_eq!(step(0, 3, true), 2);
        assert_eq!(step(1, 3, true), 0);
    }

    // ── build_order ──

    #[test]
    fn test_build_order_current_first_then_mru_then_rest() {
        let nodes = vec![node("a", "w1"), node("b", "w1"), node("c", "w2"), node("d", "w2")];
        let f = focused("c", "w2");
        let mut ts = HashMap::new();
        ts.insert("a".to_string(), 100u64);
        ts.insert("b".to_string(), 300u64);
        ts.insert("c".to_string(), 500u64); // current: excluded from MRU section
        let order = build_order(&nodes, Some(&f), &ts);
        let ids: Vec<&str> = order.iter().map(|p| p.pane_id.as_str()).collect();
        // current, then by ts desc (b > a), then never-focused (d)
        assert_eq!(ids, vec!["c", "b", "a", "d"]);
    }

    #[test]
    fn test_build_order_no_focused_pane() {
        let nodes = vec![node("a", "w1"), node("b", "w1")];
        let mut ts = HashMap::new();
        ts.insert("b".to_string(), 300u64);
        let order = build_order(&nodes, None, &ts);
        let ids: Vec<&str> = order.iter().map(|p| p.pane_id.as_str()).collect();
        assert_eq!(ids, vec!["b", "a"]);
    }

    #[test]
    fn test_build_order_dead_mru_panes_excluded() {
        // MRU knows "gone", but it's not in the live node list.
        let nodes = vec![node("a", "w1")];
        let mut ts = HashMap::new();
        ts.insert("gone".to_string(), 900u64);
        ts.insert("a".to_string(), 100u64);
        let order = build_order(&nodes, None, &ts);
        let ids: Vec<&str> = order.iter().map(|p| p.pane_id.as_str()).collect();
        assert_eq!(ids, vec!["a"]);
    }

    // ── session persistence ──

    #[test]
    fn test_session_roundtrip_and_delete() {
        with_temp_dir(|_dir| {
            let s = session(&[("a", "w1"), ("b", "w1")], 1, 42);
            save_session(&s).unwrap();
            let loaded = load_session().unwrap();
            assert_eq!(loaded.position, 1);
            assert_eq!(loaded.order.len(), 2);
            assert!(!loaded.popup_open);
            delete_session();
            assert!(load_session().is_none());
        });
    }

    #[test]
    fn test_session_loads_pre_popup_format() {
        // Session files written before the popup fields existed must load.
        with_temp_dir(|_dir| {
            let old = r#"{
                "started_at": 1, "last_press_at": 1,
                "order": [{"pane_id": "a", "workspace_id": "w1"}],
                "position": 0
            }"#;
            let path = session_path();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, old).unwrap();
            let s = load_session().unwrap();
            assert!(!s.popup_open);
            assert!(s.popup_pane_id.is_none());
        });
    }

    // ── freshness ──

    #[test]
    fn test_fresh_within_and_past_timeout() {
        let s = session(&[("a", "w1")], 0, 1000);
        assert!(fresh(&s, 1000 + DEFAULT_TIMEOUT_MS, DEFAULT_TIMEOUT_MS));
        assert!(!fresh(&s, 1001 + DEFAULT_TIMEOUT_MS, DEFAULT_TIMEOUT_MS));
    }

    // ── reconcile ──

    #[test]
    fn test_reconcile_records_landed_then_post_focus() {
        with_temp_dir(|_dir| {
            let mut s = session(&[("a", "w1"), ("b", "w1")], 1, 42);
            s.landed = Some(CyclePane {
                pane_id: "b".into(),
                workspace_id: "w1".into(),
            });
            s.post_focus.push(CyclePane {
                pane_id: "d".into(),
                workspace_id: "w2".into(),
            });
            reconcile_records(&s).unwrap();
            let entries = tracker::load_mru();
            let panes: Vec<&str> = entries
                .iter()
                .filter(|e| e.kind == MruKind::Pane)
                .map(|e| e.id.as_str())
                .collect();
            // "d" recorded last → most recent → first in MRU order.
            assert_eq!(panes, vec!["d", "b"]);
            let ws: Vec<&str> = entries
                .iter()
                .filter(|e| e.kind == MruKind::Workspace)
                .map(|e| e.id.as_str())
                .collect();
            assert_eq!(ws, vec!["w2", "w1"]);
        });
    }

    #[test]
    fn test_reconcile_skips_hopped_through_panes() {
        with_temp_dir(|_dir| {
            // Session walked a → b → c but only landed on c.
            let mut s = session(&[("a", "w1"), ("b", "w1"), ("c", "w1")], 2, 42);
            s.landed = Some(CyclePane {
                pane_id: "c".into(),
                workspace_id: "w1".into(),
            });
            reconcile_records(&s).unwrap();
            let entries = tracker::load_mru();
            let panes: Vec<&str> = entries
                .iter()
                .filter(|e| e.kind == MruKind::Pane)
                .map(|e| e.id.as_str())
                .collect();
            assert_eq!(panes, vec!["c"], "hopped-through panes must not be recorded");
        });
    }

    // ── on_track_event ──

    #[test]
    fn test_track_event_absorbed_during_fresh_session_echo_ignored() {
        with_temp_dir(|_dir| {
            let mut s = session(&[("a", "w1"), ("b", "w1")], 1, now_ms());
            s.landed = Some(CyclePane {
                pane_id: "b".into(),
                workspace_id: "w1".into(),
            });
            save_session(&s).unwrap();

            // Echo of our own focus: absorbed, not remembered.
            let d = on_track_event(Some(("b", "w1"))).unwrap();
            assert_eq!(d, TrackDisposition::Absorbed);
            assert!(load_session().unwrap().post_focus.is_empty());

            // A different pane: absorbed but remembered for reconcile.
            let d = on_track_event(Some(("x", "w2"))).unwrap();
            assert_eq!(d, TrackDisposition::Absorbed);
            assert_eq!(load_session().unwrap().post_focus.len(), 1);

            // MRU stayed clean the whole time.
            assert!(tracker::load_mru().is_empty());
        });
    }

    #[test]
    fn test_track_event_popup_pane_not_absorbed_into_post_focus() {
        with_temp_dir(|_dir| {
            let mut s = session(&[("a", "w1"), ("b", "w1")], 1, now_ms());
            s.popup_open = true;
            s.popup_pane_id = Some("popup-pane".into());
            save_session(&s).unwrap();

            let d = on_track_event(Some(("popup-pane", "w1"))).unwrap();
            assert_eq!(d, TrackDisposition::Absorbed);
            assert!(
                load_session().unwrap().post_focus.is_empty(),
                "popup's own pane must not be queued for reconcile"
            );
        });
    }

    #[test]
    fn test_track_event_expired_popup_session_still_absorbs() {
        with_temp_dir(|_dir| {
            // Popup session past its timeout: the popup commits it itself;
            // track must not reconcile-and-delete out from under it.
            let mut s = session(&[("a", "w1"), ("b", "w1")], 1, 1); // ancient
            s.popup_open = true;
            save_session(&s).unwrap();

            let d = on_track_event(Some(("z", "w3"))).unwrap();
            assert_eq!(d, TrackDisposition::Absorbed);
            assert!(load_session().is_some(), "popup session must survive");
        });
    }

    #[test]
    fn test_track_event_stale_session_reconciles_then_proceeds() {
        with_temp_dir(|_dir| {
            let mut s = session(&[("a", "w1"), ("b", "w1")], 1, 1); // ancient
            s.landed = Some(CyclePane {
                pane_id: "b".into(),
                workspace_id: "w1".into(),
            });
            save_session(&s).unwrap();

            let d = on_track_event(Some(("z", "w3"))).unwrap();
            assert_eq!(d, TrackDisposition::Proceed);
            assert!(load_session().is_none(), "stale session must be deleted");
            let panes: Vec<String> = tracker::load_mru()
                .into_iter()
                .filter(|e| e.kind == MruKind::Pane)
                .map(|e| e.id)
                .collect();
            assert_eq!(panes, vec!["b"], "landing reconciled; caller records z itself");
        });
    }

    #[test]
    fn test_track_event_no_session_proceeds() {
        with_temp_dir(|_dir| {
            let d = on_track_event(Some(("a", "w1"))).unwrap();
            assert_eq!(d, TrackDisposition::Proceed);
        });
    }

    // ── end_session_now ──

    #[test]
    fn test_end_session_now_reconciles_fresh_session() {
        with_temp_dir(|_dir| {
            let mut s = session(&[("a", "w1"), ("b", "w1")], 1, now_ms());
            s.landed = Some(CyclePane {
                pane_id: "b".into(),
                workspace_id: "w1".into(),
            });
            save_session(&s).unwrap();
            end_session_now().unwrap();
            assert!(load_session().is_none());
            assert!(!tracker::load_mru().is_empty());
        });
    }

    // ── manifest options ──

    #[test]
    fn test_manifest_cycle_options() {
        with_temp_dir(|dir| {
            fs::write(
                dir.join("herdr-plugin.toml"),
                "cycle_timeout_ms = 123\ncycle_popup_on_first = true\n",
            )
            .unwrap();
            // SAFETY: with_temp_dir holds the global env lock for the
            // closure, same discipline as HERDR_PLUGIN_STATE_DIR itself.
            unsafe {
                std::env::set_var("HERDR_PLUGIN_ROOT", dir);
            }
            let t = timeout_ms();
            let p = popup_on_first();
            unsafe {
                std::env::remove_var("HERDR_PLUGIN_ROOT");
            }
            assert_eq!(t, 123);
            assert!(p);
            // Without the env var: defaults.
            assert_eq!(timeout_ms(), DEFAULT_TIMEOUT_MS);
            assert!(!popup_on_first());
        });
    }

    // ── active_popup_session ──

    #[test]
    fn test_active_popup_session_requires_popup_flag() {
        with_temp_dir(|_dir| {
            let s = session(&[("a", "w1"), ("b", "w1")], 1, now_ms());
            save_session(&s).unwrap();
            assert!(active_popup_session().is_none());

            let mut s = s;
            s.popup_open = true;
            save_session(&s).unwrap();
            assert!(active_popup_session().is_some());
        });
    }
}
