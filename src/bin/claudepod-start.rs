use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(disable_version_flag = true, trailing_var_arg = true)]
struct Args {
    /// Start a login shell instead of the default agent mode.
    #[arg(short = 's')]
    shell: bool,

    /// Mount path, or host:guest volume spec, into the guest.
    #[arg(short = 'v', value_name = "SPEC")]
    extra_volumes: Vec<OsString>,

    /// Command to run inside the guest.
    #[arg(value_name = "COMMAND", num_args = 0.., allow_hyphen_values = true)]
    command: Vec<OsString>,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let command_name = command_name()?;
    let username = compile_env("CLAUDEPOD_USERNAME", option_env!("CLAUDEPOD_USERNAME"))?;
    let image = compile_env("CLAUDEPOD_IMAGE", option_env!("CLAUDEPOD_IMAGE"))?;
    let podman = compile_env("CLAUDEPOD_PODMAN", option_env!("CLAUDEPOD_PODMAN"))?;

    let mode = if args.shell {
        "shell"
    } else {
        default_mode_from_command_name(&command_name)?
    };

    let state_dir = state_dir()?;
    let home_dir = state_dir.join("home");
    std::fs::create_dir_all(&home_dir)
        .with_context(|| format!("failed to create {}", home_dir.display()))?;

    let src_root = src_root()?;
    let project_dir = std::env::current_dir().context("failed to get current directory")?;
    let (guest_path, need_project_share) = guest_project_path(&project_dir, &src_root, username)?;

    load_image(image, podman)?;

    let mut volumes = vec![
        OsString::from("/nix/store:/nix/store:ro"),
        OsString::from("/nix/var/nix/db:/nix/.host-nix/nix/var/nix/db:ro"),
        OsString::from(format!("{}:/home/{username}", home_dir.display())),
        OsString::from(format!("{}:/home/{username}/src", src_root.display())),
    ];
    if need_project_share {
        volumes.push(OsString::from(format!(
            "{}:{guest_path}",
            project_dir.display()
        )));
    }
    for spec in args.extra_volumes {
        if spec.to_string_lossy().contains(':') {
            volumes.push(spec);
        } else {
            volumes.push(OsString::from(format!(
                "{}:{}",
                spec.to_string_lossy(),
                spec.to_string_lossy()
            )));
        }
    }

    let mut env_names = std::env::vars_os()
        .map(|(name, _value)| name)
        .filter(|name| name.to_string_lossy().starts_with("CLAUDE_CODE_"))
        .collect::<Vec<_>>();
    env_names.sort();
    if std::env::var_os("MAX_THINKING_TOKENS").is_some_and(|value| !value.is_empty()) {
        env_names.push(OsString::from("MAX_THINKING_TOKENS"));
    }

    println!("Starting {command_name}...");
    println!("  Host path: {}", project_dir.display());
    println!("  Guest path: {guest_path}");
    println!();

    let mut command = Command::new(podman);
    command.args([
        "run",
        "--rm",
        "-it",
        "--userns=keep-id:uid=1000,gid=100",
        "--user",
        "0:0",
        "--cap-add=SYS_ADMIN",
        "--cap-add=NET_ADMIN",
        "--cap-add=NET_RAW",
        "--cap-add=SYS_PTRACE",
        "--device",
        "/dev/fuse",
        "--systemd=always",
        "--no-hostname",
        "--no-hosts",
        "--dns=none",
        "--pids-limit=16384",
        "--security-opt",
        "unmask=ALL",
    ]);
    for volume in volumes {
        command.arg("-v").arg(volume);
    }
    for name in env_names {
        command.arg("-e").arg(name);
    }
    command
        .arg("-e")
        .arg(format!("CLAUDEPOD_PROJECT_PATH={guest_path}"))
        .arg("-e")
        .arg(format!("CLAUDEPOD_MODE={mode}"))
        .arg("-e")
        .arg(format!("CLAUDEPOD_HAS_PROJECT={need_project_share}"))
        .arg("claudepod:latest")
        .args(args.command);

    Err(command.exec()).context("failed to exec podman")
}

fn compile_env(name: &'static str, value: Option<&'static str>) -> Result<&'static str> {
    value
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("{name} was not compiled into this binary"))
}

fn command_name() -> Result<String> {
    let argv0 = std::env::args_os()
        .next()
        .ok_or_else(|| anyhow!("argv[0] is missing"))?;
    Path::new(&argv0)
        .file_name()
        .ok_or_else(|| {
            anyhow!(
                "failed to determine command name from {}",
                Path::new(&argv0).display()
            )
        })
        .map(|name| name.to_string_lossy().into_owned())
}

fn default_mode_from_command_name(command_name: &str) -> Result<&'static str> {
    match command_name {
        "claudepod" => Ok("claude"),
        "gptpod" => Ok("codex"),
        other => bail!("unknown command name {other:?}; expected claudepod or gptpod"),
    }
}

fn state_dir() -> Result<PathBuf> {
    xdg::BaseDirectories::with_prefix("claudepod")
        .get_data_home()
        .ok_or_else(|| anyhow!("HOME is not set and XDG_DATA_HOME is unavailable"))
}

fn src_root() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join("src"))
}

fn guest_project_path(
    project_dir: &Path,
    src_root: &Path,
    username: &str,
) -> Result<(String, bool)> {
    if let Ok(rel_path) = project_dir.strip_prefix(src_root) {
        let guest_path = if rel_path.as_os_str().is_empty() {
            format!("/home/{username}/src")
        } else {
            format!("/home/{username}/src/{}", rel_path.display())
        };
        return Ok((guest_path, false));
    }

    let project_name = project_dir
        .file_name()
        .ok_or_else(|| anyhow!("failed to determine project directory name"))?
        .to_string_lossy();
    Ok((format!("/projects/{project_name}"), true))
}

fn load_image(image: &str, podman: &str) -> Result<()> {
    let mut image_stream = Command::new(image)
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start image stream {image}"))?;
    let image_stdout = image_stream
        .stdout
        .take()
        .ok_or_else(|| anyhow!("failed to capture image stream stdout"))?;

    let mut podman_load = Command::new(podman)
        .args(["load", "-q"])
        .stdin(Stdio::from(image_stdout))
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start {podman} load"))?;

    let podman_status = podman_load
        .wait()
        .context("failed to wait for podman load")?;
    let image_status = image_stream
        .wait()
        .context("failed to wait for image stream")?;

    if !podman_status.success() {
        bail!("podman load failed with {podman_status}");
    }
    if !image_status.success() {
        bail!("image stream failed with {image_status}");
    }

    Ok(())
}
