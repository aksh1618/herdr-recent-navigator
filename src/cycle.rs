//! Alt-tab style MRU pane cycling.
//!
//! Terminals cannot observe modifier release, so "keep walking deeper vs.
//! start a new cycle" is decided by a timeout-based session: `cycle`
//! invocations within `cycle_timeout_ms` of the previous press continue the
//! same session over a frozen snapshot of the MRU order; after the timeout
//! the next press starts fresh from the live MRU state.
//!
//! While a session is active, `track` events are absorbed into the session
//! instead of `mru.json` so panes that are merely hopped *through* never
//! pollute recency order. When the session ends (next press after timeout,
//! or the TUI opening), only the pane the cycle landed on — plus any panes
//! the user focused by other means during the window — are reconciled into
//! the MRU store, in chronological order.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;

use anyhow::{Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::ipc::{self, FocusedPaneInfo};
use crate::models::NavigationNode;
use crate::tracker::{self, MruKind};

const DEFAULT_TIMEOUT_MS: u64 = 2000;

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

/// Read `cycle_timeout_ms` from the plugin manifest, falling back to the
/// default. Mirrors the manifest `theme` fallback in `main.rs`.
pub fn timeout_ms() -> u64 {
    let Ok(root) = std::env::var("HERDR_PLUGIN_ROOT") else {
        return DEFAULT_TIMEOUT_MS;
    };
    let path = PathBuf::from(root).join("herdr-plugin.toml");
    let Ok(content) = fs::read_to_string(path) else {
        return DEFAULT_TIMEOUT_MS;
    };
    content
        .parse::<toml::Value>()
        .ok()
        .and_then(|v| v.get("cycle_timeout_ms")?.as_integer())
        .map(|n| n.max(0) as u64)
        .unwrap_or(DEFAULT_TIMEOUT_MS)
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
/// are not the cycle's own echo are remembered for reconciliation; all
/// events during an active session are absorbed (kept out of `mru.json`).
pub fn on_track_event(pane_event: Option<(&str, &str)>) -> Result<TrackDisposition> {
    let now = now_ms();
    let timeout = timeout_ms();
    let _lock = acquire_lock()?;
    match load_session() {
        Some(mut s) if fresh(&s, now, timeout) => {
            if let Some((pane_id, ws_id)) = pane_event {
                let is_echo = s.landed.as_ref().is_some_and(|l| l.pane_id == pane_id);
                if !is_echo {
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
        Some(s) => {
            delete_session();
            reconcile_records(&s)?;
            Ok(TrackDisposition::Proceed)
        }
        None => Ok(TrackDisposition::Proceed),
    }
}

/// End any session (fresh or stale) and reconcile it. Called when the
/// navigator TUI opens: opening the switcher is a deliberate action that
/// terminates a cycle.
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

/// Advance the session by one step (skipping panes that fail to focus,
/// e.g. closed since the snapshot) and persist it. Returns false if no pane
/// in the order could be focused.
fn advance_and_focus(s: &mut CycleSession, reverse: bool, now: u64) -> bool {
    let len = s.order.len();
    if len == 0 {
        return false;
    }
    let mut pos = s.position;
    for _ in 0..len {
        pos = step(pos, len, reverse);
        let cand = s.order[pos].clone();
        if ipc::focus_pane(&cand.pane_id).is_ok() {
            s.position = pos;
            s.landed = Some(cand);
            s.last_press_at = now;
            return true;
        }
    }
    false
}

/// The `cycle` subcommand: one alt-tab step through MRU pane order.
pub fn run_cycle(reverse: bool) -> Result<()> {
    let now = now_ms();
    let timeout = timeout_ms();
    let _lock = acquire_lock()?;

    // Continue an active session.
    if let Some(mut s) = load_session().filter(|s| fresh(s, now, timeout)) {
        if advance_and_focus(&mut s, reverse, now) {
            return save_session(&s);
        }
        // Nothing in the snapshot is focusable anymore: end the session and
        // fall through to build a fresh one.
        delete_session();
        reconcile_records(&s)?;
    }

    // Reconcile a stale session before starting over.
    if let Some(stale) = load_session() {
        delete_session();
        reconcile_records(&stale)?;
    }

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
    };
    if advance_and_focus(&mut s, reverse, now) {
        save_session(&s)?;
    }
    Ok(())
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
            delete_session();
            assert!(load_session().is_none());
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
}
