//! Write access a tree can lose (DESIGN.md §14.5; D080, D114): a share's
//! tree is writable while the user's role allows it, and the role can change
//! or the share be revoked while changes are queued.
//!
//! Losing it closes the write gate and holds every queued entry, and every
//! entry queued while it lasts, with [`NO_WRITE_ACCESS`], the way
//! `queue park` holds an entry: nothing staged is deleted (RQ3), the files
//! stay readable in the mount, and a local change does not lift the hold.
//! An entry parked for another reason keeps it (D121). Access comes back
//! with [`Engine::restore_write_access`] (the role allows writing again) or
//! at the next mount; the daemon moves what is still queued to `orphaned/`
//! when the share is gone ([`super::orphan`]).
//!
//! Two ways to lose it: the daemon sees a role that does not allow writing
//! ([`Engine::withdraw_write_access`]), or, on a tree that
//! [follows the server](Engine::follow_write_refusals), the server refuses a
//! change ("permission denied", code 2011 or HTTP 403). My Files never
//! follows the server: a refusal there parks only its entry, as before.

use super::*;
use std::sync::atomic::AtomicBool;

/// The park reason of entries held while the tree has no write access.
pub const NO_WRITE_ACCESS: &str = "no write access (the share's role does not allow changes)";

/// Told how many entries are held when the server refused a change.
pub type WriteRefusedHook = Arc<dyn Fn(usize) + Send + Sync>;

/// A tree's write access as the server last answered it.
#[derive(Default)]
pub(crate) struct Rights {
    /// A refusal closes the gate (a share's tree).
    follows_server: AtomicBool,
    /// The server refused a change since the gate last opened.
    refused: AtomicBool,
    hook: parking_lot::Mutex<Option<WriteRefusedHook>>,
}

impl Engine {
    /// From now on a "permission denied" of a change closes the write gate
    /// and holds the queue; `hook` is told once per loss how many entries
    /// are held (D114).
    pub fn follow_write_refusals(&self, hook: WriteRefusedHook) {
        *self.rights.hook.lock() = Some(hook);
        self.rights.follows_server.store(true, Ordering::SeqCst);
    }

    /// Whether the server refused a change since the gate last opened.
    pub fn write_was_refused(&self) -> bool {
        self.rights.refused.load(Ordering::SeqCst)
    }

    /// A step of the journal was refused ("permission denied").
    pub(crate) fn write_refused(&self) {
        if !self.rights.follows_server.load(Ordering::SeqCst) {
            return;
        }
        let first = !self.rights.refused.swap(true, Ordering::SeqCst);
        let held = self.withdraw_write_access();
        if first {
            tracing::warn!(
                held,
                "the server refused a change: no write access to this tree any more; \
                 its changes are kept and wait"
            );
            let hook = self.rights.hook.lock().clone();
            if let Some(hook) = hook {
                hook(held);
            }
        }
    }

    /// Closes the write gate and holds every queued entry: the changes stay
    /// staged and readable, and wait for [`Self::restore_write_access`] or
    /// the next mount. Returns how many entries are held.
    pub fn withdraw_write_access(&self) -> usize {
        self.set_write_gate(false);
        self.journal.hold_all(NO_WRITE_ACCESS)
    }

    /// Opens the write gate and lets the held entries upload; returns how
    /// many were held.
    pub fn restore_write_access(&self) -> usize {
        self.rights.refused.store(false, Ordering::SeqCst);
        self.set_write_gate(true);
        self.journal.release_held(NO_WRITE_ACCESS)
    }
}
