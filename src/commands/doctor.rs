use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};

use crate::{
    cli::DoctorArgs,
    commands::config::{
        load_default_credentials, load_isolated_credentials_from_path, resolve_config_path,
        resolve_db_path, LocalCredentials,
    },
    config::AppConfig,
    db::Database,
    local_api::LocalServer,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoctorCommandReport {
    pub lines: Vec<String>,
    pub failures: Vec<String>,
}

impl DoctorCommandReport {
    pub fn print(&self) {
        for line in &self.lines {
            println!("{line}");
        }
    }
}

pub async fn run(args: DoctorArgs) -> anyhow::Result<()> {
    let online = args.online;
    let server = LocalServer::start(args.paths.clone(), None, 200).await?;
    let report = server.client().doctor_command(online).await?;
    report.print();
    if report.failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(report.failures.join("; ")))
    }
}

pub(crate) async fn doctor_report(
    args: DoctorArgs,
    credentials: Option<LocalCredentials>,
) -> anyhow::Result<DoctorCommandReport> {
    let uses_default_config = args.paths.config.is_none();
    let config_path = resolve_config_path(args.paths.config)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    let credentials = if let Some(credentials) = credentials {
        credentials
    } else if uses_default_config {
        load_default_credentials()?
    } else {
        LocalCredentials::process_env()
    };
    let db_path = resolve_db_path(
        args.paths.db,
        &config_path,
        config.runtime.sqlite_path.as_deref(),
        "doctor",
    )?;

    let mut failures = Vec::new();
    let mut lines = Vec::new();

    lines.push("doctor: read-only health checks".to_owned());
    lines.push(format!("config: ok ({})", config_path.display()));

    match Database::open(&db_path) {
        Ok(database) => {
            lines.push(format!("database: ok ({})", db_path.display()));
            match database.run_migrations() {
                Ok(()) => lines.push("migrations: ok".to_owned()),
                Err(error) => {
                    lines.push(format!("migrations: failed ({error})"));
                    failures.push("DB migrations failed".to_owned());
                }
            }
        }
        Err(error) => {
            lines.push(format!("database: failed ({})", db_path.display()));
            failures.push(format!("failed to open DB: {error}"));
        }
    }

    check_env_var(
        &credentials,
        &config.toggl.api_token_env,
        &mut failures,
        &mut lines,
    );
    for site in config.enabled_jira_sites() {
        check_env_var(&credentials, &site.email_env, &mut failures, &mut lines);
        check_env_var(&credentials, &site.api_token_env, &mut failures, &mut lines);
    }

    lines.push("target sites:".to_owned());
    lines.push(format!("  {}: ok", config.toggl.base_url));
    for site in config.enabled_jira_sites() {
        lines.push(format!("  {}: ok", site.base_url));
    }

    if args.online {
        run_online_checks(&config, &mut failures, &mut lines).await;
    } else {
        lines.push(
            "online checks: skipped (pass --online to enable non-secret connectivity checks)"
                .to_owned(),
        );
    }

    if failures.is_empty() {
        lines.push("doctor: ok".to_owned());
    } else {
        lines.push("doctor: failed".to_owned());
    }

    Ok(DoctorCommandReport { lines, failures })
}

pub(crate) async fn doctor_report_with_isolated_credentials(
    args: DoctorArgs,
    credentials_path: std::path::PathBuf,
) -> anyhow::Result<DoctorCommandReport> {
    doctor_report(
        args,
        Some(load_isolated_credentials_from_path(&credentials_path)?),
    )
    .await
}

fn check_env_var(
    credentials: &LocalCredentials,
    name: &str,
    failures: &mut Vec<String>,
    lines: &mut Vec<String>,
) {
    if credentials.contains_secret(name) {
        lines.push(format!("{name}: set"));
    } else {
        lines.push(format!("{name}: missing"));
        failures.push(format!("missing env var {name}"));
    }
}

async fn run_online_checks(
    config: &AppConfig,
    failures: &mut Vec<String>,
    lines: &mut Vec<String>,
) {
    let Ok(client) = reqwest::Client::builder().build() else {
        lines.push("online checks: failed to build HTTP client".to_owned());
        failures.push("failed to build online check HTTP client".to_owned());
        return;
    };

    check_url_online(&client, "toggl", &config.toggl.base_url, failures, lines).await;
    for site in config.enabled_jira_sites() {
        check_url_online(&client, &site.key, &site.base_url, failures, lines).await;
    }
}

async fn check_url_online(
    client: &reqwest::Client,
    label: &str,
    url: &str,
    failures: &mut Vec<String>,
    lines: &mut Vec<String>,
) {
    match client.head(url).send().await {
        Ok(response) if response.status().is_success() || response.status().is_redirection() => {
            lines.push(format!("online {label}: ok ({})", response.status()));
        }
        Ok(response) => {
            lines.push(format!("online {label}: failed ({})", response.status()));
            failures.push(format!("online check failed for {label}"));
        }
        Err(error) => {
            lines.push(format!("online {label}: failed ({error})"));
            failures.push(format!("online check failed for {label}"));
        }
    }
}
