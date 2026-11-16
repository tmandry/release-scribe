//! Read-only GitHub collection. The transport boundary keeps offline fixtures
//! and the real CLI on the same collection path.
use crate::{Evidence, GithubContext};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{collections::BTreeSet, process::Command};

pub trait Api {
    fn get(&self, endpoint: &str, paginated: bool) -> Result<Value>;
}

pub struct Gh;
impl Api for Gh {
    fn get(&self, endpoint: &str, paginated: bool) -> Result<Value> {
        let mut command = Command::new("gh");
        command.env_remove("OPENAI_API_KEY");
        command.args(["api", "--method", "GET", endpoint]);
        if paginated {
            command.args(["--paginate", "--slurp"]);
        }
        let result = command.output().context("running gh api")?;
        ensure!(
            result.status.success(),
            "GitHub GET failed; response withheld"
        );
        let value = serde_json::from_slice(&result.stdout)?;
        if paginated {
            flatten_pages(value)
        } else {
            Ok(value)
        }
    }
}

pub fn flatten_pages(value: Value) -> Result<Value> {
    let pages = value
        .as_array()
        .context("paginated response must contain pages")?;
    let mut items = Vec::new();
    for page in pages {
        items.extend(
            page.as_array()
                .context("expected an array page")?
                .iter()
                .cloned(),
        );
    }
    Ok(Value::Array(items))
}

pub fn fetch_context(evidence: &Evidence, api: &impl Api) -> GithubContext {
    let mut result = GithubContext {
        repository: evidence.repository.clone(),
        ..Default::default()
    };
    let numbers: BTreeSet<_> = evidence
        .commits
        .iter()
        .flat_map(|c| c.pull_requests.iter().copied())
        .collect();
    for number in numbers {
        let base = format!("repos/{}", evidence.repository);
        let pr = match api.get(&format!("{base}/pulls/{number}"), false) {
            Ok(value) => value,
            Err(error) => {
                result
                    .limitations
                    .push(format!("PR #{number} unavailable: {error}"));
                continue;
            }
        };
        let head = pr["head"]["ref"].as_str().unwrap_or_default();
        if head.starts_with("release-please")
            || pr["labels"].as_array().is_some_and(|labels| {
                labels.iter().any(|l| {
                    l["name"]
                        .as_str()
                        .unwrap_or_default()
                        .starts_with("autorelease:")
                })
            })
        {
            result.pull_requests.insert(
                number,
                json!({"number": number, "release_metadata_only": true}),
            );
            continue;
        }
        let mut discussion = Vec::new();
        for endpoint in [
            format!("{base}/issues/{number}/comments?per_page=100"),
            format!("{base}/pulls/{number}/reviews?per_page=100"),
            format!("{base}/pulls/{number}/comments?per_page=100"),
        ] {
            match api.get(&endpoint, true) {
                Ok(Value::Array(items)) => {
                    for item in items {
                        discussion.push(json!({"id": item["id"], "body": item["body"], "url": item["html_url"],
                            "created_at": item.get("created_at").or_else(|| item.get("submitted_at")),
                            "updated_at": item["updated_at"], "review": item["state"], "path": item["path"],
                            "user": item["user"]["login"]}));
                    }
                }
                Ok(_) => result
                    .limitations
                    .push(format!("PR #{number}: malformed discussion response")),
                Err(error) => result
                    .limitations
                    .push(format!("PR #{number}: missing discussion: {error}")),
            }
        }
        discussion.sort_by_key(|c| {
            (
                c["created_at"].as_str().unwrap_or_default().to_owned(),
                c["url"].as_str().unwrap_or_default().to_owned(),
            )
        });
        result.pull_requests.insert(number, json!({"number": number, "title": pr["title"],
            "body": pr["body"], "updated_at": pr["updated_at"], "merge_commit_sha": pr["merge_commit_sha"],
            "head": head, "discussion": discussion}));
    }
    result.limitations.push("GitHub content is a current snapshot; historical deleted comments and older body revisions cannot be recovered by this collector.".into());
    result
}
