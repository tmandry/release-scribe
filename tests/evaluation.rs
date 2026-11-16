use anyhow::{Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use release_scribe::{evaluation::*, github::Api, *};
use serde_json::{Value, json};
use std::{cell::RefCell, collections::BTreeMap, fs};

#[derive(Default)]
struct State {
    refs: BTreeMap<String, String>,
    trees: BTreeMap<String, BTreeMap<String, String>>,
    commits: BTreeMap<String, String>,
    writes: Vec<(String, String, Value)>,
    pr: Option<Value>,
    fail_pr: bool,
    unrelated: bool,
    next: u64,
}
struct Fake {
    state: RefCell<State>,
    base: String,
}
impl Fake {
    fn id(s: &mut State) -> String {
        s.next += 1;
        format!("{:040x}", s.next)
    }
}
impl Api for Fake {
    fn get(&self, endpoint: &str, _: bool) -> Result<Value> {
        let s = self.state.borrow();
        if endpoint.ends_with("/releases/latest") {
            return Ok(json!({"id":1,"tag_name":"v1","draft":false,"prerelease":false}));
        }
        if endpoint.ends_with("/commits/v1") {
            return Ok(json!({"sha":self.base}));
        }
        if endpoint.contains("/pulls?state=open") {
            return Ok(Value::Array(s.pr.iter().cloned().collect()));
        }
        if endpoint.ends_with("/pulls/42") {
            return Ok(s.pr.clone().unwrap());
        }
        if endpoint.contains("/compare/") {
            return Ok(
                json!({"files":[{"filename":if s.unrelated {"Cargo.toml"}else{".release-scribe/notes.md"}}]}),
            );
        }
        if let Some(id) = endpoint.split("/git/commits/").nth(1) {
            return Ok(json!({"tree":{"sha":s.commits[id]}}));
        }
        if endpoint.contains("?per_page=100") {
            return Ok(json!([]));
        }
        bail!("unexpected GET {endpoint}")
    }
}
impl Remote for Fake {
    fn optional(&self, endpoint: &str) -> Result<Option<Value>> {
        let s = self.state.borrow();
        if let Some(branch) = endpoint.split("/git/ref/heads/").nth(1) {
            return Ok(s.refs.get(branch).map(|sha| json!({"object":{"sha":sha}})));
        }
        if let Some(tail) = endpoint.split("/contents/").nth(1) {
            let (path, id) = tail.split_once("?ref=").unwrap();
            return Ok(s
                .commits
                .get(id)
                .and_then(|tree| s.trees[tree].get(path))
                .map(|content| json!({"encoding":"base64","content":STANDARD.encode(content)})));
        }
        bail!("unexpected optional GET {endpoint}")
    }
    fn write(&self, method: &str, endpoint: &str, body: &Value) -> Result<Value> {
        let mut s = self.state.borrow_mut();
        s.writes
            .push((method.into(), endpoint.into(), body.clone()));
        if endpoint.ends_with("/git/trees") {
            let mut files = body["base_tree"]
                .as_str()
                .map(|id| s.trees[id].clone())
                .unwrap_or_default();
            for e in body["tree"].as_array().unwrap() {
                files.insert(
                    e["path"].as_str().unwrap().into(),
                    e["content"].as_str().unwrap().into(),
                );
            }
            let id = Self::id(&mut s);
            s.trees.insert(id.clone(), files);
            return Ok(json!({"sha":id}));
        }
        if endpoint.ends_with("/git/commits") {
            let id = Self::id(&mut s);
            s.commits
                .insert(id.clone(), body["tree"].as_str().unwrap().into());
            return Ok(json!({"sha":id}));
        }
        if endpoint.contains("/git/refs") {
            let branch = if method == "POST" {
                body["ref"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("refs/heads/")
                    .unwrap()
            } else {
                assert_eq!(body["force"], false);
                endpoint.split("/git/refs/heads/").nth(1).unwrap()
            };
            assert!([BRANCH, STATE].contains(&branch));
            s.refs
                .insert(branch.into(), body["sha"].as_str().unwrap().into());
            return Ok(json!({}));
        }
        if endpoint.contains("/statuses/") {
            assert_eq!(body["state"], "failure");
            return Ok(json!({}));
        }
        if endpoint.ends_with("/pulls") || endpoint.ends_with("/pulls/42") {
            if s.fail_pr {
                s.fail_pr = false;
                bail!("simulated PR write failure");
            }
            let head = s.refs[BRANCH].clone();
            let mut pr = if method == "POST" {
                assert_eq!(body["draft"], true);
                json!({"number":42,"draft":true,"base":{"ref":"main"}})
            } else {
                assert!(body.get("draft").is_none());
                s.pr.clone().unwrap()
            };
            pr["body"] = body["body"].clone();
            pr["head"] = json!({"sha":head});
            pr["updated_at"] = json!("unchanged");
            pr["html_url"] = json!("https://example.invalid/pull/42");
            s.pr = Some(pr.clone());
            return Ok(pr);
        }
        bail!("unexpected write {endpoint}")
    }
}
fn fixture() -> (tempfile::TempDir, Fake, Config) {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-b", "main"]).unwrap();
    git(
        repo.path(),
        &["config", "user.email", "test@example.invalid"],
    )
    .unwrap();
    git(repo.path(), &["config", "user.name", "Test"]).unwrap();
    fs::write(repo.path().join("guidelines.md"), "Write for users").unwrap();
    git(repo.path(), &["add", "."]).unwrap();
    git(repo.path(), &["commit", "-m", "base"]).unwrap();
    let base = git(repo.path(), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_owned();
    git(repo.path(), &["tag", "v1"]).unwrap();
    fs::write(repo.path().join("code"), "feature").unwrap();
    git(repo.path(), &["add", "."]).unwrap();
    git(repo.path(), &["commit", "-m", "feat: Feature"]).unwrap();
    let source = git(repo.path(), &["rev-parse", "HEAD"])
        .unwrap()
        .trim()
        .to_owned();
    let mut state = State::default();
    state.refs.insert("main".into(), source.clone());
    state.commits.insert(source, "a".repeat(40));
    state.trees.insert(
        "a".repeat(40),
        BTreeMap::from([("code".into(), "feature".into())]),
    );
    let config: Config = serde_json::from_value(json!({"repository":"owner/project","source_branch":"main","guidelines":"guidelines.md","model":"test"})).unwrap();
    (
        repo,
        Fake {
            state: RefCell::new(state),
            base,
        },
        config,
    )
}
fn response(body: &Value) -> Result<Value> {
    let input: Value = serde_json::from_str(body["input"].as_str().unwrap()).unwrap();
    let sources: Vec<_> = input["evidence"]["commits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["id"].clone())
        .collect();
    let notes = json!({"overview":"This release includes a useful feature.","bullets":[{"id":"b1","section":"Features","text":"Use the feature.","sources":sources}],"coverage":sources.iter().map(|id|json!({"source":id,"disposition":"included","bullets":["b1"],"reason":"New capability"})).collect::<Vec<_>>(),"questions":[]});
    Ok(
        json!({"status":"completed","output":[{"content":[{"type":"output_text","text":notes.to_string()}]}]}),
    )
}
#[test]
fn dry_run_is_free_and_new_pr_is_draft_with_durable_cache_reused_on_fresh_runner() {
    let (repo, api, config) = fixture();
    let output = tempfile::tempdir().unwrap();
    run(
        repo.path(),
        &config,
        &output.path().join("dry"),
        false,
        &api,
        |_| panic!("paid dry run"),
    )
    .unwrap();
    assert!(api.state.borrow().writes.is_empty());
    run(
        repo.path(),
        &config,
        &output.path().join("first"),
        true,
        &api,
        response,
    )
    .unwrap();
    let s = api.state.borrow();
    assert_eq!(s.pr.as_ref().unwrap()["draft"], true);
    let state_write = s
        .writes
        .iter()
        .position(|(_, e, b)| e.ends_with("/git/refs") && b["ref"] == format!("refs/heads/{STATE}"))
        .unwrap();
    let pr_write = s
        .writes
        .iter()
        .position(|(_, e, _)| e.ends_with("/pulls"))
        .unwrap();
    assert!(state_write < pr_write);
    drop(s);
    api.state.borrow_mut().pr.as_mut().unwrap()["draft"] = json!(false);
    let fresh = tempfile::tempdir().unwrap();
    run(
        repo.path(),
        &config,
        &fresh.path().join("again"),
        true,
        &api,
        |_| panic!("paid cache hit"),
    )
    .unwrap();
    assert_eq!(api.state.borrow().pr.as_ref().unwrap()["draft"], false);
}
#[test]
fn failed_pr_write_resumes_from_durable_response_and_unrelated_changes_are_rejected() {
    let (repo, api, config) = fixture();
    api.state.borrow_mut().fail_pr = true;
    let output = tempfile::tempdir().unwrap();
    assert!(
        run(
            repo.path(),
            &config,
            &output.path().join("fail"),
            true,
            &api,
            response
        )
        .is_err()
    );
    run(
        repo.path(),
        &config,
        &output.path().join("resume"),
        true,
        &api,
        |_| panic!("paid retry"),
    )
    .unwrap();
    api.state.borrow_mut().unrelated = true;
    let before = api.state.borrow().writes.len();
    assert!(
        run(
            repo.path(),
            &config,
            &output.path().join("reject"),
            true,
            &api,
            |_| panic!("paid rejected branch")
        )
        .is_err()
    );
    assert_eq!(before, api.state.borrow().writes.len());
}
#[test]
fn stale_source_is_cached_without_updating_the_preview() {
    let (repo, api, config) = fixture();
    let output = tempfile::tempdir().unwrap();
    let result = run(repo.path(), &config, output.path(), true, &api, |body| {
        api.state
            .borrow_mut()
            .refs
            .insert("main".into(), "f".repeat(40));
        response(body)
    });
    assert!(result.is_err());
    let s = api.state.borrow();
    assert!(s.refs.contains_key(STATE));
    assert!(!s.refs.contains_key(BRANCH));
    assert!(s.pr.is_none());
}

#[test]
fn manual_body_edits_survive_generation_and_the_next_run_reuses_the_proposal() {
    let (repo, api, config) = fixture();
    let output = tempfile::tempdir().unwrap();
    run(
        repo.path(),
        &config,
        &output.path().join("initial"),
        true,
        &api,
        response,
    )
    .unwrap();
    {
        let mut s = api.state.borrow_mut();
        let pr = s.pr.as_mut().unwrap();
        pr["body"] = json!(
            pr["body"]
                .as_str()
                .unwrap()
                .replace("Use the feature", "Maintainer wording")
        );
    }
    run(
        repo.path(),
        &config,
        &output.path().join("edited"),
        true,
        &api,
        |body| {
            assert!(
                body["input"]
                    .as_str()
                    .unwrap()
                    .contains("Maintainer wording")
            );
            let mut result = response(body)?;
            let text = result["output"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .replace("a useful feature", "a revised feature");
            result["output"][0]["content"][0]["text"] = json!(text);
            Ok(result)
        },
    )
    .unwrap();
    let s = api.state.borrow();
    assert!(
        s.pr.as_ref().unwrap()["body"]
            .as_str()
            .unwrap()
            .contains("Maintainer wording")
    );
    let files = &s.trees[&s.commits[&s.refs[BRANCH]]];
    assert!(files[".release-scribe/proposed.md"].contains("a revised feature"));
    drop(s);
    run(
        repo.path(),
        &config,
        &output.path().join("repeat"),
        true,
        &api,
        |_| panic!("paid unchanged editorial input"),
    )
    .unwrap();
}


#[test]
fn configured_budget_is_enforced_before_paid_calls_or_remote_writes() {
    let (repo, api, mut config) = fixture();
    assert_eq!(config.max_input_bytes, DEFAULT_MAX_INPUT_BYTES);
    config.max_input_bytes = 1;
    let output = tempfile::tempdir().unwrap();
    let result = run(
        repo.path(),
        &config,
        &output.path().join("rejected"),
        true,
        &api,
        |_| panic!("oversize input made a paid call"),
    );
    assert!(result.unwrap_err().to_string().contains("limit is 1"));
    assert!(api.state.borrow().writes.is_empty());

    config.max_input_bytes = 2_000_000;
    let result = run(
        repo.path(),
        &config,
        &output.path().join("accepted"),
        false,
        &api,
        |_| panic!("dry run made a paid call"),
    )
    .unwrap();
    assert_eq!(result["input"]["limit_bytes"], 2_000_000);
    assert!(result["input"]["bytes"].as_u64().unwrap() > 1);
    assert!(result["input"]["sections"]["commit_patches"].is_number());
    assert!(api.state.borrow().writes.is_empty());
}
