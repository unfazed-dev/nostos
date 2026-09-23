//! `nostos link` — app-side: scaffold `.nostos/` (config.json + gitignored
//! `local/`) at the app repo root. See ADR-0023 D1/D3. Distinct from the
//! operator `nostos init` (publication + `nostos.toml`).

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Result};
use clap::Args;

use nostos_infra::rules_file::{self, RULES_FILE_NAME};

use crate::config::{Backend, LinkMode, ProjectConfig, DOT_NOSTOS_DIR, LOCAL_DIR};
use crate::direct;
use crate::prompt::prompt_nonempty;

#[derive(Debug, Args)]
pub struct LinkArgs {
    /// `server` (a nostos-server holds the slot) or `direct` (the device syncs
    /// straight from the client's own Postgres — no Nostos server at all).
    #[arg(long, default_value = "server")]
    pub mode: String,
    /// The nostos-server `/sync` WebSocket URL (`ws://` or `wss://`). Prompted
    /// for if omitted. Direct mode derives it from the Supabase URL instead.
    #[arg(long)]
    pub sync_url: Option<String>,
    /// Direct mode: a synced table every authenticated device may read. Repeat
    /// per table. Required for any table with no scope — direct mode refuses to
    /// infer that an unscoped table is public.
    #[arg(long = "public")]
    pub public: Vec<String>,
    /// Direct mode: how long `cairn.prune()` keeps change rows.
    #[arg(long, default_value = direct::DEFAULT_RETENTION)]
    pub retention: String,
    /// Direct mode: also generate the push path, posting to this Edge Function
    /// URL when a change arrives for a scope with no awake device. Omit and no
    /// push objects are generated at all.
    #[arg(long)]
    pub push: Option<String>,
    /// Direct mode, with `--push`: a table whose changes arrive AS the
    /// notification, the only kind iOS shows a user-quit app (ADR-0037 §2b).
    /// Repeat per table. Server mode's `NOSTOS_PUSH_TABLES` grammar:
    /// `table:visible[@/route/{id}]:<title>:<body>` or
    /// `table:action[@/route/{id}]:<category>:<title>:<body>`, `{col}` filled
    /// from the changed row.
    #[arg(long, requires = "push")]
    pub visible: Vec<String>,
    /// Direct mode, with `--push`: also roll it out through the `supabase` CLI
    /// (`supabase login` first) — apply `.nostos/direct.sql` with `pg_net`, mint
    /// the shared secret on both sides, deploy the `cairn-push` Edge Function.
    #[arg(long, requires_all = ["push", "fcm_service_account"])]
    pub deploy: bool,
    /// The Firebase service-account JSON `--deploy` hands the Edge Function as
    /// `FCM_SERVICE_ACCOUNT`. Read, never copied into the repo.
    #[arg(long)]
    pub fcm_service_account: Option<PathBuf>,
    /// Backend kind: `postgres` | `supabase` | `appwrite` (ADR-0023 D4).
    /// Defaults to `postgres` when omitted.
    #[arg(long)]
    pub backend: Option<String>,
    /// Supabase project URL (with `--backend supabase`).
    #[arg(long)]
    pub supabase_url: Option<String>,
    /// Supabase publishable (anon) key (with `--backend supabase`).
    #[arg(long)]
    pub supabase_anon_key: Option<String>,
    /// Human project name (informational).
    #[arg(long, default_value = "nostos-app")]
    pub project: String,
}

/// Run `nostos link`: resolve the sync URL + backend, write `.nostos/config.json`,
/// create the gitignored `.nostos/local/`, and ensure `.gitignore` covers it.
///
/// # Errors
/// [`anyhow::Error`] if the sync URL is not `ws://`/`wss://`, the backend kind
/// is unknown, a required (prompted) value is empty, or any disk write fails.
// `async` by contract: `main.rs` dispatches `run(args, &cwd).await` alongside
// the other subcommands, so the signature must stay async even though the link
// body is pure file IO today. ponytail: drop this allow if link grows a
// reachability probe against `sync_url` (the natural async surface).
#[allow(clippy::unused_async)]
pub async fn run(args: LinkArgs, cwd: &Path) -> Result<()> {
    let mode = match args.mode.as_str() {
        "server" => LinkMode::Server,
        "direct" => LinkMode::Direct,
        other => bail!("unknown --mode `{other}` (expected `server` or `direct`)"),
    };
    if mode == LinkMode::Direct {
        return run_direct(args, cwd);
    }
    let sync_url = match args.sync_url {
        Some(u) => u,
        None => prompt_nonempty("nostos-server /sync URL (ws:// or wss://): ")?,
    };
    if !(sync_url.starts_with("ws://") || sync_url.starts_with("wss://")) {
        bail!(
            "sync URL must start with `ws://` or `wss://` (got `{sync_url}`); \
             `nostos link` needs the nostos-server WebSocket endpoint"
        );
    }

    let backend = resolve_backend(
        args.backend.as_deref(),
        args.supabase_url.as_deref(),
        args.supabase_anon_key.as_deref(),
    )?;

    let config = ProjectConfig {
        mode: LinkMode::Server,
        project: args.project,
        sync_url,
        backend: Some(backend),
    };
    config.save(cwd)?;

    let local_dir = cwd.join(DOT_NOSTOS_DIR).join(LOCAL_DIR);
    std::fs::create_dir_all(&local_dir)?;

    ensure_gitignore_local(cwd)?;

    println!("\u{2713} wrote `.nostos/config.json`");
    println!("\u{2713} created gitignored `.nostos/local/`");
    println!(
        "note: only publishable keys belong in config.json \u{2014} \
         service keys / JWT secrets / DB passwords go in `.env` or `.nostos/local/`"
    );
    println!("next: `nostos pull && nostos gen`");
    Ok(())
}

/// Direct mode: no sync server exists, so the "sync URL" is the Realtime
/// socket the doorbell joins. Everything else in `.nostos/config.json` is
/// unchanged, and the generated SQL lands in `.nostos/direct.sql`.
fn run_direct(args: LinkArgs, cwd: &Path) -> Result<()> {
    if args.sync_url.is_some() {
        bail!(
            "--sync-url is server mode only: direct mode has no nostos-server to \
             point at. The device reaches the database itself, and the socket it \
             opens is the project's own Realtime endpoint."
        );
    }
    let backend = resolve_backend(
        Some(args.backend.as_deref().unwrap_or("supabase")),
        args.supabase_url.as_deref(),
        args.supabase_anon_key.as_deref(),
    )?;
    let Backend::Supabase { url, .. } = &backend else {
        bail!(
            "direct mode requires `--backend supabase`: the generated SQL calls \
             auth.jwt(), realtime.send() and grants to the `authenticated` role, \
             none of which exist on a plain Postgres. Run a nostos-server for those \
             (`--mode server`)."
        );
    };

    let rules_path = cwd.join(RULES_FILE_NAME);
    let rules = rules_file::load(&rules_path)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no {RULES_FILE_NAME} at {} — direct mode generates one trigger per \
             synced table, so it needs the table list first: run `nostos rules init`",
            rules_path.display()
        )
    })?;
    let tables = direct::plan(&rules, &args.public)?;
    let templates = args
        .visible
        .iter()
        .map(|spec| direct::parse_visible(spec))
        .collect::<Result<Vec<_>>>()?;
    if let Some(t) = templates
        .iter()
        .find(|t| !tables.iter().any(|d| d.table == t.table))
    {
        bail!(
            "--visible names `{}`, which is not a synced table: its changes never \
             reach cairn.changes, so it could never push",
            t.table
        );
    }
    let push = args.push.as_ref().map(|endpoint| direct::PushConfig {
        endpoint: endpoint.clone(),
        templates,
        ..direct::PushConfig::default()
    });
    let sql = direct::render_with_push(&tables, &args.retention, push.as_ref());

    let config = ProjectConfig {
        mode: LinkMode::Direct,
        project: args.project,
        sync_url: realtime_url(url),
        backend: Some(backend.clone()),
    };
    config.save(cwd)?;

    let local_dir = cwd.join(DOT_NOSTOS_DIR).join(LOCAL_DIR);
    std::fs::create_dir_all(&local_dir)?;
    ensure_gitignore_local(cwd)?;

    let sql_path = cwd.join(DOT_NOSTOS_DIR).join(direct::OUTPUT_FILE);
    std::fs::write(&sql_path, &sql)?;
    if push.is_some() {
        let dir = cwd.join(PUSH_FN_DIR);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("index.ts"), PUSH_FN)?;
    }

    println!("\u{2713} wrote `.nostos/config.json` (mode: direct)");
    println!(
        "\u{2713} wrote `.nostos/{}` \u{2014} {} table(s), retention {}",
        direct::OUTPUT_FILE,
        tables.len(),
        args.retention
    );
    if push.is_some() {
        println!("\u{2713} wrote `{PUSH_FN_DIR}/index.ts`");
    }
    if let (true, Some(service_account)) = (args.deploy, &args.fcm_service_account) {
        deploy("supabase", cwd, &project_ref(url)?, &sql, service_account)?;
        println!(
            "\u{2713} applied `.nostos/{}`, set the push secret, deployed cairn-push",
            direct::OUTPUT_FILE
        );
        println!("next: turn OFF \"Allow public access\" in the project's Realtime settings,");
    } else {
        println!(
            "next: apply it \u{2014} `psql \"$DATABASE_URL\" -f .nostos/{}`",
            direct::OUTPUT_FILE
        );
        println!("      then turn OFF \"Allow public access\" in the project's Realtime settings,");
        if push.is_some() {
            println!(
                "      push is included \u{2014} rerun with `--deploy --fcm-service-account \
                 <json>` to roll it out, or by hand: `create extension if not exists \
                 pg_net;`, set `cairn.push_config.secret`, deploy {PUSH_FN_DIR}."
            );
        }
    }
    println!("      then `nostos doctor --mode direct` to check it landed.");
    Ok(())
}

/// Where `--push` writes the Edge Function, the layout `supabase functions
/// deploy` reads.
const PUSH_FN_DIR: &str = "supabase/functions/cairn-push";

/// The Edge Function source, compiled in so an app repo gets the version its
/// SQL was generated against — the trigger's request body is their contract.
const PUSH_FN: &str = include_str!("../../../../supabase/functions/cairn-push/index.ts");

/// `https://<ref>.supabase.co` -> `<ref>`. `--deploy` drives a hosted project
/// through the Management API, so a local or self-hosted URL is refused rather
/// than guessed at.
fn project_ref(url: &str) -> Result<String> {
    url.trim_end_matches('/')
        .strip_prefix("https://")
        .and_then(|host| host.strip_suffix(".supabase.co"))
        .filter(|r| !r.is_empty() && !r.contains(['.', '/']))
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "--deploy needs a hosted project (https://<ref>.supabase.co), got `{url}`"
            )
        })
}

/// `--deploy`: the push rollout, through the `supabase` CLI (`bin`, a parameter
/// so the test can stand a recorder in for it).
///
/// The shared secret is minted here and written to both sides in one run, so
/// they cannot drift; a step that fails leaves them apart until the next
/// `--deploy`, which re-mints both. Secrets travel in 0600 files under the
/// gitignored `.nostos/local/`, never in argv (visible to `ps`), and the files
/// are removed whether or not the rollout succeeded.
fn deploy(
    bin: &str,
    cwd: &Path,
    project_ref: &str,
    sql: &str,
    service_account: &Path,
) -> Result<()> {
    use rand::RngCore;

    let sa: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(service_account)?)?;
    let sa_line = serde_json::to_string(&sa)?;
    if ["project_id", "client_email", "private_key"]
        .iter()
        .any(|k| sa.get(k).is_none())
        || sa_line.contains('\'')
    {
        bail!(
            "{} is not a Firebase service-account JSON (needs project_id, client_email, private_key)",
            service_account.display()
        );
    }
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    let secret = bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    });

    let local = cwd.join(DOT_NOSTOS_DIR).join(LOCAL_DIR);
    let sql_file = local.join("deploy.sql");
    let env_file = local.join("nostos-push.env");
    let result = (|| {
        write_private(
            &sql_file,
            &format!(
                "create extension if not exists pg_net;\n{sql}\n\
                 update cairn.push_config set secret = '{secret}' where id = 1;\n"
            ),
        )?;
        // Single-quoted: the dotenv parser keeps the key's `\n` escapes as-is.
        write_private(
            &env_file,
            &format!("NOSTOS_PUSH_SECRET={secret}\nFCM_SERVICE_ACCOUNT='{sa_line}'\n"),
        )?;
        let sql_arg = sql_file.to_string_lossy();
        let env_arg = env_file.to_string_lossy();
        for args in [
            &[
                "db",
                "query",
                "--linked",
                "--project-ref",
                project_ref,
                "-f",
                &sql_arg,
            ][..],
            &[
                "secrets",
                "set",
                "--project-ref",
                project_ref,
                "--env-file",
                &env_arg,
            ],
            &[
                "functions",
                "deploy",
                "cairn-push",
                "--project-ref",
                project_ref,
                "--no-verify-jwt",
                "--use-api",
            ],
        ] {
            let status = Command::new(bin)
                .args(args)
                .current_dir(cwd)
                .stdout(Stdio::null())
                .status()?;
            if !status.success() {
                bail!("`supabase {}` failed ({status})", args[..2].join(" "));
            }
        }
        Ok(())
    })();
    let _ = std::fs::remove_file(&sql_file);
    let _ = std::fs::remove_file(&env_file);
    result
}

fn write_private(path: &Path, content: &str) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(content.as_bytes())?;
    Ok(())
}

/// `https://xyz.supabase.co` -> `wss://xyz.supabase.co/realtime/v1/websocket`,
/// the endpoint `nostos_client::doorbell` joins.
fn realtime_url(project_url: &str) -> String {
    let host = project_url
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let scheme = if project_url.starts_with("http://") {
        "ws"
    } else {
        "wss"
    };
    format!("{scheme}://{host}/realtime/v1/websocket")
}

fn resolve_backend(
    kind: Option<&str>,
    supabase_url: Option<&str>,
    supabase_anon_key: Option<&str>,
) -> Result<Backend> {
    match kind.unwrap_or("postgres") {
        "postgres" => Ok(Backend::Postgres),
        "supabase" => {
            let url = match supabase_url {
                Some(u) => u.to_string(),
                None => prompt_nonempty("Supabase project URL (https://...supabase.co): ")?,
            };
            let anon_key = match supabase_anon_key {
                Some(k) => k.to_string(),
                None => prompt_nonempty("Supabase publishable (anon) key: ")?,
            };
            Ok(Backend::Supabase { url, anon_key })
        }
        "appwrite" => bail!("appwrite backend is post-v1 (ADR-0023 D4)"),
        other => {
            bail!("unknown backend `{other}`; valid options: `postgres`, `supabase`, `appwrite`")
        }
    }
}

fn ensure_gitignore_local(cwd: &Path) -> Result<()> {
    const ENTRY: &str = ".nostos/local/";
    let path = cwd.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == ENTRY) {
        return Ok(());
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(ENTRY);
    content.push('\n');
    std::fs::write(&path, content)?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A `supabase` stand-in: records its argv and the files it was handed,
    /// then exits with `code`.
    fn fake_supabase(dir: &Path, code: u8) -> String {
        let bin = dir.join("supabase");
        let log = dir.join("log");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\necho \"ARGS $*\" >> {log}\nprev=\nfor a in \"$@\"; do\n  \
                 case \"$prev\" in -f|--env-file) cat \"$a\" >> {log};; esac\n  prev=$a\ndone\n\
                 exit {code}\n",
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin.to_string_lossy().into_owned()
    }

    fn app_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nostos-link-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join(DOT_NOSTOS_DIR).join(LOCAL_DIR)).unwrap();
        std::fs::write(
            dir.join("sa.json"),
            r#"{"project_id":"p","client_email":"e@p","private_key":"-----BEGIN-----\nk\n-----END-----\n"}"#,
        )
        .unwrap();
        dir
    }

    #[test]
    fn deploy_sets_one_secret_on_both_sides_and_leaves_no_secret_behind() {
        let dir = app_dir();
        let bin = fake_supabase(&dir, 0);
        deploy(&bin, &dir, "abc", "select 1;", &dir.join("sa.json")).unwrap();

        let log = std::fs::read_to_string(dir.join("log")).unwrap();
        let secret = log
            .split("set secret = '")
            .nth(1)
            .and_then(|rest| rest.get(..64))
            .unwrap();
        assert!(secret.chars().all(|c| c.is_ascii_hexdigit()), "{secret}");
        assert!(log.contains("create extension if not exists pg_net;\nselect 1;"));
        assert!(log.contains(&format!("NOSTOS_PUSH_SECRET={secret}\n")));
        assert!(log.contains(r#"FCM_SERVICE_ACCOUNT='{"client_email":"e@p","#));
        let argv: Vec<&str> = log.lines().filter(|l| l.starts_with("ARGS ")).collect();
        assert_eq!(argv.len(), 3, "{log}");
        assert!(argv[0].starts_with("ARGS db query --linked --project-ref abc -f "));
        assert!(argv[1].starts_with("ARGS secrets set --project-ref abc --env-file "));
        assert_eq!(
            argv[2],
            "ARGS functions deploy cairn-push --project-ref abc --no-verify-jwt --use-api"
        );
        assert!(
            argv.iter().all(|l| !l.contains(secret)),
            "the secret stays out of argv"
        );

        // A failed rollout still takes its secrets with it.
        let failing = fake_supabase(&dir, 1);
        assert!(deploy(&failing, &dir, "abc", "select 1;", &dir.join("sa.json")).is_err());
        let local = dir.join(DOT_NOSTOS_DIR).join(LOCAL_DIR);
        assert_eq!(
            std::fs::read_dir(&local).unwrap().count(),
            0,
            "no secret file left"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deploy_targets_only_a_hosted_project() {
        assert_eq!(project_ref("https://abc.supabase.co/").unwrap(), "abc");
        for url in [
            "http://127.0.0.1:54321",
            "https://db.example.com",
            "https://.supabase.co",
        ] {
            assert!(project_ref(url).is_err(), "{url}");
        }
    }
}
