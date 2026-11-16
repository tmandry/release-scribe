//! Manual, isolated preview PRs. Production release refs are never written.
use crate::{github::Api, *};
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    io::Write,
    process::{Command, Stdio},
};

pub const BRANCH: &str = "release-scribe/evaluation";
pub const STATE: &str = "release-scribe-state-evaluation";
pub const MARKER: &str = "<!-- release-scribe:evaluation -->";
pub const START: &str = "<!-- release-scribe:notes -->";
pub const END: &str = "<!-- /release-scribe:notes -->";
pub const GUARD: &str = include_str!("../templates/no-merge.yml");

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub repository: String,
    pub source_branch: String,
    pub guidelines: String,
    #[serde(default)]
    pub context: Vec<String>,
    #[serde(default)]
    pub omit: Vec<String>,
    #[serde(default)]
    pub documentation_links: BTreeMap<String, String>,
    pub model: String,
}

pub trait Remote: Api {
    fn optional(&self, endpoint: &str) -> Result<Option<Value>>;
    fn write(&self, method: &str, endpoint: &str, body: &Value) -> Result<Value>;
}
impl Remote for github::Gh {
    fn optional(&self, endpoint: &str) -> Result<Option<Value>> {
        let output = Command::new("gh")
            .args(["api", "--method", "GET", endpoint])
            .output()?;
        if !output.status.success() {
            if String::from_utf8_lossy(&output.stderr).contains("(HTTP 404)") {
                return Ok(None);
            }
            anyhow::bail!("GitHub read failed; credentials and response withheld");
        }
        Ok(Some(serde_json::from_slice(&output.stdout)?))
    }
    fn write(&self, method: &str, endpoint: &str, body: &Value) -> Result<Value> {
        let mut child = Command::new("gh")
            .args(["api", "--method", method, endpoint, "--input", "-"])
            .env_remove("OPENAI_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("GitHub stdin")?
            .write_all(&serde_json::to_vec(body)?)?;
        let output = child.wait_with_output()?;
        ensure!(
            output.status.success(),
            "GitHub write failed; rerun manually after inspecting the remote state"
        );
        if output.stdout.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(serde_json::from_slice(&output.stdout)?)
        }
    }
}

fn sha(value: &Value) -> Result<String> {
    let value = value.as_str().context("missing object ID")?;
    ensure!(
        value.len() == 40 && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid object ID"
    );
    Ok(value.into())
}
fn file(api: &impl Remote, root: &str, reference: &str, path: &str) -> Result<Option<String>> {
    api.optional(&format!("{root}/contents/{path}?ref={reference}"))?
        .map(|v| {
            ensure!(v["encoding"] == "base64", "unexpected file encoding");
            Ok(String::from_utf8(
                STANDARD.decode(
                    v["content"]
                        .as_str()
                        .context("missing file content")?
                        .replace('\n', ""),
                )?,
            )?)
        })
        .transpose()
}
fn head(api: &impl Remote, root: &str, branch: &str) -> Result<Option<String>> {
    api.optional(&format!("{root}/git/ref/heads/{branch}"))?
        .map(|v| sha(&v["object"]["sha"]))
        .transpose()
}
fn tree(api: &impl Remote, root: &str, commit: &str) -> Result<String> {
    sha(&api.get(&format!("{root}/git/commits/{commit}"), false)?["tree"]["sha"])
}
fn entry(path: &str, content: &str) -> Value {
    json!({"path":path,"mode":"100644","type":"blob","content":content})
}

fn feedback(api: &impl Remote, root: &str, number: u64) -> Result<Vec<Value>> {
    let mut result = Vec::new();
    for endpoint in [
        format!("issues/{number}/comments"),
        format!("pulls/{number}/reviews"),
        format!("pulls/{number}/comments"),
    ] {
        let items = api.get(&format!("{root}/{endpoint}?per_page=100"), true)?;
        for item in items.as_array().context("invalid feedback")? {
            if item["user"]["type"] == "Bot"
                || item["body"].as_str().unwrap_or_default().trim().is_empty()
            {
                continue;
            }
            result.push(json!({"url":item["html_url"],"body":item["body"],"updated_at":item["updated_at"],"state":item["state"]}));
        }
    }
    result.sort_by_key(|v| v["url"].as_str().unwrap_or_default().to_owned());
    Ok(result)
}
fn commit(
    api: &impl Remote,
    root: &str,
    base_tree: &str,
    entries: Vec<Value>,
    parents: Vec<String>,
) -> Result<String> {
    let tree = api.write(
        "POST",
        &format!("{root}/git/trees"),
        &json!({"base_tree":base_tree,"tree":entries}),
    )?;
    sha(&api.write("POST", &format!("{root}/git/commits"), &json!({"message":"chore: Update release notes evaluation","tree":sha(&tree["sha"])?,"parents":parents}))?["sha"])
}
fn advance(
    api: &impl Remote,
    root: &str,
    branch: &str,
    previous: Option<&str>,
    next: &str,
) -> Result<()> {
    ensure!(
        head(api, root, branch)?.as_deref() == previous,
        "branch changed concurrently; no ref update attempted"
    );
    if previous.is_some() {
        api.write(
            "PATCH",
            &format!("{root}/git/refs/heads/{branch}"),
            &json!({"sha":next,"force":false}),
        )?;
    } else {
        api.write(
            "POST",
            &format!("{root}/git/refs"),
            &json!({"ref":format!("refs/heads/{branch}"),"sha":next}),
        )?;
    }
    Ok(())
}

pub fn managed_body(body: &str) -> Result<&str> {
    let (_, tail) = body
        .split_once(START)
        .context("missing notes marker; preserve the PR and inspect manually")?;
    let (text, _) = tail.split_once(END).context("missing end marker")?;
    ensure!(
        body.matches(START).count() == 1 && body.matches(END).count() == 1,
        "duplicate notes markers"
    );
    Ok(text.trim())
}
pub fn reconcile(current: Option<&str>, previous: Option<&str>, proposed: &str) -> (String, bool) {
    match current {
        Some(text) if previous != Some(text.trim()) => (text.to_owned(), true),
        _ => (proposed.to_owned(), false),
    }
}

/// Without apply this collects a reviewable request but invokes no model and writes no GitHub state.
pub fn run(
    repo: &Path,
    config: &Config,
    output: &Path,
    apply: bool,
    api: &impl Remote,
    model: impl FnOnce(&Value) -> Result<Value>,
) -> Result<Value> {
    ensure!(valid_repository(&config.repository), "invalid repository");
    git(
        repo,
        &[
            "check-ref-format",
            &format!("refs/heads/{}", config.source_branch),
        ],
    )?;
    ensure!(
        config.source_branch != BRANCH && config.source_branch != STATE,
        "source branch must be independent"
    );
    let root = format!("repos/{}", config.repository);
    let source = head(api, &root, &config.source_branch)?.context("source branch missing")?;
    // The checkout must be the exact trusted source snapshot; never run a PR's config.
    ensure!(
        git(repo, &["rev-parse", "HEAD"])?.trim() == source,
        "checkout is stale; fetch the source branch and rerun"
    );
    let release = api.get(&format!("{root}/releases/latest"), false)?;
    ensure!(
        release["draft"] == false && release["prerelease"] == false,
        "expected a published stable release"
    );
    let tag = release["tag_name"].as_str().context("release has no tag")?;
    let released_sha = sha(&api.get(&format!("{root}/commits/{tag}"), false)?["sha"])?;
    ensure!(
        git(repo, &["rev-parse", &format!("{tag}^{{commit}}")])?.trim() == released_sha,
        "local release tag differs from GitHub"
    );
    let old = head(api, &root, BRANCH)?;
    let prs = api.get(
        &format!(
            "{root}/pulls?state=open&head={}:{}&per_page=100",
            config.repository.split('/').next().unwrap(),
            BRANCH
        ),
        true,
    )?;
    let prs = prs.as_array().context("invalid PR list")?;
    ensure!(prs.len() <= 1, "multiple evaluation PRs");
    let pr = prs.first();
    if let Some(pr) = pr {
        ensure!(
            pr["body"].as_str().unwrap_or_default().contains(MARKER),
            "unmanaged evaluation PR"
        );
        ensure!(
            pr["base"]["ref"] == config.source_branch,
            "evaluation PR targets a different branch"
        );
        ensure!(
            pr["head"]["sha"].as_str() == old.as_deref(),
            "PR head changed"
        );
    }
    let mut state = Value::Null;
    let mut current_notes = None;
    if let Some(old) = &old {
        state = serde_json::from_str(
            &file(api, &root, old, ".release-scribe/state.json")?
                .context("unmanaged evaluation branch")?,
        )?;
        ensure!(
            state["kind"] == "release-scribe-evaluation-v1",
            "unknown evaluation branch"
        );
        let original = sha(&state["source"])?;
        let comparison = api.get(&format!("{root}/compare/{original}...{old}"), false)?;
        let files = comparison["files"]
            .as_array()
            .context("comparison missing files")?;
        ensure!(files.len() < 300, "comparison may be truncated");
        ensure!(
            files
                .iter()
                .all(|f| allowed_path(f["filename"].as_str().unwrap_or_default())),
            "evaluation branch has unrelated changes; refusing to overwrite"
        );
        current_notes = file(api, &root, old, ".release-scribe/notes.md")?;
        ensure!(
            current_notes.is_some(),
            "notes file was deleted; preserve the deletion and reconcile manually"
        );
    }
    let previous = state["last_generated"].as_str().map(str::trim);
    let current_body = pr.and_then(|p| p["body"].as_str());
    let edited_body = current_body
        .map(managed_body)
        .transpose()?
        .filter(|b| Some(*b) != previous);
    let edited_notes = current_notes
        .as_deref()
        .filter(|s| Some(s.trim()) != previous);
    let feedback = match pr {
        Some(pr) => feedback(
            api,
            &root,
            pr["number"].as_u64().context("missing PR number")?,
        )?,
        None => Vec::new(),
    };
    let initial = collect(
        repo,
        tag,
        &source,
        &config.repository,
        &config.omit,
        &config.context,
        GithubContext::default(),
    )?;
    let github = github::fetch_context(&initial, api);
    ensure!(
        github.limitations.len() == 1,
        "incomplete GitHub context; inspect collection before spending credits"
    );
    let mut evidence = collect(
        repo,
        tag,
        &source,
        &config.repository,
        &config.omit,
        &config.context,
        github,
    )?;
    evidence.documentation_links = config.documentation_links.clone();
    let request = openai::effective_request(&Request {
        prompt_version: PROMPT_VERSION.into(),
        instructions: default_instructions(),
        model: config.model.clone(),
        settings: BTreeMap::new(),
        guidelines: git(repo, &["show", &format!("{source}:{}", config.guidelines)])?,
        prior_state: json!({"maintainer_notes":edited_notes,"maintainer_pr_notes":edited_body,"feedback":feedback}),
        evidence,
    })?;
    let key = write_request(&request, &output.join("input"), 800_000)?;
    let state_head = head(api, &root, STATE)?;
    let cache = output.join("cache");
    if let Some(state_head) = &state_head
        && let Some(content) = file(api, &root, state_head, &format!("responses/{key}.json"))?
    {
        let notes: Notes = serde_json::from_str(&content)?;
        save_response(&cache, &request, &notes)?;
        if let Some(usage) = file(
            api,
            &root,
            state_head,
            &format!("responses/{key}.usage.json"),
        )? {
            let value: Value = serde_json::from_str(&usage)?;
            fs::write(
                cache.join(format!("{key}.usage.json")),
                serde_json::to_vec_pretty(&value)?,
            )?;
        }
    }
    if !apply {
        return Ok(
            json!({"apply":false,"source":source,"input_key":key,"cache_hit":load_response(&cache,&request)?.is_some()}),
        );
    }
    let generated = openai::generate(&request, &cache, model)?;
    // Persist the validated result before changing the PR, including on a resumed run.
    let cache_path = format!("responses/{key}.json");
    if state_head.is_none()
        || file(api, &root, state_head.as_deref().unwrap(), &cache_path)?.is_none()
    {
        let base_tree = match &state_head {
            Some(s) => tree(api, &root, s)?,
            None => {
                sha(&api.write("POST", &format!("{root}/git/trees"), &json!({"tree":[]}))?["sha"])?
            }
        };
        let parents = state_head.iter().cloned().collect();
        let next = commit(
            api,
            &root,
            &base_tree,
            vec![
                entry(
                    &cache_path,
                    &serde_json::to_string_pretty(&generated.notes)?,
                ),
                entry(
                    &format!("responses/{key}.usage.json"),
                    &serde_json::to_string_pretty(&generated.metadata)?,
                ),
            ],
            parents,
        )?;
        advance(api, &root, STATE, state_head.as_deref(), &next)?;
    }
    ensure!(
        head(api, &root, &config.source_branch)?.as_deref() == Some(&source),
        "source changed; result cached, PR untouched"
    );
    ensure!(
        api.get(&format!("{root}/releases/latest"), false)?["id"] == release["id"],
        "published release changed; result cached, rerun"
    );
    ensure!(
        head(api, &root, BRANCH)? == old,
        "evaluation branch changed; result cached, PR untouched"
    );
    // Recheck body/feedback revision before applying a proposal based on it.
    if let Some(pr) = pr {
        let number = pr["number"].as_u64().context("missing PR number")?;
        let fresh = api.get(&format!("{root}/pulls/{number}"), false)?;
        ensure!(
            self::feedback(api, &root, number)? == feedback,
            "feedback changed; result cached, rerun to reconcile"
        );
        ensure!(
            fresh["updated_at"] == pr["updated_at"] && fresh["body"] == pr["body"],
            "PR changed; result cached, rerun to reconcile"
        );
    }
    let markdown = render(&request.evidence, &generated.notes)?;
    let (notes, protected_notes) = reconcile(current_notes.as_deref(), previous, &markdown);
    let (body_notes, protected_body) = reconcile(
        current_body.map(managed_body).transpose()?,
        previous,
        &markdown,
    );
    let state = json!({"kind":"release-scribe-evaluation-v1","source":source,"base":request.evidence.base,"input_key":key,
        "last_generated":markdown,"human_edits_preserved":protected_notes || protected_body});
    let entries = vec![
        entry(".release-scribe/notes.md", &notes),
        entry(".release-scribe/proposed.md", &markdown),
        entry(
            ".release-scribe/state.json",
            &serde_json::to_string_pretty(&state)?,
        ),
        entry(
            ".release-scribe/coverage.json",
            &serde_json::to_string_pretty(&generated.notes.coverage)?,
        ),
        entry(
            ".release-scribe/review.md",
            &generated
                .notes
                .questions
                .iter()
                .map(|q| format!("- {q}\n"))
                .collect::<String>(),
        ),
        entry(".github/workflows/release-scribe-no-merge.yml", GUARD),
    ];
    let mut parents: Vec<String> = old.iter().cloned().collect();
    if !parents.contains(&source) {
        parents.push(source.clone());
    }
    let next = commit(api, &root, &tree(api, &root, &source)?, entries, parents)?;
    advance(api, &root, BRANCH, old.as_deref(), &next)?;
    let body = if let Some(body) = current_body {
        let (prefix, tail) = body.split_once(START).context("notes marker")?;
        let (_, suffix) = tail.split_once(END).context("end marker")?;
        format!("{prefix}{START}\n\n{body_notes}\n{END}{suffix}")
    } else {
        format!(
            "{MARKER}\nThis is an isolated release-notes evaluation. **Do not merge.** No versions, changelog, tags, or releases are changed. Suggestions are collected on the next manual run.\n\n{START}\n\n{body_notes}\n{END}\n\nCoverage and reviewer questions are in `.release-scribe/`. Maintainer edits are preserved; `proposed.md` contains the newest proposal."
        )
    };
    let result = if let Some(pr) = pr {
        api.write(
            "PATCH",
            &format!("{root}/pulls/{}", pr["number"]),
            &json!({"body":body}),
        )?
    } else {
        api.write("POST",&format!("{root}/pulls"),&json!({"title":"[DO NOT MERGE] Release notes evaluation","body":body,"head":BRANCH,"base":config.source_branch,"draft":true,"maintainer_can_modify":true}))?
    };
    api.write("POST",&format!("{root}/statuses/{next}"),&json!({"state":"failure","context":"release-scribe/no-merge","description":"Evaluation only. Do not merge this PR."}))?;
    Ok(
        json!({"url":result["html_url"],"input_key":key,"generation":generated.metadata,"human_edits_preserved":protected_notes || protected_body}),
    )
}

pub fn allowed_path(path: &str) -> bool {
    path.starts_with(".release-scribe/") || path == ".github/workflows/release-scribe-no-merge.yml"
}
