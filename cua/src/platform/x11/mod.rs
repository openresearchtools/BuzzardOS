//! X11 window enumeration via x11rb.
//!
//! Uses _NET_CLIENT_LIST_STACKING to get the list of top-level windows,
//! then reads WM_NAME/_NET_WM_NAME, _NET_WM_PID, and geometry per window.

use anyhow::Result;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

#[derive(Debug, Clone)]
pub struct WindowInfo {
    /// X11 Window (XID) cast to u64.
    pub xid: u64,
    pub pid: Option<u32>,
    pub app_name: String,
    pub title: String,
    pub is_on_screen: bool,
    pub z_index: Option<usize>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// List top-level windows, optionally filtered by pid.
pub fn list_windows(filter_pid: Option<u32>) -> Vec<WindowInfo> {
    match list_windows_inner(filter_pid) {
        Ok(w) => w,
        Err(_) => Vec::new(),
    }
}

fn list_windows_inner(filter_pid: Option<u32>) -> Result<Vec<WindowInfo>> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    // Get _NET_CLIENT_LIST_STACKING (or fallback to _NET_CLIENT_LIST).
    let windows = get_window_list(&conn, root)?;

    let mut result = Vec::new();
    for (z_index, xid) in windows.into_iter().enumerate() {
        let pid = get_window_pid(&conn, xid).ok().flatten();
        if let Some(fp) = filter_pid {
            if pid != Some(fp) {
                continue;
            }
        }

        let title = get_window_title(&conn, xid).unwrap_or_default();
        if title.trim().is_empty() {
            continue;
        }
        let app_name = get_window_class(&conn, xid)
            .map(|(instance, class)| if class.is_empty() { instance } else { class })
            .unwrap_or_default();
        let is_on_screen = conn
            .get_window_attributes(xid)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|attributes| attributes.map_state == MapState::VIEWABLE);

        let geom = conn.get_geometry(xid)?.reply().ok();
        let (x, y, w, h) = if let Some(g) = geom {
            // Translate to root coordinates.
            let trans = conn.translate_coordinates(xid, root, 0, 0)?.reply().ok();
            let (rx, ry) = trans
                .map(|t| (t.dst_x as i32, t.dst_y as i32))
                .unwrap_or((0, 0));
            (rx, ry, g.width as u32, g.height as u32)
        } else {
            (0, 0, 0, 0)
        };

        result.push(WindowInfo {
            xid: xid as u64,
            pid,
            app_name,
            title,
            is_on_screen,
            z_index: Some(z_index_from_bottom_to_top(z_index)),
            x,
            y,
            width: w,
            height: h,
        });
    }

    Ok(result)
}

fn z_index_from_bottom_to_top(position: usize) -> usize {
    position
}

fn get_window_list(conn: &RustConnection, root: Window) -> Result<Vec<Window>> {
    let atom_names = ["_NET_CLIENT_LIST_STACKING", "_NET_CLIENT_LIST"];
    for name in &atom_names {
        if let Ok(atom) = get_atom(conn, name) {
            if let Ok(reply) = conn
                .get_property(false, root, atom, AtomEnum::WINDOW, 0, u32::MAX)?
                .reply()
            {
                let windows: Vec<Window> = reply
                    .value32()
                    .map(|iter| iter.collect())
                    .unwrap_or_default();
                if client_list_property(reply.type_, windows.as_slice()).is_some() {
                    return Ok(windows);
                }
            }
        }
    }

    // No EWMH client-list property means there may be no window manager. In
    // that case only expose mapped root children; unmapped Electron children
    // can otherwise be reported before a late-starting WM reparents them.
    let tree = conn.query_tree(root)?.reply()?;
    Ok(tree
        .children
        .into_iter()
        .filter(|window| {
            conn.get_window_attributes(*window)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .map(|attributes| fallback_window_is_listable(attributes.map_state))
                .unwrap_or(false)
        })
        .collect())
}

fn client_list_property(property_type: Atom, windows: &[Window]) -> Option<&[Window]> {
    (property_type != x11rb::NONE).then_some(windows)
}

fn fallback_window_is_listable(map_state: MapState) -> bool {
    map_state == MapState::VIEWABLE
}

fn get_atom(conn: &RustConnection, name: &str) -> Result<Atom> {
    Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

/// Ask the X11 window manager to set one exact top-level window frame, then
/// read the window geometry back in the same desktop coordinate space exposed
/// by `list_windows`. Uses EWMH rather than configuring a client directly.
pub fn set_window_frame(
    xid: u64,
    pid: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<(Option<WindowInfo>, bool, Option<String>)> {
    let xid = u32::try_from(xid).map_err(|_| anyhow::anyhow!("window_id is out of X11 range"))?;
    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let owner = get_window_pid(&conn, xid)?;
    match owner {
        Some(owner) if owner == pid => {}
        Some(owner) => anyhow::bail!("window_id {xid} belongs to pid {owner}, not pid {pid}"),
        None => anyhow::bail!("window_id {xid} has no verifiable _NET_WM_PID owner"),
    }

    let atom = get_atom(&conn, "_NET_MOVERESIZE_WINDOW")?;
    let fields = (1_u32 << 8) | (1 << 9) | (1 << 10) | (1 << 11);
    let source_indication = 1_u32 << 12; // normal application (EWMH §4.1.5)
    let event = ClientMessageEvent::new(
        32,
        xid,
        atom,
        ClientMessageData::from([
            fields | source_indication,
            x as u32,
            y as u32,
            width,
            height,
        ]),
    );
    let mutation_error = conn
        .send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )
        .and_then(|_| conn.flush())
        .err()
        .map(|error| format!("_NET_MOVERESIZE_WINDOW request failed: {error}"));

    let requested = (x, y, width, height);
    let mut observed = None;
    for _ in 0..8 {
        observed = list_windows(Some(pid))
            .into_iter()
            .find(|window| window.xid == u64::from(xid));
        if observed
            .as_ref()
            .is_some_and(|window| (window.x, window.y, window.width, window.height) == requested)
        {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    let confirmed = observed
        .as_ref()
        .is_some_and(|window| (window.x, window.y, window.width, window.height) == requested);
    Ok((observed, confirmed, mutation_error))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowControlAction {
    Close,
    Minimize,
    Maximize,
    Restore,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowControlState {
    pub present: bool,
    pub minimized: bool,
    pub maximized: bool,
}

fn read_window_control_state(
    conn: &RustConnection,
    root: Window,
    xid: Window,
) -> Result<WindowControlState> {
    let present = get_window_list(conn, root)?.contains(&xid);
    if !present {
        return Ok(WindowControlState::default());
    }
    let minimized = conn
        .get_window_attributes(xid)?
        .reply()
        .map(|attributes| attributes.map_state != MapState::VIEWABLE)
        .unwrap_or(true);
    let net_wm_state = get_atom(conn, "_NET_WM_STATE")?;
    let max_vert = get_atom(conn, "_NET_WM_STATE_MAXIMIZED_VERT")?;
    let max_horz = get_atom(conn, "_NET_WM_STATE_MAXIMIZED_HORZ")?;
    let states = conn
        .get_property(false, xid, net_wm_state, AtomEnum::ATOM, 0, u32::MAX)?
        .reply()
        .ok()
        .and_then(|reply| reply.value32().map(Iterator::collect::<Vec<_>>))
        .unwrap_or_default();
    Ok(WindowControlState {
        present,
        minimized,
        maximized: states.contains(&max_vert) && states.contains(&max_horz),
    })
}

fn x11_window_control_satisfied(action: WindowControlAction, state: WindowControlState) -> bool {
    match action {
        WindowControlAction::Close => !state.present,
        WindowControlAction::Minimize => state.present && state.minimized,
        WindowControlAction::Maximize => state.present && state.maximized,
        WindowControlAction::Restore => state.present && !state.minimized && !state.maximized,
    }
}

/// Request an orderly EWMH/ICCCM window state change and independently read
/// the exact target's resulting state back from the window manager.
pub fn control_window(
    xid: u64,
    pid: u32,
    action: WindowControlAction,
) -> Result<(WindowControlState, WindowControlState)> {
    let xid = u32::try_from(xid).map_err(|_| anyhow::anyhow!("window_id is out of X11 range"))?;
    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let owner = get_window_pid(&conn, xid)?;
    match owner {
        Some(owner) if owner == pid => {}
        Some(owner) => anyhow::bail!("window_id {xid} belongs to pid {owner}, not pid {pid}"),
        None => anyhow::bail!("window_id {xid} has no verifiable _NET_WM_PID owner"),
    }
    let before = read_window_control_state(&conn, root, xid)?;

    let (atom, data) = match action {
        WindowControlAction::Close => (get_atom(&conn, "_NET_CLOSE_WINDOW")?, [0, 1, 0, 0, 0]),
        WindowControlAction::Minimize => (
            get_atom(&conn, "WM_CHANGE_STATE")?,
            [3, 0, 0, 0, 0], // ICCCM IconicState
        ),
        WindowControlAction::Maximize => (
            get_atom(&conn, "_NET_WM_STATE")?,
            [
                1, // _NET_WM_STATE_ADD
                get_atom(&conn, "_NET_WM_STATE_MAXIMIZED_VERT")?,
                get_atom(&conn, "_NET_WM_STATE_MAXIMIZED_HORZ")?,
                1,
                0,
            ],
        ),
        WindowControlAction::Restore => (
            get_atom(&conn, "_NET_WM_STATE")?,
            [
                0, // _NET_WM_STATE_REMOVE
                get_atom(&conn, "_NET_WM_STATE_MAXIMIZED_VERT")?,
                get_atom(&conn, "_NET_WM_STATE_MAXIMIZED_HORZ")?,
                1,
                0,
            ],
        ),
    };
    let event = ClientMessageEvent::new(32, xid, atom, ClientMessageData::from(data));
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        event,
    )?;
    if action == WindowControlAction::Restore && before.minimized {
        let active = get_atom(&conn, "_NET_ACTIVE_WINDOW")?;
        let event =
            ClientMessageEvent::new(32, xid, active, ClientMessageData::from([1, 0, 0, 0, 0]));
        conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            event,
        )?;
    }
    conn.flush()?;

    let mut after = before;
    for _ in 0..20 {
        after = read_window_control_state(&conn, root, xid)?;
        if x11_window_control_satisfied(action, after) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Ok((before, after))
}

fn get_window_pid(conn: &RustConnection, window: Window) -> Result<Option<u32>> {
    let atom = get_atom(conn, "_NET_WM_PID")?;
    let reply = conn
        .get_property(false, window, atom, AtomEnum::CARDINAL, 0, 1)?
        .reply()?;
    Ok(reply.value32().and_then(|mut i| i.next()))
}

fn get_window_title(conn: &RustConnection, window: Window) -> Result<String> {
    // Try _NET_WM_NAME (UTF-8) first.
    if let Ok(atom) = get_atom(conn, "_NET_WM_NAME") {
        if let Ok(utf8_atom) = get_atom(conn, "UTF8_STRING") {
            if let Ok(reply) = conn
                .get_property(false, window, atom, utf8_atom, 0, 1024)?
                .reply()
            {
                if !reply.value.is_empty() {
                    return Ok(String::from_utf8_lossy(&reply.value).into_owned());
                }
            }
        }
    }
    // Fallback: WM_NAME (latin-1 / ASCII).
    let reply = conn
        .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)?
        .reply()?;
    Ok(String::from_utf8_lossy(&reply.value).into_owned())
}

/// Return the WM_CLASS pair for `xid` as `(instance, class)`.
///
/// X11's `WM_CLASS` property is two NUL-separated strings; the first is
/// the instance name, the second is the class name. Either field can
/// be empty. Used by [`crate::platform::terminal::is_terminal_window`] to detect
/// terminal emulators that share a process tree with another GUI
/// (e.g. Ghostty's `WM_CLASS = "ghostty\0Ghostty\0"`).
///
/// Returns `None` when no X connection is available, the window has no
/// WM_CLASS atom set, or the property could not be read.
pub fn wm_class_for_window(xid: u64) -> Option<(String, String)> {
    let (conn, _) = RustConnection::connect(None).ok()?;
    get_window_class(&conn, xid as u32)
}

fn get_window_class(conn: &RustConnection, xid: Window) -> Option<(String, String)> {
    let reply = conn
        .get_property(
            false,
            xid as u32,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            0,
            512,
        )
        .ok()?
        .reply()
        .ok()?;
    let raw = reply.value;
    let mut parts = raw.split(|&b| b == 0).filter(|s| !s.is_empty());
    let instance = parts
        .next()
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .unwrap_or_default();
    let class = parts
        .next()
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .unwrap_or_default();
    if instance.is_empty() && class.is_empty() {
        return None;
    }
    Some((instance, class))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_present_client_list_does_not_fall_back_to_query_tree() {
        assert_eq!(client_list_property(1, &[]), Some([].as_slice()));
    }

    #[test]
    fn absent_client_list_allows_query_tree_fallback() {
        assert_eq!(client_list_property(x11rb::NONE, &[]), None);
    }

    #[test]
    fn query_tree_fallback_only_lists_viewable_windows() {
        assert!(fallback_window_is_listable(MapState::VIEWABLE));
        assert!(!fallback_window_is_listable(MapState::UNMAPPED));
        assert!(!fallback_window_is_listable(MapState::UNVIEWABLE));
    }

    #[test]
    fn ewmh_bottom_to_top_order_normalizes_to_higher_is_frontmost() {
        let indices: Vec<_> = (0..3).map(z_index_from_bottom_to_top).collect();
        assert_eq!(indices, vec![0, 1, 2]);
        assert!(indices[2] > indices[0]);
    }
}
