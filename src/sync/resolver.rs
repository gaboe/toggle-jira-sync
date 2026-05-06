use std::fmt;

use crate::{
    db::{Database, DbError, NewJiraIssueSiteCache},
    jira::{JiraClient, JiraError},
    sync::planner::IssueSiteMapping,
};

#[derive(Debug, Clone)]
pub struct ResolverSite {
    pub key: String,
    pub client: JiraClient,
}

#[derive(Debug)]
pub struct IssueSiteResolver<'a> {
    database: &'a Database,
    sites: Vec<ResolverSite>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIssueSite {
    pub issue_key: String,
    pub jira_site_key: String,
}

#[derive(Debug)]
pub enum IssueSiteResolutionError {
    Db(DbError),
    Jira {
        site_key: String,
        issue_key: String,
        source: JiraError,
    },
    NoMatchingSite {
        issue_key: String,
    },
    MultipleMatchingSites {
        issue_key: String,
        site_keys: Vec<String>,
    },
}

impl fmt::Display for IssueSiteResolutionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Db(error) => write!(formatter, "{error}"),
            Self::Jira {
                site_key,
                issue_key,
                source,
            } => write!(
                formatter,
                "failed to discover Jira issue {issue_key} on site {site_key}: {source}"
            ),
            Self::NoMatchingSite { issue_key } => {
                write!(
                    formatter,
                    "Jira issue {issue_key} was not found on any enabled site"
                )
            }
            Self::MultipleMatchingSites {
                issue_key,
                site_keys,
            } => write!(
                formatter,
                "Jira issue {issue_key} exists on multiple enabled sites: {}",
                site_keys.join(", ")
            ),
        }
    }
}

impl std::error::Error for IssueSiteResolutionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(error) => Some(error),
            Self::Jira { source, .. } => Some(source),
            Self::NoMatchingSite { .. } | Self::MultipleMatchingSites { .. } => None,
        }
    }
}

impl From<DbError> for IssueSiteResolutionError {
    fn from(error: DbError) -> Self {
        Self::Db(error)
    }
}

impl<'a> IssueSiteResolver<'a> {
    pub fn new(database: &'a Database, sites: Vec<ResolverSite>) -> Self {
        Self { database, sites }
    }

    pub async fn resolve_issue_key(
        &self,
        issue_key: &str,
    ) -> Result<ResolvedIssueSite, IssueSiteResolutionError> {
        if let Some(cached) = self.database.get_jira_issue_site_cache(issue_key)? {
            return Ok(ResolvedIssueSite {
                issue_key: cached.issue_key,
                jira_site_key: cached.jira_site_key,
            });
        }

        let mut matches = Vec::new();
        for site in &self.sites {
            let exists = site
                .client
                .issue_exists(issue_key)
                .await
                .map_err(|source| IssueSiteResolutionError::Jira {
                    site_key: site.key.clone(),
                    issue_key: issue_key.to_owned(),
                    source,
                })?;
            if exists {
                matches.push(site.key.clone());
            }
        }

        match matches.as_slice() {
            [site_key] => {
                self.database
                    .upsert_jira_issue_site_cache(&NewJiraIssueSiteCache {
                        issue_key,
                        jira_site_key: site_key,
                        source: "discovery",
                    })?;
                Ok(ResolvedIssueSite {
                    issue_key: issue_key.to_owned(),
                    jira_site_key: site_key.clone(),
                })
            }
            [] => Err(IssueSiteResolutionError::NoMatchingSite {
                issue_key: issue_key.to_owned(),
            }),
            _ => Err(IssueSiteResolutionError::MultipleMatchingSites {
                issue_key: issue_key.to_owned(),
                site_keys: matches,
            }),
        }
    }
}

impl From<ResolvedIssueSite> for IssueSiteMapping {
    fn from(resolved: ResolvedIssueSite) -> Self {
        Self {
            issue_key: resolved.issue_key,
            jira_site_key: resolved.jira_site_key,
        }
    }
}
