//! `nostos dev` — run nostos-server locally using `nostos.toml` + `.env`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::config::{NostosConfig, DEFAULT_FILE_NAME};
use crate::dotenv;

pub async fn run(cwd: &Path) -> Result<()> {
    let cfg = NostosConfig::load(&cwd.join(DEFAULT_FILE_NAME))?;

    let env_path = cwd.join(".env");
    let dotenv_vars = dotenv::read(&env_path);
    let pg_url = dotenv_vars.get(&cfg.db.url_env).cloned().with_context(|| {
        format!(
            "{} is not set in {} — run `nostos init` first",
            cfg.db.url_env,
            env_path.display()
        )
    })?;
    let jwt_secret = dotenv_vars
        .get("NOSTOS_SUPABASE_JWT_SECRET")
        .map(String::as_str);
    let mut env_pairs = cfg.server_env(&pg_url, jwt_secret);
    push_rules_file_env(&mut env_pairs, cwd);

    let binary = locate_server_binary();
    println!("Starting nostos-server ({})...", binary.describe());
    print_startup_banner(&cfg);

    let mut cmd = binary.command();
    for (k, v) in &env_pairs {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut child = cmd.spawn().context("spawning nostos-server")?;

    // Ctrl-C in a terminal is delivered to the whole foreground process
    // group — nostos-server's own SIGINT/SIGTERM handler
    // (`crates/nostos-server/src/main.rs::shutdown_signal`) will see it too
    // and drain gracefully. We just wait for it to actually exit so we don't
    // return (and let the terminal reclaim the prompt) before it's done.
    tokio::select! {
        () = ctrl_c_or_pending() => {
            println!("\nreceived Ctrl-C, waiting for nostos-server to shut down...");
        }
        status = child.wait() => {
            let status = status.context("waiting for nostos-server")?;
            anyhow::bail!("nostos-server exited unexpectedly: {status}");
        }
    }
    let status = child
        .wait()
        .await
        .context("waiting for nostos-server to exit")?;
    println!("nostos-server exited: {status}");
    Ok(())
}

/// `NostosConfig::server_env` knows nothing about the project directory, so
/// the `nostos_rules.toml` default on the server's `Config` resolves *by
/// coincidence* (the child inherits our cwd). Make the path explicit instead
/// of relying on that. Not folded into `server_env` itself — `deploy.rs`
/// conceptually shares that helper's shape, where the rules file ships
/// inside the image and the server's own relative default is correct.
fn push_rules_file_env(env_pairs: &mut Vec<(String, String)>, cwd: &Path) {
    env_pairs.push((
        "NOSTOS_RULES_FILE".to_string(),
        cwd.join("nostos_rules.toml").display().to_string(),
    ));
}

async fn ctrl_c_or_pending() {
    if tokio::signal::ctrl_c().await.is_err() {
        std::future::pending::<()>().await;
    }
}

enum ServerBinary {
    /// A `nostos-server` binary co-installed next to this `nostos` binary
    /// (the release-artifact case, W6).
    Path(PathBuf),
    /// Dev fallback: build+run nostos-server from the workspace.
    ///
    /// ponytail: this always spawns a *child process*, whichever branch —
    /// there's no in-process embed. Upgrade path once distribution matters
    /// (W6): either vendor the nostos-server binary into the `nostos` release
    /// artifact (true single static binary), or give nostos-server a library
    /// entry point (`nostos_server::run(Config)`) this crate can call
    /// in-process, cutting the process-spawn + env-var handoff entirely.
    Cargo,
}

impl ServerBinary {
    fn describe(&self) -> String {
        match self {
            Self::Path(p) => p.display().to_string(),
            Self::Cargo => "cargo run -p nostos-server".to_string(),
        }
    }

    fn command(&self) -> Command {
        match self {
            Self::Path(p) => Command::new(p),
            Self::Cargo => {
                let mut c = Command::new("cargo");
                c.args([
                    "run",
                    "--quiet",
                    "-p",
                    "nostos-server",
                    "--features",
                    "pg",
                    "--",
                ]);
                c
            }
        }
    }
}

fn locate_server_binary() -> ServerBinary {
    let sibling_name = if cfg!(windows) {
        "nostos-server.exe"
    } else {
        "nostos-server"
    };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(sibling_name);
            if candidate.is_file() {
                return ServerBinary::Path(candidate);
            }
        }
    }
    ServerBinary::Cargo
}

fn print_startup_banner(cfg: &NostosConfig) {
    let host = if cfg.server.bind.starts_with("0.0.0.0") {
        "localhost"
    } else {
        cfg.server.bind.split(':').next().unwrap_or("localhost")
    };
    let port = cfg.server.bind.rsplit(':').next().unwrap_or("8800");
    let ws_url = format!("ws://{host}:{port}{}", cfg.server.ws_path);
    println!("  ws URL: {ws_url}");
    println!();
    println!("  Flutter snippet:");
    println!("    final nostos = await Nostos.connect(");
    println!("      url: '{ws_url}',");
    println!("      token: supabaseSession.accessToken,");
    println!("    );");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_env_includes_absolute_rules_file_path() {
        let mut env_pairs = Vec::new();
        let cwd = Path::new("/some/project/dir");
        push_rules_file_env(&mut env_pairs, cwd);
        let (_k, v) = env_pairs
            .iter()
            .find(|(k, _)| k == "NOSTOS_RULES_FILE")
            .expect("NOSTOS_RULES_FILE present");
        assert_eq!(v, &cwd.join("nostos_rules.toml").display().to_string());
        assert!(Path::new(v).is_absolute());
    }
}
