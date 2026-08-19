// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.
// Buzzard modifications: AGPL-3.0-or-later

//! Minimal session cancellation and cleanup state for daemonless CUA calls.
//!
//! This module intentionally contains no observer, metrics, event export, or
//! transport classification. Sessions exist only to revoke in-flight input and
//! release session-owned resources.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

type SessionEndHook = Arc<dyn Fn(&str) + Send + Sync>;

static HOOKS: OnceLock<Mutex<HashMap<u64, SessionEndHook>>> = OnceLock::new();
static NEXT_HOOK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static ENDED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
static GENERATIONS: OnceLock<Mutex<HashMap<String, u64>>> = OnceLock::new();

fn hooks() -> &'static Mutex<HashMap<u64, SessionEndHook>> {
    HOOKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ended() -> &'static Mutex<HashSet<String>> {
    ENDED.get_or_init(|| Mutex::new(HashSet::new()))
}

fn generations() -> &'static Mutex<HashMap<String, u64>> {
    GENERATIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn trackable(session: &str) -> bool {
    !session.is_empty() && session != "default"
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionLease {
    session_id: String,
    generation: u64,
}

impl SessionLease {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

pub fn capture_session_lease(session: &str) -> Option<SessionLease> {
    if !trackable(session) || is_session_ended(session) {
        return None;
    }
    let generation = *generations()
        .lock()
        .unwrap()
        .entry(session.to_owned())
        .or_insert(1);
    Some(SessionLease {
        session_id: session.to_owned(),
        generation,
    })
}

pub fn session_lease_is_current(lease: &SessionLease) -> bool {
    !is_session_ended(&lease.session_id)
        && generations()
            .lock()
            .unwrap()
            .get(&lease.session_id)
            .copied()
            == Some(lease.generation)
}

pub fn is_session_ended(session: &str) -> bool {
    trackable(session) && ended().lock().unwrap().contains(session)
}

pub fn revive_session(session: &str) -> bool {
    if !trackable(session) {
        return false;
    }
    let revived = ended().lock().unwrap().remove(session);
    if revived {
        let mut state = generations().lock().unwrap();
        *state.entry(session.to_owned()).or_insert(1) += 1;
    }
    revived
}

pub fn touch_session(session: &str) {
    if trackable(session) && !is_session_ended(session) {
        generations()
            .lock()
            .unwrap()
            .entry(session.to_owned())
            .or_insert(1);
    }
}

pub fn fire_session_end(session: &str) -> bool {
    if !trackable(session) || !ended().lock().unwrap().insert(session.to_owned()) {
        return false;
    }
    if let Some(generation) = generations().lock().unwrap().get_mut(session) {
        *generation += 1;
    }
    crate::core::capture_scope::clear_session(session);
    let callbacks = hooks()
        .lock()
        .unwrap()
        .values()
        .cloned()
        .collect::<Vec<_>>();
    for callback in callbacks {
        callback(session);
    }
    true
}

pub fn end_session(session: &str) {
    fire_session_end(session);
}

pub fn register_session_end_hook(callback: impl Fn(&str) + Send + Sync + 'static) {
    std::mem::forget(register_scoped_session_end_hook(callback));
}

pub fn register_scoped_session_end_hook(
    callback: impl Fn(&str) + Send + Sync + 'static,
) -> SessionEndHookRegistration {
    let id = NEXT_HOOK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    hooks().lock().unwrap().insert(id, Arc::new(callback));
    SessionEndHookRegistration { id }
}

pub struct SessionEndHookRegistration {
    id: u64,
}

impl Drop for SessionEndHookRegistration {
    fn drop(&mut self) {
        hooks().lock().unwrap().remove(&self.id);
    }
}
