//! Desktop notifications. Linux talks to the session's notification service
//! with `gdbus` (part of GLib, which the web view already requires), avoiding
//! a ~45-crate D-Bus stack; other platforms use the Tauri plugin.

use tauri::AppHandle;

#[cfg(target_os = "linux")]
pub fn show(_app: &AppHandle, title: &str, body: &str) {
    use std::process::{Command, Stdio};
    // Arguments go straight to gdbus (no shell); strings are GVariant-quoted.
    let _ = Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest=org.freedesktop.Notifications",
            "--object-path=/org/freedesktop/Notifications",
            "--method=org.freedesktop.Notifications.Notify",
            "'Carteiro'",
            "0",
            "'carteiro'",
            &gvariant_string(title),
            &gvariant_string(body),
            "[]",
            "{}",
            "-1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(not(target_os = "linux"))]
pub fn show(app: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let _ = app.notification().builder().title(title).body(body).show();
}

/// Android 13+ only shows notifications after the user allows them; asks
/// once (later starts find the answer already given). No-op elsewhere.
#[cfg(not(target_os = "linux"))]
pub fn ask_permission(app: &AppHandle) {
    use tauri_plugin_notification::{NotificationExt, PermissionState};
    let n = app.notification();
    if matches!(n.permission_state(), Ok(PermissionState::Prompt | PermissionState::PromptWithRationale)) {
        let _ = n.request_permission();
    }
}

#[cfg(target_os = "linux")]
pub fn ask_permission(_app: &AppHandle) {}

/// A GVariant text-format string literal.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn gvariant_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_gvariant() {
        assert_eq!(gvariant_string("Ana's \\ note\nnext"), "'Ana\\'s \\\\ note next'");
    }
}
