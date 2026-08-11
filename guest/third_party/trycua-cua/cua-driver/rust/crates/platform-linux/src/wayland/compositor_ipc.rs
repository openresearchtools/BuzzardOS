//! Sway compositor metadata behind one internal interface.

pub use super::sway_ipc::Window;

pub fn list_windows() -> Option<Vec<Window>> {
    super::sway_ipc::list_windows()
}

pub fn window_for_id(id: u64) -> Option<Window> {
    super::sway_ipc::window_for_id(id)
}

pub fn window_for_pid(pid: u32) -> Option<Window> {
    super::sway_ipc::window_for_pid(pid)
}

pub fn window_for_title(title: &str) -> Option<Window> {
    super::sway_ipc::window_for_title(title)
}

pub fn window_for_app_id(app_id: &str) -> Option<Window> {
    super::sway_ipc::window_for_app_id(app_id)
}

pub fn window_origin_for_pid(pid: u32) -> Option<(i32, i32)> {
    window_for_pid(pid).map(|window| (window.x, window.y))
}

pub fn window_origin_for_title(title: &str) -> Option<(i32, i32)> {
    window_for_title(title).map(|window| (window.x, window.y))
}
