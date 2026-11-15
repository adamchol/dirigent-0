//! `dirigent host install|uninstall|status`: the host as a service of the system's, started
//! with it and again when it stops. Yours (a launchd agent on macOS, a systemd user unit on
//! Linux: while you're logged in), or the system's (`--system`, as root: a launchd daemon, a
//! systemd system unit, running as root or `--user`, with the credentials and config in
//! `/etc/dirigent`). Players outlive the daemon (each holds its session's lease), so the services
//! stop only the daemon itself.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::Context;

const LABEL: &str = "dev.dirigent.host";
const UNIT: &str = "dirigent-host.service";
/// The system's service's credentials and config.
const ETC: &str = "/etc/dirigent";

/// Which service: yours, or the system's (as `user`, by default root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub system: bool,
    pub user: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Os {
    Macos,
    Linux,
}

fn os() -> anyhow::Result<Os> {
    match std::env::consts::OS {
        "macos" => Ok(Os::Macos),
        "linux" => Ok(Os::Linux),
        other => anyhow::bail!("a host on {other} isn't a service dirigent installs"),
    }
}

/// What a service runs: this binary, `host run`, with `env`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Run {
    exe: PathBuf,
    args: Vec<String>,
    env: Vec<(String, String)>,
}

impl Service {
    fn file(&self, os: Os) -> anyhow::Result<PathBuf> {
        Ok(match (os, self.system) {
            (Os::Macos, false) => home()?.join(format!("Library/LaunchAgents/{LABEL}.plist")),
            (Os::Macos, true) => PathBuf::from(format!("/Library/LaunchDaemons/{LABEL}.plist")),
            (Os::Linux, false) => home()?.join(format!(".config/systemd/user/{UNIT}")),
            (Os::Linux, true) => PathBuf::from(format!("/etc/systemd/system/{UNIT}")),
        })
    }

    fn log(&self) -> anyhow::Result<PathBuf> {
        Ok(match self.system {
            true => PathBuf::from("/var/log/dirigent-host.log"),
            false => home()?.join("Library/Logs/dirigent-host.log"),
        })
    }

    fn run(&self, exe: PathBuf) -> Run {
        let mut args = vec!["host".to_string(), "run".to_string()];
        let mut env = vec![(
            "PATH".to_string(),
            std::env::var("PATH").unwrap_or_default(),
        )];
        if self.system {
            args.push("--system".into());
            env.push((
                "DIRIGENT_CREDENTIALS".into(),
                format!("{ETC}/credentials.json"),
            ));
            env.push(("DIRIGENT_HOST_CONFIG".into(), format!("{ETC}/host.toml")));
            if let Some(user) = &self.user {
                if let Some(home) = home_of(user) {
                    env.push(("HOME".into(), home));
                }
            }
        }
        Run { exe, args, env }
    }
}

/// A launchd job's property list.
fn plist(service: &Service, run: &Run, log: &Path) -> String {
    let string = |s: &str| format!("<string>{}</string>", xml(s));
    let mut args: Vec<String> = vec![string(&run.exe.to_string_lossy())];
    args.extend(run.args.iter().map(|a| string(a)));
    let env: Vec<String> = run
        .env
        .iter()
        .map(|(k, v)| format!("    <key>{}</key>{}", xml(k), string(v)))
        .collect();
    let user = match (&service.user, service.system) {
        (Some(user), true) => format!("  <key>UserName</key>{}\n", string(user)),
        _ => String::new(),
    };
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>Label</key>{label}\n\
         \x20 <key>ProgramArguments</key>\n  <array>{args}</array>\n\
         \x20 <key>EnvironmentVariables</key>\n  <dict>\n{env}\n  </dict>\n\
         {user}\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><true/>\n\
         \x20 <!-- players outlive the daemon: they hold their sessions' leases -->\n\
         \x20 <key>AbandonProcessGroup</key><true/>\n\
         \x20 <key>StandardOutPath</key>{log}\n\
         \x20 <key>StandardErrorPath</key>{log}\n\
         </dict>\n</plist>\n",
        label = string(LABEL),
        args = args.join(""),
        env = env.join("\n"),
        log = string(&log.to_string_lossy()),
    )
}

/// A systemd unit.
fn unit(service: &Service, run: &Run) -> String {
    let mut exec = vec![quote(&run.exe.to_string_lossy())];
    exec.extend(run.args.iter().map(|a| quote(a)));
    let env: Vec<String> = run
        .env
        .iter()
        .map(|(k, v)| format!("Environment={}\n", quote(&format!("{k}={v}"))))
        .collect();
    let user = match (&service.user, service.system) {
        (Some(user), true) => format!("User={user}\n"),
        _ => String::new(),
    };
    let wanted = match service.system {
        true => "multi-user.target",
        false => "default.target",
    };
    format!(
        "[Unit]\n\
         Description=dirigent host: runs players for your sessions\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={exec}\n\
         {env}{user}\
         Restart=always\n\
         RestartSec=5\n\
         # players outlive the daemon: they hold their sessions' leases\n\
         KillMode=process\n\
         \n\
         [Install]\n\
         WantedBy={wanted}\n",
        exec = exec.join(" "),
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

fn home() -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(
        std::env::var_os("HOME").context("HOME isn't set")?,
    ))
}

/// `user`'s home directory, as the shell has it.
fn home_of(user: &str) -> Option<String> {
    if !user
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        return None;
    }
    let out = std::process::Command::new("sh")
        .args(["-c", &format!("echo ~{user}")])
        .output()
        .ok()?;
    let home = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!home.starts_with('~') && !home.is_empty()).then_some(home)
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

async fn command(program: &str, args: &[&str]) -> anyhow::Result<()> {
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
    Ok(())
}

async fn uid() -> anyhow::Result<String> {
    let out = tokio::process::Command::new("id")
        .arg("-u")
        .output()
        .await?;
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}

/// Install the service and start it (in place of one there already).
pub async fn install(service: &Service, credentials: Option<PathBuf>) -> anyhow::Result<()> {
    let os = os()?;
    if service.system {
        anyhow::ensure!(uid().await? == "0", "--system installs as root (sudo)");
        system_files(service, credentials)?;
    }
    let file = service.file(os)?;
    let run = service.run(exe()?);
    let text = match os {
        Os::Macos => plist(service, &run, &service.log()?),
        Os::Linux => unit(service, &run),
    };
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = stop(service, os, &file).await;
    std::fs::write(&file, text).with_context(|| format!("writing {}", file.display()))?;
    let path = file.to_string_lossy();
    match (os, service.system) {
        (Os::Macos, false) => {
            let domain = format!("gui/{}", uid().await?);
            command("launchctl", &["bootstrap", &domain, &path]).await?
        }
        (Os::Macos, true) => command("launchctl", &["bootstrap", "system", &path]).await?,
        (Os::Linux, false) => {
            command("systemctl", &["--user", "daemon-reload"]).await?;
            command("systemctl", &["--user", "enable", "--now", UNIT]).await?;
        }
        (Os::Linux, true) => {
            command("systemctl", &["daemon-reload"]).await?;
            command("systemctl", &["enable", "--now", UNIT]).await?;
        }
    }
    println!("Installed {}", file.display());
    match (os, service.system) {
        (Os::Macos, _) => println!("Its log: {}", service.log()?.display()),
        (Os::Linux, false) => {
            println!("Its log: journalctl --user -u {UNIT}");
            println!(
                "It runs while you're logged in; to keep it running after: loginctl enable-linger"
            );
        }
        (Os::Linux, true) => println!("Its log: journalctl -u {UNIT}"),
    }
    Ok(())
}

/// The system's service's credentials (copied from `credentials`, or the installing user's)
/// and config (a new one, unless there's one), the service's user's.
fn system_files(service: &Service, credentials: Option<PathBuf>) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let from = match credentials {
        Some(path) => path,
        // under sudo, the user who ran it
        None => match std::env::var("SUDO_USER").ok().and_then(|u| home_of(&u)) {
            Some(home) => PathBuf::from(home).join(".config/dirigent/credentials.json"),
            None => crate::credentials::path()?,
        },
    };
    let creds = std::fs::read(&from).with_context(|| {
        format!(
            "reading the credentials {} (sign in first: dirigent login; or give --credentials)",
            from.display()
        )
    })?;
    std::fs::create_dir_all(ETC)?;
    let to = Path::new(ETC).join("credentials.json");
    std::fs::write(&to, creds)?;
    std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o600))?;
    let config = Path::new(ETC).join("host.toml");
    if !config.exists() {
        std::fs::write(
            &config,
            "# dirigent host's config (see `dirigent host --help`)\n\
             # url = \"https://dirigent.example.com\"\n\
             # name = \"this-machine\"\n\
             roots = [\"/var/lib/dirigent\"]\n\
             # slots = 2\n\
             # labels = []\n",
        )?;
    }
    std::fs::create_dir_all("/var/lib/dirigent")?;
    if let Some(user) = &service.user {
        for path in [to.as_path(), Path::new("/var/lib/dirigent")] {
            let status = std::process::Command::new("chown")
                .arg(user)
                .arg(path)
                .status()?;
            anyhow::ensure!(status.success(), "chown {user} {}", path.display());
        }
    }
    Ok(())
}

/// Stop the service, if it's there.
async fn stop(service: &Service, os: Os, file: &Path) -> anyhow::Result<()> {
    match (os, service.system) {
        (Os::Macos, false) => {
            let target = format!("gui/{}/{LABEL}", uid().await?);
            command("launchctl", &["bootout", &target]).await
        }
        (Os::Macos, true) => command("launchctl", &["bootout", &format!("system/{LABEL}")]).await,
        (Os::Linux, false) if file.exists() => {
            command("systemctl", &["--user", "disable", "--now", UNIT]).await
        }
        (Os::Linux, true) if file.exists() => {
            command("systemctl", &["disable", "--now", UNIT]).await
        }
        (Os::Linux, _) => Ok(()),
    }
}

/// Stop the service and remove it (the players running go on until their sessions stop).
pub async fn uninstall(service: &Service) -> anyhow::Result<()> {
    let os = os()?;
    let file = service.file(os)?;
    if !file.exists() {
        println!("Not installed ({} isn't there)", file.display());
        return Ok(());
    }
    if let Err(e) = stop(service, os, &file).await {
        eprintln!("stopping it: {e:#}");
    }
    std::fs::remove_file(&file)?;
    if os == Os::Linux {
        let mut args = vec!["daemon-reload"];
        if !service.system {
            args.insert(0, "--user");
        }
        command("systemctl", &args).await?;
    }
    println!("Removed {}", file.display());
    Ok(())
}

/// Whether the service is installed, and running.
pub async fn status(service: &Service) -> anyhow::Result<()> {
    let os = os()?;
    let file = service.file(os)?;
    if !file.exists() {
        println!(
            "Not installed (dirigent host install{})",
            match service.system {
                true => " --system",
                false => "",
            }
        );
        return Ok(());
    }
    println!("Installed: {}", file.display());
    let running = match (os, service.system) {
        (Os::Macos, false) => {
            let target = format!("gui/{}/{LABEL}", uid().await?);
            command("launchctl", &["print", &target]).await.is_ok()
        }
        (Os::Macos, true) => command("launchctl", &["print", &format!("system/{LABEL}")])
            .await
            .is_ok(),
        (Os::Linux, false) => command("systemctl", &["--user", "is-active", "--quiet", UNIT])
            .await
            .is_ok(),
        (Os::Linux, true) => command("systemctl", &["is-active", "--quiet", UNIT])
            .await
            .is_ok(),
    };
    println!("{}", if running { "Running" } else { "Not running" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(system: bool) -> Run {
        Run {
            exe: PathBuf::from("/opt/homebrew/bin/dirigent"),
            args: match system {
                true => vec!["host".into(), "run".into(), "--system".into()],
                false => vec!["host".into(), "run".into()],
            },
            env: vec![("PATH".into(), "/opt/homebrew/bin:/usr/bin".into())],
        }
    }

    #[test]
    fn a_launchd_job_runs_the_host_and_leaves_its_players() {
        let mine = Service {
            system: false,
            user: None,
        };
        let text = plist(
            &mine,
            &run(false),
            Path::new("/Users/a/Library/Logs/dirigent-host.log"),
        );
        assert!(text.contains("<key>Label</key><string>dev.dirigent.host</string>"));
        assert!(text.contains(
            "<array><string>/opt/homebrew/bin/dirigent</string><string>host</string>\
             <string>run</string></array>"
        ));
        assert!(text.contains("<key>PATH</key><string>/opt/homebrew/bin:/usr/bin</string>"));
        assert!(text.contains("<key>AbandonProcessGroup</key><true/>"));
        assert!(!text.contains("UserName"));
        let theirs = Service {
            system: true,
            user: Some("adam".into()),
        };
        let text = plist(&theirs, &run(true), Path::new("/var/log/dirigent-host.log"));
        assert!(text.contains("<key>UserName</key><string>adam</string>"));
        assert!(text.contains("<string>--system</string>"));
    }

    #[test]
    fn a_systemd_unit_runs_the_host_and_leaves_its_players() {
        let mine = Service {
            system: false,
            user: None,
        };
        let text = unit(&mine, &run(false));
        assert!(text.contains("ExecStart=/opt/homebrew/bin/dirigent host run\n"));
        assert!(text.contains("Environment=PATH=/opt/homebrew/bin:/usr/bin\n"));
        assert!(text.contains("KillMode=process\n") && text.contains("WantedBy=default.target"));
        let theirs = Service {
            system: true,
            user: Some("adam".into()),
        };
        let text = unit(&theirs, &run(true));
        assert!(text.contains("User=adam\n") && text.contains("WantedBy=multi-user.target"));
        assert_eq!(quote("a b\"c"), "\"a b\\\"c\"");
    }

    #[test]
    fn the_systems_service_has_its_own_credentials_and_config() {
        let theirs = Service {
            system: true,
            user: None,
        };
        let run = theirs.run(PathBuf::from("/usr/local/bin/dirigent"));
        assert_eq!(run.args, ["host", "run", "--system"]);
        let env: Vec<&str> = run.env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            env,
            ["PATH", "DIRIGENT_CREDENTIALS", "DIRIGENT_HOST_CONFIG"]
        );
        assert_eq!(
            theirs.file(Os::Linux).unwrap(),
            PathBuf::from("/etc/systemd/system/dirigent-host.service")
        );
        assert_eq!(
            theirs.file(Os::Macos).unwrap(),
            PathBuf::from("/Library/LaunchDaemons/dev.dirigent.host.plist")
        );
    }
}
