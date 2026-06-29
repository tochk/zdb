//! Connection passwords stored in the OS keychain: Windows Credential Manager,
//! macOS Keychain, or the Linux Secret Service.
//!
//! All operations are best-effort. If no keychain/secret service is available
//! (e.g. a headless Linux box), these degrade quietly — callers fall back to an
//! in-memory password for the session.

const SERVICE: &str = "zdb";

/// Store (or replace) the password for a connection. Returns whether it stuck.
pub fn set_password(name: &str, password: &str) -> bool {
    keyring::Entry::new(SERVICE, name)
        .and_then(|e| e.set_password(password))
        .is_ok()
}

/// Load a stored password, or `None` if absent / unavailable.
pub fn get_password(name: &str) -> Option<String> {
    keyring::Entry::new(SERVICE, name).ok()?.get_password().ok()
}

/// Remove a stored password (best effort).
pub fn delete_password(name: &str) {
    if let Ok(entry) = keyring::Entry::new(SERVICE, name) {
        let _ = entry.delete_credential();
    }
}
