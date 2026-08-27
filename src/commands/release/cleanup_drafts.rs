//! One-off cleanup of the draft-release backlog the legacy tag trigger left
//! behind: every publish-*-* tag push used to open a draft GitHub Release
//! for something the bundle workflow never built (123 drafts of 775 releases
//! at the time of writing, almost all asset-free).
//!
//! Dry-run by default. Deletes ONLY drafts, ONLY asset-free ones, ONLY older
//! than the explicit cutoff, and only with --delete after the dry-run output
//! has been reviewed. Deletion is irreversible.

use std::fmt::{Display, Formatter};

use anyhow::Context;
use chrono::{DateTime, SecondsFormat, Utc};
use clap::Parser;
use serde::Serialize;

use crate::PrettyPrintable;

#[derive(Debug, Parser, Clone)]
#[command(about = "Delete asset-free draft releases left by the legacy tag trigger")]
pub struct Options {
    /// Only drafts created strictly before this date (YYYY-MM-DD).
    #[arg(long)]
    pub before: String,
    /// Actually delete; without this the command only lists.
    #[arg(long, default_value_t = false)]
    pub delete: bool,
    /// owner/name; defaults to GITHUB_REPOSITORY.
    #[arg(long, env = "GITHUB_REPOSITORY")]
    pub repo: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct DraftRecord {
    pub id: u64,
    pub created_at: String,
    pub tag: String,
    pub name: String,
}

#[derive(Debug, Serialize, Clone)]
pub struct CleanupDraftsResult {
    pub candidates: Vec<DraftRecord>,
    pub deleted: bool,
}

impl Display for CleanupDraftsResult {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for d in &self.candidates {
            writeln!(
                f,
                "{} draft {} ({}) tag={} name={}",
                if self.deleted {
                    "deleted"
                } else {
                    "would delete"
                },
                d.id,
                d.created_at,
                d.tag,
                d.name
            )?;
        }
        write!(
            f,
            "{} draft release(s){}",
            self.candidates.len(),
            if self.deleted {
                " deleted"
            } else {
                "; re-run with --delete after review"
            }
        )
    }
}

impl PrettyPrintable for CleanupDraftsResult {
    fn pretty_print(&self) -> String {
        self.to_string()
    }
}

/// Parse the `--before` date into the strict cutoff instant: midnight UTC at
/// the START of that day, so a draft created ON the date is kept.
fn cutoff_from_date(before: &str) -> anyhow::Result<DateTime<Utc>> {
    let date = chrono::NaiveDate::parse_from_str(before, "%Y-%m-%d")
        .with_context(|| format!("--before must be YYYY-MM-DD, got '{before}'"))?;
    Ok(date
        .and_hms_opt(0, 0, 0)
        .expect("midnight exists on every date")
        .and_utc())
}

pub async fn run(options: &Options) -> anyhow::Result<CleanupDraftsResult> {
    let cutoff = cutoff_from_date(&options.before)?;
    let token = std::env::var("GH_TOKEN")
        .or_else(|_| std::env::var("GITHUB_TOKEN"))
        .context("GH_TOKEN/GITHUB_TOKEN is not set; the cleanup needs a token")?;
    let (owner, name) = options
        .repo
        .split_once('/')
        .with_context(|| format!("--repo must be owner/name, got '{}'", options.repo))?;
    let octocrab = octocrab::OctocrabBuilder::new()
        .personal_token(token)
        .build()?;

    let first_page = octocrab
        .repos(owner, name)
        .releases()
        .list()
        .per_page(100)
        .send()
        .await
        .with_context(|| format!("cannot list releases of {}", options.repo))?;
    let releases = octocrab
        .all_pages::<octocrab::models::repos::Release>(first_page)
        .await
        .with_context(|| format!("cannot paginate releases of {}", options.repo))?;

    let candidates: Vec<DraftRecord> = releases
        .into_iter()
        .filter(|r| r.draft && r.assets.is_empty())
        .filter(|r| r.created_at.is_some_and(|created| created < cutoff))
        .map(|r| DraftRecord {
            id: *r.id,
            created_at: r
                .created_at
                .expect("filtered on presence")
                .to_rfc3339_opts(SecondsFormat::Secs, true),
            tag: if r.tag_name.is_empty() {
                "<no tag>".to_string()
            } else {
                r.tag_name
            },
            name: match r.name {
                Some(name) if !name.is_empty() => name,
                _ => "<no name>".to_string(),
            },
        })
        .collect();

    if options.delete {
        for candidate in &candidates {
            octocrab
                .repos(owner, name)
                .releases()
                .delete(candidate.id)
                .await
                .with_context(|| format!("failed to delete draft release {}", candidate.id))?;
        }
    }

    Ok(CleanupDraftsResult {
        candidates,
        deleted: options.delete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutoff_is_midnight_utc_and_strictly_before() {
        let cutoff = cutoff_from_date("2026-08-01").unwrap();
        let last_instant_before = "2026-07-31T23:59:59Z".parse::<DateTime<Utc>>().unwrap();
        let on_the_date = "2026-08-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert!(last_instant_before < cutoff);
        // Created ON the cutoff date: kept.
        assert!(on_the_date >= cutoff);
    }

    #[test]
    fn malformed_dates_are_rejected() {
        assert!(cutoff_from_date("01-08-2026").is_err());
        assert!(cutoff_from_date("2026-13-01").is_err());
        assert!(cutoff_from_date("yesterday").is_err());
        assert!(cutoff_from_date("").is_err());
    }
}
