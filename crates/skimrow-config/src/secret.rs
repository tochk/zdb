//! Connection passwords stored in the OS keychain: Windows Credential Manager,
//! macOS Keychain, or the Linux Secret Service.
//!
//! All operations are best-effort. If no keychain/secret service is available
//! (e.g. a headless Linux box), these degrade quietly — callers fall back to an
//! in-memory password for the session.

use crate::{APP_NAME as SERVICE, LEGACY_NAME};

/// Test-only instrumentation (behind the `test-probe` feature): a per-name count
/// of [`get_password`] calls. Lets a test prove the blocking — and, on macOS,
/// prompting — keychain read is deferred to a background task instead of running
/// synchronously on the UI thread (which froze the render loop → the app hung on
/// selecting a saved connection).
#[cfg(feature = "test-probe")]
pub mod probe {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};

    static CALLS: LazyLock<Mutex<HashMap<String, usize>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));

    pub(super) fn record(name: &str) {
        *CALLS.lock().unwrap().entry(name.to_string()).or_default() += 1;
    }

    /// How many times `get_password(name)` has been called so far.
    pub fn get_password_calls(name: &str) -> usize {
        CALLS.lock().unwrap().get(name).copied().unwrap_or(0)
    }
}

/// The keychain name a connection's SSH password or key passphrase is stored
/// under (its database password is under the connection's own name).
pub fn ssh_account(name: &str) -> String {
    format!("{name} (ssh)")
}

/// Store (or replace) the password for a connection. Returns whether it stuck.
pub fn set_password(name: &str, password: &str) -> bool {
    keyring::Entry::new(SERVICE, name)
        .and_then(|e| e.set_password(password))
        .is_ok()
}

/// Load a stored password, or `None` if absent / unavailable. One saved
/// before the rename (service `zdb`) is moved to the current service on first
/// read.
pub fn get_password(name: &str) -> Option<String> {
    #[cfg(feature = "test-probe")]
    probe::record(name);
    if let Some(password) = keyring::Entry::new(SERVICE, name).ok()?.get_password().ok() {
        return Some(password);
    }
    let legacy = keyring::Entry::new(LEGACY_NAME, name).ok()?;
    let password = legacy.get_password().ok()?;
    if set_password(name, &password) {
        let _ = legacy.delete_credential();
    }
    Some(password)
}

/// Remove a stored password (best effort), including a pre-rename one that was
/// never read — else [`get_password`] would bring it back.
pub fn delete_password(name: &str) {
    for service in [SERVICE, LEGACY_NAME] {
        if let Ok(entry) = keyring::Entry::new(service, name) {
            let _ = entry.delete_credential();
        }
    }
}
