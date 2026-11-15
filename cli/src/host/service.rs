//! `dirigent host install|uninstall`: the host as your service, started with your session and
//! again when it stops: a launchd agent on macOS (in your login session, where Xcode's signing
//! keychain and the Simulator are; a locked screen doesn't stop it, logging out does), a systemd
//! user unit on Linux (lingering, so it runs from boot). Never root: the host and its players are
//! yours, with your credentials (`~/.config/dirigent/credentials.json`). Players outlive the
//! daemon (each holds its session's lease), so the services stop only the daemon itself.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Context;

/// The agent's label (`dirigent host install`'s; home-manager's and Homebrew's have their own).
pub const LABEL: &str = "dev.dirigent.host";
pub const UNIT: &str = "dirigent-host.service";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Macos,
    Linux,
}

pub fn os() -> anyhow::Result<Os> {
    match std::env::consts::OS {
        "macos" => Ok(Os::Macos),
        "linux" => Ok(Os::Linux),
        other => anyhow::bail!("a host on {other} isn't a service dirigent installs"),
    }
}

/// What the service runs: this binary, `host run`, with `env`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Run {
    exe: PathBuf,
    env: Vec<(String, String)>,
}

/// The service's file.
fn file(os: Os) -> anyhow::Result<PathBuf> {
    Ok(match os {
        Os::Macos => home()?.join(format!("Library/LaunchAgents/{LABEL}.plist")),
        Os::Linux => home()?.join(format!(".config/systemd/user/{UNIT}")),
    })
}

/// The agent's log (on Linux, the journal's).
fn log() -> anyhow::Result<PathBuf> {
    Ok(home()?.join("Library/Logs/dirigent-host.log"))
}

/// A launchd agent's property list.
fn plist(run: &Run, log: &Path) -> String {
    let string = |s: &str| format!("<string>{}</string>", xml(s));
    let env: Vec<String> = run
        .env
        .iter()
        .map(|(k, v)| format!("    <key>{}</key>{}", xml(k), string(v)))
        .collect();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>Label</key>{label}\n\
         \x20 <key>ProgramArguments</key>\n  <array>{exe}{host}{run}</array>\n\
         \x20 <key>EnvironmentVariables</key>\n  <dict>\n{env}\n  </dict>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><true/>\n\
         \x20 <!-- players outlive the daemon: they hold their sessions' leases -->\n\
         \x20 <key>AbandonProcessGroup</key><true/>\n\
         \x20 <key>StandardOutPath</key>{log}\n\
         \x20 <key>StandardErrorPath</key>{log}\n\
         </dict>\n</plist>\n",
        label = string(LABEL),
        exe = string(&run.exe.to_string_lossy()),
        host = string("host"),
        run = string("run"),
        env = env.join("\n"),
        log = string(&log.to_string_lossy()),
    )
}

/// A systemd user unit.
fn unit(run: &Run) -> String {
    let env: Vec<String> = run
        .env
        .iter()
        .map(|(k, v)| format!("Environment={}\n", quote(&format!("{k}={v}"))))
        .collect();
    format!(
        "[Unit]\n\
         Description=dirigent host: runs players for your sessions\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exe} host run\n\
         {env}\
         Restart=always\n\
         RestartSec=5\n\
         # players outlive the daemon: they hold their sessions' leases\n\
         KillMode=process\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = quote(&run.exe.to_string_lossy()),
        env = env.join(""),
    )
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// A word for systemd's command lines and assignments.
fn quote(s: &str) -> String {
    match s.contains([' ', '"', '\\', '\'']) {
        true => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        false => s.to_string(),
    }
}

pub fn home() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").context("HOME isn't set")?,
    ))
}

/// This binary, by the path it was run by (a profile's or Homebrew's link, which upgrades keep),
/// not the file it resolves to.
fn exe() -> anyhow::Result<PathBuf> {
    let arg0 = PathBuf::from(std::env::args_os().next().context("no argv[0]")?);
    if arg0.components().count() > 1 {
        return Ok(std::path::absolute(&arg0)?);
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(&arg0))
        .find(|p| p.is_file())
        .map_or_else(|| Ok(std::env::current_exe()?), Ok)
}

/// `program`'s output, if it succeeds.
pub async fn output(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let out = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("running {program}"))?;
    anyhow::ensure!(
        out.status.success(),
        "{program} {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

async fn command(program: &str, args: &[&str]) -> anyhow::Result<()> {
    output(program, args).await.map(drop)
}

pub async fn uid() -> anyhow::Result<String> {
    Ok(output("id", &["-u"]).await?.trim().to_string())
}

/// The host is never root's: refuse to be it.
pub async fn not_root(what: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        uid().await? != "0",
        "{what} as the user whose sessions it runs, not as root (no sudo): the host and its \
         players are yours, signed in with your `dirigent login`"
    );
    Ok(())
}

/// Install the service and start it (in place of one there already).
pub async fn install() -> anyhow::Result<()> {
    let os = os()?;
    not_root("install the host").await?;
    let file = file(os)?;
    let run = Run {
        exe: exe()?,
        env: vec![(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        )],
    };
    let text = match os {
        Os::Macos => plist(&run, &log()?),
        Os::Linux => unit(&run),
    };
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = stop(os, &file).await;
    std::fs::write(&file, text).with_context(|| format!("writing {}", file.display()))?;
    let path = file.to_string_lossy();
    match os {
        Os::Macos => {
            let domain = format!("gui/{}", uid().await?);
            command("launchctl", &["bootstrap", &domain, &path])
                .await
                .context(
                    "starting it in your login session (it needs one: run this in a terminal \
                     on the Mac's screen, not over SSH)",
                )?
        }
        Os::Linux => {
            command("systemctl", &["--user", "daemon-reload"]).await?;
            command("systemctl", &["--user", "enable", "--now", UNIT]).await?;
        }
    }
    println!("Installed {}", file.display());
    match os {
        Os::Macos => {
            println!("Its log: {}", log()?.display());
            println!(
                "It runs while you're logged in (a locked screen doesn't stop it); after a \
                 restart, once you log in again: turn on automatic login to have it back by itself"
            );
        }
        Os::Linux => {
            println!("Its log: journalctl --user -u {UNIT}");
            // from boot, and after you log out
            match command("loginctl", &["enable-linger"]).await {
                Ok(()) => println!("It runs from boot (lingering is on for you)"),
                Err(e) => println!(
                    "It runs while you're logged in; to have it from boot, turn on lingering \
                     (`sudo loginctl enable-linger $USER`): {e:#}"
                ),
            }
        }
    }
    Ok(())
}

/// Stop the service, if it's there.
async fn stop(os: Os, file: &Path) -> anyhow::Result<()> {
    match os {
        Os::Macos => {
            let target = format!("gui/{}/{LABEL}", uid().await?);
            command("launchctl", &["bootout", &target]).await
        }
        Os::Linux if file.exists() => {
            command("systemctl", &["--user", "disable", "--now", UNIT]).await
        }
        Os::Linux => Ok(()),
    }
}

/// Stop the service and remove it (the players running go on until their sessions stop).
pub async fn uninstall() -> anyhow::Result<()> {
    let os = os()?;
    let file = file(os)?;
    if !file.exists() {
        println!("Not installed ({} isn't there)", file.display());
        return Ok(());
    }
    if let Err(e) = stop(os, &file).await {
        eprintln!("stopping it: {e:#}");
    }
    std::fs::remove_file(&file)?;
    if os == Os::Linux {
        command("systemctl", &["--user", "daemon-reload"]).await?;
    }
    println!("Removed {}", file.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Run {
        Run {
            exe: PathBuf::from("/opt/homebrew/bin/dirigent"),
            env: vec![("PATH".into(), "/opt/homebrew/bin:/usr/bin".into())],
        }
    }

    #[test]
    fn a_launchd_agent_runs_the_host_and_leaves_its_players() {
        let text = plist(&run(), Path::new("/Users/a/Library/Logs/dirigent-host.log"));
        assert!(text.contains("<key>Label</key><string>dev.dirigent.host</string>"));
        assert!(text.contains(
            "<array><string>/opt/homebrew/bin/dirigent</string><string>host</string>\
             <string>run</string></array>"
        ));
        assert!(text.contains("<key>PATH</key><string>/opt/homebrew/bin:/usr/bin</string>"));
        assert!(text.contains("<key>AbandonProcessGroup</key><true/>"));
        assert!(text.contains(
            "<key>StandardOutPath</key><string>/Users/a/Library/Logs/dirigent-host.log</string>"
        ));
        // the user's own: no other user, no session made for it
        assert!(!text.contains("UserName") && !text.contains("SessionCreate"));
    }

    #[test]
    fn a_systemd_user_unit_runs_the_host_and_leaves_its_players() {
        let text = unit(&run());
        assert!(text.contains("ExecStart=/opt/homebrew/bin/dirigent host run\n"));
        assert!(text.contains("Environment=PATH=/opt/homebrew/bin:/usr/bin\n"));
        assert!(text.contains("KillMode=process\n") && text.contains("WantedBy=default.target"));
        assert!(!text.contains("User="));
        assert_eq!(quote("a b\"c"), "\"a b\\\"c\"");
    }
}
