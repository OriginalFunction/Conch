//! User-level service units so conchd survives logout/reboot.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn render_launchd(conchd: &Path, data_dir: &Path) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
  <key>Label</key>
  <string>com.conch.conchd</string>
  <key>ProgramArguments</key>
  <array>
    <string>{conchd}</string>
    <string>--localhost</string>
    <string>--data-dir</string>
    <string>{data}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
",
        conchd = conchd.display(),
        data = data_dir.display(),
        log = data_dir.join("conchd.log").display()
    )
}

pub fn render_systemd(conchd: &Path, data_dir: &Path) -> String {
    format!(
        "[Unit]
Description=Conch room daemon
After=network.target

[Service]
ExecStart={conchd} --localhost --data-dir {data}
Restart=always
RestartSec=2

[Install]
WantedBy=default.target
",
        conchd = conchd.display(),
        data = data_dir.display()
    )
}

pub fn is_homebrew(conchd: &Path) -> bool {
    let text = conchd.to_string_lossy();
    text.starts_with("/opt/homebrew/")
        || text.contains("/Cellar/")
        || text.starts_with("/home/linuxbrew/")
}

pub fn unit_path(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents/com.conch.conchd.plist")
    } else {
        home.join(".config/systemd/user/conchd.service")
    }
}

fn home() -> std::io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| std::io::Error::other("HOME is not set"))
}

fn run(program: &str, args: &[String]) -> std::io::Result<()> {
    let status = Command::new(program).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{program} {} failed with {status}",
            args.join(" ")
        )))
    }
}

/// Install and start the user-level unit, or explain the Homebrew path.
pub fn install(conchd: &Path, data_dir: &Path) -> std::io::Result<()> {
    if is_homebrew(conchd) {
        println!("conchd comes from Homebrew; run: brew services start conch");
        return Ok(());
    }
    let unit = unit_path(&home()?);
    fs::create_dir_all(unit.parent().unwrap())?;
    if cfg!(target_os = "macos") {
        fs::write(&unit, render_launchd(conchd, data_dir))?;
        let uid = String::from_utf8_lossy(&Command::new("id").arg("-u").output()?.stdout)
            .trim()
            .to_string();
        let _ = run(
            "launchctl",
            &["bootout".into(), format!("gui/{uid}/com.conch.conchd")],
        );
        run(
            "launchctl",
            &[
                "bootstrap".into(),
                format!("gui/{uid}"),
                unit.display().to_string(),
            ],
        )?;
    } else {
        fs::write(&unit, render_systemd(conchd, data_dir))?;
        run("systemctl", &["--user".into(), "daemon-reload".into()])?;
        run(
            "systemctl",
            &[
                "--user".into(),
                "enable".into(),
                "--now".into(),
                "conchd".into(),
            ],
        )?;
    }
    println!("service installed: {}", unit.display());
    Ok(())
}

pub fn uninstall(_data_dir: &Path) -> std::io::Result<()> {
    let unit = unit_path(&home()?);
    if !unit.exists() {
        println!("no conch service unit at {}", unit.display());
        return Ok(());
    }
    if cfg!(target_os = "macos") {
        let uid = String::from_utf8_lossy(&Command::new("id").arg("-u").output()?.stdout)
            .trim()
            .to_string();
        let _ = run(
            "launchctl",
            &["bootout".into(), format!("gui/{uid}/com.conch.conchd")],
        );
    } else {
        let _ = run(
            "systemctl",
            &[
                "--user".into(),
                "disable".into(),
                "--now".into(),
                "conchd".into(),
            ],
        );
    }
    fs::remove_file(&unit)?;
    println!("service removed: {}", unit.display());
    Ok(())
}

/// For `doctor`: is a unit file present for this user?
pub fn unit_installed() -> bool {
    home().map(|h| unit_path(&h).exists()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn launchd_plist_runs_at_load_and_keeps_alive() {
        let plist = render_launchd(Path::new("/opt/bin/conchd"), Path::new("/home/u/.conch"));
        assert!(plist.contains("<key>RunAtLoad</key>\n  <true/>"));
        assert!(plist.contains("<key>KeepAlive</key>\n  <true/>"));
        assert!(plist.contains("<string>/opt/bin/conchd</string>\n    <string>--localhost</string>\n    <string>--data-dir</string>\n    <string>/home/u/.conch</string>"));
        assert!(plist.contains("<string>/home/u/.conch/conchd.log</string>"));
    }

    #[test]
    fn systemd_unit_restarts_and_points_at_binary() {
        let unit = render_systemd(Path::new("/opt/bin/conchd"), Path::new("/home/u/.conch"));
        assert!(unit.contains("ExecStart=/opt/bin/conchd --localhost --data-dir /home/u/.conch"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn homebrew_prefixes_are_detected() {
        assert!(is_homebrew(Path::new("/opt/homebrew/bin/conchd")));
        assert!(is_homebrew(Path::new(
            "/usr/local/Cellar/conch/1.2.2/bin/conchd"
        )));
        assert!(!is_homebrew(Path::new("/Users/me/.local/bin/conchd")));
    }

    #[test]
    fn unit_paths_are_user_level() {
        let home = Path::new("/h");
        #[cfg(target_os = "macos")]
        assert_eq!(
            unit_path(home),
            Path::new("/h/Library/LaunchAgents/com.conch.conchd.plist")
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            unit_path(home),
            Path::new("/h/.config/systemd/user/conchd.service")
        );
    }
}
