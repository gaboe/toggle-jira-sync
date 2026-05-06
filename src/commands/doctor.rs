use anyhow::{anyhow, Context};

use crate::{
    cli::DoctorArgs,
    commands::config::{load_default_credentials_into_env, resolve_config_path, resolve_db_path},
    config::AppConfig,
    db::Database,
};

pub async fn run(args: DoctorArgs) -> anyhow::Result<()> {
    let uses_default_config = args.paths.config.is_none();
    let config_path = resolve_config_path(args.paths.config)?;
    let config = AppConfig::from_path(&config_path)
        .with_context(|| format!("failed to load config {}", config_path.display()))?;
    if uses_default_config {
        load_default_credentials_into_env()?;
    }
    let db_path = resolve_db_path(
        args.paths.db,
        &config_path,
        config.runtime.sqlite_path.as_deref(),
        "doctor",
    )?;

    let mut failures = Vec::new();

    println!("doctor: read-only health checks");
    println!("config: ok ({})", config_path.display());

    match Database::open(&db_path) {
        Ok(database) => {
            println!("database: ok ({})", db_path.display());
            match database.run_migrations() {
                Ok(()) => println!("migrations: ok"),
                Err(error) => {
                    println!("migrations: failed ({error})");
                    failures.push("DB migrations failed".to_owned());
                }
            }
        }
        Err(error) => {
            println!("database: failed ({})", db_path.display());
            failures.push(format!("failed to open DB: {error}"));
        }
    }

    check_env_var(&config.toggl.api_token_env, &mut failures);
    for site in config.enabled_jira_sites() {
        check_env_var(&site.email_env, &mut failures);
        check_env_var(&site.api_token_env, &mut failures);
    }

    println!("target sites:");
    println!("  {}: ok", config.toggl.base_url);
    for site in config.enabled_jira_sites() {
        println!("  {}: ok", site.base_url);
    }

    if args.online {
        run_online_checks(&config, &mut failures).await;
    } else {
        println!("online checks: skipped (pass --online to enable non-secret connectivity checks)");
    }

    if failures.is_empty() {
        println!("doctor: ok");
        Ok(())
    } else {
        println!("doctor: failed");
        Err(anyhow!(failures.join("; ")))
    }
}

fn check_env_var(name: &str, failures: &mut Vec<String>) {
    if std::env::var_os(name).is_some() {
        println!("{name}: set");
    } else {
        println!("{name}: missing");
        failures.push(format!("missing env var {name}"));
    }
}

async fn run_online_checks(config: &AppConfig, failures: &mut Vec<String>) {
    let Ok(client) = reqwest::Client::builder().build() else {
        println!("online checks: failed to build HTTP client");
        failures.push("failed to build online check HTTP client".to_owned());
        return;
    };

    check_url_online(&client, "toggl", &config.toggl.base_url, failures).await;
    for site in config.enabled_jira_sites() {
        check_url_online(&client, &site.key, &site.base_url, failures).await;
    }
}

async fn check_url_online(
    client: &reqwest::Client,
    label: &str,
    url: &str,
    failures: &mut Vec<String>,
) {
    match client.head(url).send().await {
        Ok(response) if response.status().is_success() || response.status().is_redirection() => {
            println!("online {label}: ok ({})", response.status());
        }
        Ok(response) => {
            println!("online {label}: failed ({})", response.status());
            failures.push(format!("online check failed for {label}"));
        }
        Err(error) => {
            println!("online {label}: failed ({error})");
            failures.push(format!("online check failed for {label}"));
        }
    }
}
