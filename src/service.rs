//! Run `ego-bridge serve` as a macOS LaunchAgent. It must live in the GUI
//! (Aqua) session: ego-browser reaches ego lite over Mach IPC, which only
//! works from inside the logged-in user's session.

use std::path::{Path, PathBuf};

pub const LABEL: &str = "io.github.nguyenvanduocit.ego-bridge";

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// The LaunchAgent property list for `program serve <serve_args...>`.
pub fn plist(program: &Path, serve_args: &[String], log_path: &Path) -> String {
    let mut arguments = vec![program.to_string_lossy().into_owned(), "serve".to_owned()];
    arguments.extend(serve_args.iter().cloned());
    let arguments: String = arguments
        .iter()
        .map(|arg| format!("    <string>{}</string>\n", xml_escape(arg)))
        .collect();
    let log = xml_escape(&log_path.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>LimitLoadToSessionType</key>
  <string>Aqua</string>
  <key>ProcessType</key>
  <string>Interactive</string>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

pub fn plist_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home)
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

fn gui_domain() -> anyhow::Result<String> {
    let output = std::process::Command::new("id").arg("-u").output()?;
    anyhow::ensure!(output.status.success(), "`id -u` failed");
    Ok(format!("gui/{}", String::from_utf8(output.stdout)?.trim()))
}

fn launchctl(args: &[&str]) -> anyhow::Result<std::process::ExitStatus> {
    Ok(std::process::Command::new("launchctl")
        .args(args)
        .status()?)
}

/// Write the LaunchAgent and (re)start it.
pub fn install(serve_args: &[String], log_path: &Path) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        cfg!(target_os = "macos"),
        "service install is only supported on macOS"
    );
    let program = std::env::current_exe()?.canonicalize()?;
    let path = plist_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, plist(&program, serve_args, log_path))?;
    let domain = gui_domain()?;
    let target = format!("{domain}/{LABEL}");
    // Unload a previous version first; a missing service is fine.
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &target])
        .stderr(std::process::Stdio::null())
        .status();
    let path_text = path.to_string_lossy().into_owned();
    let status = launchctl(&["bootstrap", &domain, &path_text])?;
    anyhow::ensure!(
        status.success(),
        "launchctl bootstrap {domain} {path_text} failed"
    );
    Ok(path)
}

/// Stop the LaunchAgent and remove its file.
pub fn uninstall() -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        cfg!(target_os = "macos"),
        "service uninstall is only supported on macOS"
    );
    let path = plist_path()?;
    let target = format!("{}/{LABEL}", gui_domain()?);
    let _ = launchctl(&["bootout", &target]);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_runs_serve_in_the_gui_session_with_escaped_arguments() {
        let text = plist(
            Path::new("/opt/ego bridge/ego-bridge"),
            &["--allowed-host".to_owned(), "a&b".to_owned()],
            Path::new("/tmp/log.txt"),
        );
        assert!(text.contains("<string>/opt/ego bridge/ego-bridge</string>"));
        assert!(text.contains("<string>serve</string>"));
        assert!(text.contains("<string>a&amp;b</string>"));
        assert!(text.contains("<key>LimitLoadToSessionType</key>\n  <string>Aqua</string>"));
    }
}
