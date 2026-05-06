use httpmock::{Method, MockServer};
use tempfile::NamedTempFile;
use toggl_jira_sync::{
    db::{Database, NewJiraIssueSiteCache},
    jira::JiraClient,
    sync::resolver::{IssueSiteResolutionError, IssueSiteResolver, ResolverSite},
};

const EMAIL: &str = "sync@example.test";
const TOKEN: &str = "fake-jira-token-do-not-log";

fn temp_database() -> (NamedTempFile, Database) {
    let file = NamedTempFile::new().expect("temp sqlite file should be created");
    let database = Database::open(file.path()).expect("temp sqlite database should open");
    database.run_migrations().expect("migrations should run");
    (file, database)
}

fn site(key: &str, server: &MockServer) -> ResolverSite {
    ResolverSite {
        key: key.to_owned(),
        client: JiraClient::from_credentials(server.base_url(), EMAIL.to_owned(), TOKEN.to_owned()),
    }
}

#[tokio::test]
async fn resolver_returns_cached_site_without_jira_discovery_gets() {
    let (_file, db) = temp_database();
    db.upsert_jira_issue_site_cache(&NewJiraIssueSiteCache {
        issue_key: "SAB-123",
        jira_site_key: "cached-site",
        source: "discovery",
    })
    .expect("seed cache");
    let first = MockServer::start();
    let second = MockServer::start();
    let first_lookup = first.mock(|when, then| {
        when.method(Method::GET).path("/rest/api/3/issue/SAB-123");
        then.status(200)
            .json_body(serde_json::json!({ "key": "SAB-123" }));
    });
    let second_lookup = second.mock(|when, then| {
        when.method(Method::GET).path("/rest/api/3/issue/SAB-123");
        then.status(200)
            .json_body(serde_json::json!({ "key": "SAB-123" }));
    });

    let resolver =
        IssueSiteResolver::new(&db, vec![site("first", &first), site("second", &second)]);

    let resolved = resolver
        .resolve_issue_key("SAB-123")
        .await
        .expect("cache hit should resolve");

    assert_eq!(resolved.issue_key, "SAB-123");
    assert_eq!(resolved.jira_site_key, "cached-site");
    assert_eq!(first_lookup.hits(), 0);
    assert_eq!(second_lookup.hits(), 0);
}

#[tokio::test]
async fn resolver_discovers_exactly_one_site_and_caches_it() {
    let (_file, db) = temp_database();
    let missing = MockServer::start();
    let matching = MockServer::start();
    missing.mock(|when, then| {
        when.method(Method::GET)
            .path("/rest/api/3/issue/BLOGIC-456");
        then.status(404).json_body(serde_json::json!({}));
    });
    let lookup = matching.mock(|when, then| {
        when.method(Method::GET)
            .path("/rest/api/3/issue/BLOGIC-456");
        then.status(200)
            .json_body(serde_json::json!({ "key": "BLOGIC-456" }));
    });

    let resolver = IssueSiteResolver::new(
        &db,
        vec![site("sabservis", &missing), site("blogic", &matching)],
    );

    let resolved = resolver
        .resolve_issue_key("BLOGIC-456")
        .await
        .expect("one matching site should resolve");

    assert_eq!(resolved.jira_site_key, "blogic");
    lookup.assert();
    let cached = db
        .get_jira_issue_site_cache("BLOGIC-456")
        .expect("cache lookup should succeed")
        .expect("discovery should cache the mapping");
    assert_eq!(cached.jira_site_key, "blogic");
    assert_eq!(cached.source, "discovery");
}

#[tokio::test]
async fn resolver_reports_zero_or_multiple_site_matches() {
    let (_file, db) = temp_database();
    let first = MockServer::start();
    let second = MockServer::start();
    for server in [&first, &second] {
        server.mock(|when, then| {
            when.method(Method::GET).path("/rest/api/3/issue/MISSING-1");
            then.status(404).json_body(serde_json::json!({}));
        });
        server.mock(|when, then| {
            when.method(Method::GET).path("/rest/api/3/issue/AMB-1");
            then.status(200)
                .json_body(serde_json::json!({ "key": "AMB-1" }));
        });
    }

    let resolver =
        IssueSiteResolver::new(&db, vec![site("first", &first), site("second", &second)]);

    let missing = resolver
        .resolve_issue_key("MISSING-1")
        .await
        .expect_err("zero matches should be an error");
    assert!(matches!(
        missing,
        IssueSiteResolutionError::NoMatchingSite { .. }
    ));

    let ambiguous = resolver
        .resolve_issue_key("AMB-1")
        .await
        .expect_err("multiple matches should be an error");
    assert!(matches!(
        ambiguous,
        IssueSiteResolutionError::MultipleMatchingSites { .. }
    ));
}
