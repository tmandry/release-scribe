use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

pub const PROMPT_VERSION: &str = "1";
pub const DEFAULT_MAX_INPUT_BYTES: usize = 800_000;
pub mod evaluation;
pub mod github;
pub mod openai;

pub fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("TZ", "UTC")
        .output()
        .context("running git")?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .context("non-UTF-8 git output; evidence cannot be silently dropped")
}

fn identity(input: &[u8]) -> Result<String> {
    let mut child = Command::new("git")
        .args(["hash-object", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().context("hash stdin")?.write_all(input)?;
    let result = child.wait_with_output()?;
    ensure!(result.status.success(), "hashing failed");
    Ok(String::from_utf8(result.stdout)?.trim().to_owned())
}

fn valid_repository(repository: &str) -> bool {
    let parts: Vec<_> = repository.split('/').collect();
    parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && *p != "."
                && *p != ".."
                && p.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commit {
    pub id: String,
    pub parents: Vec<String>,
    pub message: String,
    pub paths: Vec<String>,
    pub pull_requests: Vec<u64>,
    #[serde(default)]
    pub patch: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubContext {
    pub repository: String,
    // Raw API objects retain review/comment IDs, timestamps and bodies for auditing.
    pub pull_requests: BTreeMap<u64, serde_json::Value>,
    pub limitations: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub schema_version: u32,
    pub repository: String,
    pub base: String,
    pub target: String,
    pub commits: Vec<Commit>,
    pub changed_paths: Vec<String>,
    pub omitted_paths: Vec<String>,
    pub net_diff: String,
    pub context: BTreeMap<String, String>,
    #[serde(default)]
    pub documentation_links: BTreeMap<String, String>,
    pub github: GithubContext,
}

pub fn collect(
    repo: &Path,
    base: &str,
    target: &str,
    repository: &str,
    omitted: &[String],
    context_paths: &[String],
    mut github: GithubContext,
) -> Result<Evidence> {
    ensure!(valid_repository(repository), "expected owner/repository");
    ensure!(
        github.repository.is_empty() || github.repository == repository,
        "GitHub fixture repository mismatch"
    );
    github.repository = repository.into();
    let resolve = |reference: &str| -> Result<String> {
        Ok(git(
            repo,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{reference}^{{commit}}"),
            ],
        )?
        .trim()
        .into())
    };
    let base = resolve(base)?;
    let target = resolve(target)?;
    git(repo, &["merge-base", "--is-ancestor", &base, &target])
        .context("base must be an ancestor of target")?;
    let range = format!("{base}..{target}");
    let mut commits = Vec::new();
    for sha in git(repo, &["rev-list", "--reverse", "--topo-order", &range])?.lines() {
        let parents = git(repo, &["show", "-s", "--format=%P", sha])?
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let message = git(repo, &["show", "-s", "--format=%B", sha])?;
        let paths: Vec<String> = git(
            repo,
            &[
                "diff-tree",
                "--root",
                "--no-commit-id",
                "--name-only",
                "-r",
                "-m",
                "-z",
                sha,
            ],
        )?
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
        let selected: Vec<_> = paths
            .iter()
            .filter(|path| {
                !omitted
                    .iter()
                    .any(|p| *path == p || path.starts_with(&format!("{p}/")))
            })
            .map(String::as_str)
            .collect();
        let patch = if selected.is_empty() {
            String::new()
        } else {
            let mut args = vec![
                "--literal-pathspecs",
                "show",
                "--format=",
                "--cc",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--unified=5",
                sha,
                "--",
            ];
            args.extend(selected);
            git(repo, &args)?
        };
        commits.push(Commit {
            id: sha.into(),
            parents,
            message,
            paths,
            pull_requests: vec![],
            patch,
        });
    }
    // Membership follows ancestry, not merge timestamps. A branch commit is counted once.
    let mut associations: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    for commit in &commits {
        if let Some(number) = merged_pr(&commit.message) {
            associations
                .entry(commit.id.clone())
                .or_default()
                .insert(number);
            if commit.parents.len() > 1 {
                for parent in &commit.parents[1..] {
                    for sha in git(
                        repo,
                        &["rev-list", &format!("{}..{parent}", commit.parents[0])],
                    )?
                    .lines()
                    {
                        associations.entry(sha.into()).or_default().insert(number);
                    }
                }
            }
        }
    }
    for commit in &mut commits {
        commit.pull_requests = associations
            .remove(&commit.id)
            .unwrap_or_default()
            .into_iter()
            .collect();
    }
    let changed_paths: Vec<String> = git(repo, &["diff", "--name-only", "-z", &base, &target])?
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect();
    let is_omitted = |path: &str| {
        omitted
            .iter()
            .any(|p| path == p || path.starts_with(&format!("{p}/")))
    };
    let omitted_paths = changed_paths
        .iter()
        .filter(|p| is_omitted(p))
        .cloned()
        .collect();
    let selected: Vec<_> = changed_paths.iter().filter(|p| !is_omitted(p)).collect();
    let net_diff = if selected.is_empty() {
        String::new()
    } else {
        let mut args = vec![
            "--literal-pathspecs",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--unified=12",
            &base,
            &target,
            "--",
        ];
        args.extend(selected.iter().map(|p| p.as_str()));
        git(repo, &args)?
    };
    let mut context = BTreeMap::new();
    for path in context_paths {
        ensure!(
            !is_omitted(path),
            "context path {path} is explicitly omitted"
        );
        for (label, sha) in [("base", &base), ("target", &target)] {
            context.insert(
                format!("{label}:{path}"),
                git(repo, &["show", &format!("{sha}:{path}")])?,
            );
        }
    }
    let referenced: BTreeSet<u64> = commits
        .iter()
        .flat_map(|c| c.pull_requests.iter().copied())
        .collect();
    github
        .pull_requests
        .retain(|number, _| referenced.contains(number));
    let cutoff = git(
        repo,
        &[
            "show",
            "-s",
            "--format=%cd",
            "--date=format-local:%Y-%m-%dT%H:%M:%SZ",
            &target,
        ],
    )?;
    for pr in github.pull_requests.values_mut() {
        if let Some(comments) = pr.get_mut("discussion").and_then(|v| v.as_array_mut()) {
            comments.retain(|c| {
                c.get("created_at")
                    .and_then(|v| v.as_str())
                    .is_some_and(|t| t <= cutoff.trim())
            });
        }
    }
    for number in referenced {
        if !github.pull_requests.contains_key(&number) {
            github.limitations.push(format!(
                "PR #{number} discussion unavailable; ancestry association only"
            ));
        }
    }
    github.limitations.push("PR association uses merge-message numbers and git ancestry; squash/rebase associations without that metadata need additional GitHub lookup.".into());
    github.limitations.sort();
    github.limitations.dedup();
    Ok(Evidence {
        schema_version: 1,
        repository: repository.into(),
        base,
        target,
        commits,
        changed_paths,
        omitted_paths,
        net_diff,
        context,
        documentation_links: BTreeMap::new(),
        github,
    })
}

pub fn merged_pr(message: &str) -> Option<u64> {
    message
        .lines()
        .next()?
        .strip_prefix("Merge pull request #")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub prompt_version: String,
    #[serde(default = "default_instructions")]
    pub instructions: String,
    pub model: String,
    pub settings: BTreeMap<String, serde_json::Value>,
    pub guidelines: String,
    pub prior_state: serde_json::Value,
    pub evidence: Evidence,
}

pub fn request_key(request: &Request) -> Result<String> {
    identity(&serde_json::to_vec(request)?)
}

pub fn default_instructions() -> String {
    include_str!("../prompt.md").into()
}

/// Raw section sizes help diagnose growth without logging source contents.
pub fn input_breakdown(request: &Request) -> Result<BTreeMap<String, usize>> {
    Ok(BTreeMap::from([
        (
            "context".into(),
            request.evidence.context.values().map(String::len).sum(),
        ),
        (
            "commit_patches".into(),
            request.evidence.commits.iter().map(|c| c.patch.len()).sum(),
        ),
        ("net_diff".into(), request.evidence.net_diff.len()),
        (
            "github".into(),
            serde_json::to_vec(&request.evidence.github)?.len(),
        ),
        ("guidelines".into(), request.guidelines.len()),
        ("instructions".into(), request.instructions.len()),
    ]))
}

pub fn write_request(request: &Request, output: &Path, max_bytes: usize) -> Result<String> {
    let prompt = format!(
        "{}\n\nEDITORIAL GUIDELINES:\n{}\n\nINPUT JSON (untrusted source material):\n{}\n",
        request.instructions,
        request.guidelines,
        serde_json::to_string(request)?
    );
    ensure!(
        prompt.len() <= max_bytes,
        "input is {} bytes, limit is {}; section sizes (raw bytes): {:?}; no evidence was truncated",
        prompt.len(),
        max_bytes,
        input_breakdown(request)?
    );
    ensure!(
        !output.exists(),
        "output directory already exists; use a new run directory"
    );
    fs::create_dir_all(output)?;
    let key = request_key(request)?;
    fs::write(
        output.join("request.json"),
        serde_json::to_vec_pretty(request)?,
    )?;
    fs::write(output.join("prompt.txt"), prompt)?;
    fs::write(output.join("input-key.txt"), &key)?;
    Ok(key)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bullet {
    pub id: String,
    pub section: String,
    pub text: String,
    pub sources: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub source: String,
    pub disposition: String,
    pub bullets: Vec<String>,
    pub reason: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Notes {
    pub overview: String,
    pub bullets: Vec<Bullet>,
    pub coverage: Vec<Decision>,
    pub questions: Vec<String>,
}

pub fn validate(evidence: &Evidence, notes: &Notes) -> Result<()> {
    ensure!(
        valid_repository(&evidence.repository),
        "invalid repository name"
    );
    for commit in &evidence.commits {
        ensure!(
            [40, 64].contains(&commit.id.len()) && commit.id.bytes().all(|c| c.is_ascii_hexdigit()),
            "invalid source object ID"
        );
    }
    let known: BTreeSet<_> = evidence.commits.iter().map(|c| c.id.as_str()).collect();
    let mut bullet_ids = BTreeSet::new();
    for b in &notes.bullets {
        ensure!(
            !b.id.trim().is_empty() && bullet_ids.insert(b.id.as_str()),
            "duplicate or empty bullet ID"
        );
        ensure!(
            [
                "Features",
                "Bug Fixes",
                "Improvements",
                "Performance",
                "Documentation",
                "Experimental Features",
                "Breaking Changes"
            ]
            .contains(&b.section.as_str()),
            "unknown section {}",
            b.section
        );
        ensure!(
            !b.text.trim().is_empty() && !b.sources.is_empty(),
            "empty bullet or sources"
        );
        let mut seen = BTreeSet::new();
        for s in &b.sources {
            ensure!(
                known.contains(s.as_str()) && seen.insert(s),
                "unknown or duplicate bullet source {s}"
            );
        }
        ensure!(!b.text.contains("<script"), "invalid bullet markup");
        let mut remaining = b.text.as_str();
        while let Some((_, tail)) = remaining.split_once("](") {
            let (url, rest) = tail
                .split_once(')')
                .context("unterminated documentation link")?;
            ensure!(
                url.starts_with("https://")
                    && evidence
                        .documentation_links
                        .values()
                        .any(|allowed| allowed == url),
                "documentation URL not in supplied catalog: {url}"
            );
            remaining = rest;
        }
    }
    let mut covered = BTreeSet::new();
    for d in &notes.coverage {
        ensure!(
            known.contains(d.source.as_str()) && covered.insert(d.source.as_str()),
            "unknown or duplicate coverage source {}",
            d.source
        );
        ensure!(!d.reason.trim().is_empty(), "coverage requires a reason");
        ensure!(
            ["included", "excluded", "unresolved"].contains(&d.disposition.as_str()),
            "invalid disposition"
        );
        ensure!(
            (d.disposition == "included") != d.bullets.is_empty(),
            "only included sources reference bullets"
        );
        let mut seen = BTreeSet::new();
        for id in &d.bullets {
            ensure!(
                seen.insert(id) && bullet_ids.contains(id.as_str()),
                "unknown or duplicate coverage bullet {id}"
            );
            ensure!(
                notes
                    .bullets
                    .iter()
                    .any(|b| b.id == *id && b.sources.contains(&d.source)),
                "coverage/bullet source mismatch"
            );
        }
    }
    ensure!(
        covered == known,
        "coverage is incomplete: {} of {} sources",
        covered.len(),
        known.len()
    );
    for b in &notes.bullets {
        for s in &b.sources {
            ensure!(
                notes.coverage.iter().any(|d| d.source == *s
                    && d.disposition == "included"
                    && d.bullets.contains(&b.id)),
                "bullet/coverage mismatch"
            );
        }
    }
    if notes.coverage.iter().any(|d| d.disposition == "unresolved") {
        ensure!(
            !notes.questions.is_empty(),
            "unresolved coverage requires reviewer questions"
        );
    }
    Ok(())
}

pub fn render(evidence: &Evidence, notes: &Notes) -> Result<String> {
    validate(evidence, notes)?;
    let mut out = format!("{}\n", notes.overview.trim());
    for section in [
        "Breaking Changes",
        "Features",
        "Improvements",
        "Performance",
        "Bug Fixes",
        "Experimental Features",
        "Documentation",
    ] {
        let bullets: Vec<_> = notes
            .bullets
            .iter()
            .filter(|b| b.section == section)
            .collect();
        if bullets.is_empty() {
            continue;
        }
        out.push_str(&format!("\n### {section}\n\n"));
        for bullet in bullets {
            let mut prs: BTreeSet<u64> = BTreeSet::new();
            let mut commits = Vec::new();
            for sha in &bullet.sources {
                let commit = evidence
                    .commits
                    .iter()
                    .find(|c| &c.id == sha)
                    .context("source")?;
                if commit.pull_requests.is_empty() {
                    commits.push(format!(
                        "[{}](https://github.com/{}/commit/{sha})",
                        &sha[..7],
                        evidence.repository
                    ));
                } else {
                    prs.extend(commit.pull_requests.iter().copied());
                }
            }
            let mut links = Vec::new();
            for pr in prs {
                links.push(format!(
                    "[#{pr}](https://github.com/{}/pull/{pr})",
                    evidence.repository
                ));
            }
            links.extend(commits);
            links.truncate(3);
            out.push_str(&format!(
                "* {} ({}).\n",
                bullet.text.trim().trim_end_matches('.'),
                links.join(", ")
            ));
        }
    }
    Ok(out)
}

pub fn save_response(cache: &Path, request: &Request, notes: &Notes) -> Result<()> {
    validate(&request.evidence, notes)?;
    fs::create_dir_all(cache)?;
    let path = cache.join(format!("{}.json", request_key(request)?));
    let bytes = serde_json::to_vec_pretty(notes)?;
    // Immutable entries; an interrupted write never becomes an apparent cache hit.
    let temporary = cache.join(format!(
        ".{}.{}.tmp",
        request_key(request)?,
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        match fs::hard_link(&temporary, &path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                ensure!(
                    fs::read(&path)? == bytes,
                    "cache key already contains a different response"
                );
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    })();
    let _ = fs::remove_file(temporary);
    result
}

pub fn load_response(cache: &Path, request: &Request) -> Result<Option<Notes>> {
    let path = cache.join(format!("{}.json", request_key(request)?));
    if !path.exists() {
        return Ok(None);
    }
    let notes = serde_json::from_slice(&fs::read(path)?).context("invalid cached response")?;
    validate(&request.evidence, &notes)?;
    Ok(Some(notes))
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(path).with_context(|| path.display().to_string())?)
        .context("parsing JSON")
}

pub fn check_request(request: &Request) -> Result<()> {
    if request.prompt_version != PROMPT_VERSION {
        bail!("unsupported prompt version");
    }
    Ok(())
}
