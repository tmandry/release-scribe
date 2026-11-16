use release_scribe::*;
use std::{collections::BTreeMap, fs, path::Path};
use tempfile::TempDir;

fn init() -> TempDir {
    let d = tempfile::tempdir().unwrap();
    git(d.path(), &["init", "-b", "main"]).unwrap();
    git(d.path(), &["config", "user.email", "tests@example.invalid"]).unwrap();
    git(d.path(), &["config", "user.name", "Tests"]).unwrap();
    d
}
fn commit(repo: &Path, file: &str, content: &str, message: &str) -> String {
    fs::write(repo.join(file), content).unwrap();
    git(repo, &["add", file]).unwrap();
    git(repo, &["commit", "-m", message]).unwrap();
    git(repo, &["rev-parse", "HEAD"]).unwrap().trim().into()
}
fn packet() -> Evidence {
    Evidence {
        schema_version: 1,
        repository: "owner/project".into(),
        base: "0".repeat(40),
        target: "1".repeat(40),
        commits: vec![Commit {
            id: "1".repeat(40),
            parents: vec![],
            message: "fix: behavior".into(),
            paths: vec!["code".into()],
            pull_requests: vec![42],
            patch: String::new(),
        }],
        changed_paths: vec![],
        omitted_paths: vec![],
        net_diff: String::new(),
        context: BTreeMap::new(),
        documentation_links: BTreeMap::new(),
        github: GithubContext::default(),
    }
}
fn notes() -> Notes {
    Notes {
        overview: "More reliable windows.".into(),
        bullets: vec![Bullet {
            id: "b1".into(),
            section: "Bug Fixes".into(),
            text: "Keep windows visible".into(),
            sources: vec!["1".repeat(40)],
        }],
        coverage: vec![Decision {
            source: "1".repeat(40),
            disposition: "included".into(),
            bullets: vec!["b1".into()],
            reason: "Visible fix".into(),
        }],
        questions: vec![],
    }
}
fn request() -> Request {
    Request {
        prompt_version: PROMPT_VERSION.into(),
        instructions: default_instructions(),
        model: "test".into(),
        settings: BTreeMap::new(),
        guidelines: "Write for users".into(),
        prior_state: serde_json::Value::Null,
        evidence: packet(),
    }
}

fn api_response(n: &Notes) -> serde_json::Value {
    serde_json::json!({"id":"resp_test","model":"test","status":"completed","usage":{"output_tokens":10},
        "output":[{"type":"message","content":[{"type":"output_text","text":serde_json::to_string(n).unwrap()}]}]})
}

#[test]
fn provider_uses_strict_schema_and_normalizes_settings_before_caching() {
    use release_scribe::openai::*;
    let r = request();
    let body = body(&r).unwrap();
    assert_eq!(body["store"], false);
    assert_eq!(body["reasoning"]["effort"], "high");
    assert_eq!(body["text"]["format"]["strict"], true);
    assert!(body.get("tools").is_none());
    let cache = tempfile::tempdir().unwrap();
    let result = generate(&r, cache.path(), |_| Ok(api_response(&notes()))).unwrap();
    assert_eq!(result.metadata["cache_hit"], false);
    let again = generate(&effective_request(&r).unwrap(), cache.path(), |_| {
        panic!("cache hit made a paid call")
    })
    .unwrap();
    assert_eq!(again.metadata["cache_hit"], true);
    let mut bad = r.clone();
    bad.settings
        .insert("temperature".into(), serde_json::json!(1));
    assert!(effective_request(&bad).is_err());
}

#[test]
fn provider_failure_refusal_and_invalid_coverage_are_never_cached_or_retried() {
    use release_scribe::openai::*;
    use serde_json::json;
    for value in [
        json!({"status":"incomplete"}),
        json!({"status":"completed","output":[{"content":[{"type":"refusal"}]}]}),
        api_response(&Notes {
            coverage: vec![],
            ..notes()
        }),
    ] {
        let cache = tempfile::tempdir().unwrap();
        let calls = std::cell::Cell::new(0);
        assert!(
            generate(&request(), cache.path(), |_| {
                calls.set(calls.get() + 1);
                Ok(value)
            })
            .is_err()
        );
        assert_eq!(calls.get(), 1);
        assert!(
            load_response(cache.path(), &effective_request(&request()).unwrap())
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn evaluation_preserves_human_edits_and_limits_writable_paths() {
    use release_scribe::evaluation::*;
    assert!(!allowed_path("Cargo.toml"));
    assert!(!allowed_path("CHANGELOG.md"));
    assert!(!allowed_path(".github/workflows/release-please.yml"));
    assert!(allowed_path(".release-scribe/notes.md"));
    assert_eq!(
        reconcile(Some("old\n"), Some("old"), "new"),
        ("new".into(), false)
    );
    assert_eq!(
        reconcile(Some("human"), Some("old"), "new"),
        ("human".into(), true)
    );
    assert_eq!(reconcile(Some(""), Some("old"), "new"), ("".into(), true));
    let body = format!("{START}\nHuman wording\n{END}");
    assert_eq!(managed_body(&body).unwrap(), "Human wording");
    assert!(managed_body("human removed markers").is_err());
}

#[cfg(unix)]
#[test]
fn transport_sends_key_only_on_stdin_and_withholds_error_body() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let script = d.path().join("curl");
    fs::write(&script, "#!/bin/sh\ncase \"$*\" in *sk-dummy*) exit 1;; esac\n[ -z \"$OPENAI_API_KEY\" ] || exit 2\nIFS= read -r header\n[ \"$header\" = 'header = \"Authorization: Bearer sk-dummy\"' ] || exit 3\nwhile [ \"$#\" -gt 0 ]; do\n if [ \"$1\" = '--output' ]; then shift; printf '%s' '{\"status\":\"completed\"}' > \"$1\"; fi\n shift\ndone\nprintf 200\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let result =
        openai::call_with(script.to_str().unwrap(), "sk-dummy", &serde_json::json!({})).unwrap();
    assert_eq!(result["status"], "completed");
    fs::write(
        &script,
        "#!/bin/sh\ncat >/dev/null\necho sk-dummy >&2\nprintf 401\n",
    )
    .unwrap();
    let error = openai::call_with(script.to_str().unwrap(), "sk-dummy", &serde_json::json!({}))
        .unwrap_err()
        .to_string();
    assert!(error.contains("401"));
    assert!(!error.contains("sk-dummy"));
}

#[test]
fn merge_range_retains_branch_commits_direct_commits_and_revert() {
    let d = init();
    let base = commit(d.path(), "code", "old", "base");
    git(d.path(), &["tag", "v1"]).unwrap();
    git(d.path(), &["checkout", "-b", "feature"]).unwrap();
    let feature = commit(d.path(), "code", "feature", "feat: New behavior");
    git(d.path(), &["checkout", "main"]).unwrap();
    let direct = commit(d.path(), "other", "direct", "User-visible direct change");
    git(
        d.path(),
        &[
            "merge",
            "--no-ff",
            "feature",
            "-m",
            "Merge pull request #42 from owner/feature",
        ],
    )
    .unwrap();
    git(d.path(), &["revert", "--no-edit", &feature]).unwrap();
    let before = git(d.path(), &["status", "--porcelain=v1"]).unwrap();
    let p = collect(
        d.path(),
        "v1",
        "HEAD",
        "owner/project",
        &[],
        &[],
        GithubContext::default(),
    )
    .unwrap();
    assert_eq!(p.base, base);
    assert_eq!(p.commits.len(), 4);
    assert!(
        p.commits
            .iter()
            .any(|c| c.id == feature && c.pull_requests == vec![42])
    );
    assert!(
        p.commits
            .iter()
            .any(|c| c.id == direct && c.pull_requests.is_empty())
    );
    assert!(!p.net_diff.contains("+feature"));
    assert_eq!(
        git(d.path(), &["status", "--porcelain=v1"]).unwrap(),
        before
    );
}

#[test]
fn omissions_remove_answer_but_preserve_inventory_and_safe_literal_paths() {
    let d = init();
    let base = commit(d.path(), "code", "old", "base");
    commit(d.path(), "CHANGELOG.md", "HOLDOUT ANSWER", "chore: release");
    commit(d.path(), "odd [x].rs", "new behavior", "fix: behavior");
    let p = collect(
        d.path(),
        &base,
        "HEAD",
        "owner/project",
        &["CHANGELOG.md".into()],
        &[],
        GithubContext::default(),
    )
    .unwrap();
    assert!(!p.net_diff.contains("HOLDOUT ANSWER"));
    assert!(p.net_diff.contains("new behavior"));
    assert_eq!(p.omitted_paths, vec!["CHANGELOG.md"]);
    assert_eq!(p.commits.len(), 2);
}

#[test]
fn reject_non_ancestor_and_wrong_github_fixture() {
    let d = init();
    let base = commit(d.path(), "code", "old", "base");
    let tip = commit(d.path(), "code", "new", "new");
    assert!(
        collect(
            d.path(),
            &tip,
            &base,
            "owner/project",
            &[],
            &[],
            GithubContext::default()
        )
        .is_err()
    );
    let gh = GithubContext {
        repository: "wrong/repo".into(),
        ..Default::default()
    };
    assert!(collect(d.path(), &base, &tip, "owner/project", &[], &[], gh).is_err());
}

#[test]
fn oversize_input_fails_without_partial_output() {
    let d = tempfile::tempdir().unwrap();
    let output = d.path().join("run");
    assert!(write_request(&request(), &output, 1).is_err());
    assert!(!output.exists());
}

#[test]
fn validation_rejects_missing_duplicate_unknown_and_inconsistent_sources() {
    let p = packet();
    let mut n = notes();
    n.coverage.clear();
    assert!(validate(&p, &n).is_err());
    n = notes();
    n.coverage.push(n.coverage[0].clone());
    assert!(validate(&p, &n).is_err());
    n = notes();
    n.bullets[0].sources = vec!["9".repeat(40)];
    assert!(validate(&p, &n).is_err());
    n = notes();
    n.coverage[0].bullets = vec!["missing".into()];
    assert!(validate(&p, &n).is_err());
    n = notes();
    n.coverage[0].disposition = "excluded".into();
    assert!(validate(&p, &n).is_err());
}

#[test]
fn unresolved_requires_question_and_exclusion_requires_reason() {
    let p = packet();
    let mut n = notes();
    n.bullets.clear();
    n.coverage[0].bullets.clear();
    n.coverage[0].disposition = "unresolved".into();
    assert!(validate(&p, &n).is_err());
    n.questions
        .push("Was this bug present in the base release?".into());
    validate(&p, &n).unwrap();
    n.coverage[0].disposition = "excluded".into();
    n.coverage[0].reason.clear();
    assert!(validate(&p, &n).is_err());
}

#[test]
fn cache_hit_invalidation_immutable_entries_and_corruption() {
    let d = tempfile::tempdir().unwrap();
    let r = request();
    assert!(load_response(d.path(), &r).unwrap().is_none());
    save_response(d.path(), &r, &notes()).unwrap();
    save_response(d.path(), &r, &notes()).unwrap();
    assert!(load_response(d.path(), &r).unwrap().is_some());
    let mut changed = r.clone();
    changed.model = "other".into();
    assert!(load_response(d.path(), &changed).unwrap().is_none());
    changed = r.clone();
    changed.guidelines.push_str(" revised");
    assert!(load_response(d.path(), &changed).unwrap().is_none());
    changed = r.clone();
    changed.instructions.push_str(" revised");
    assert!(load_response(d.path(), &changed).unwrap().is_none());
    changed = r.clone();
    changed.prior_state = serde_json::json!({"excluded": ["b1"]});
    assert!(load_response(d.path(), &changed).unwrap().is_none());
    changed = r.clone();
    changed
        .evidence
        .github
        .limitations
        .push("new feedback".into());
    assert!(load_response(d.path(), &changed).unwrap().is_none());
    let mut n = notes();
    n.overview = "different".into();
    assert!(save_response(d.path(), &r, &n).is_err());
    fs::write(
        d.path().join(format!("{}.json", request_key(&r).unwrap())),
        "broken",
    )
    .unwrap();
    assert!(load_response(d.path(), &r).is_err());
}

#[test]
fn rendering_deduplicates_pr_links_and_omits_review_questions() {
    let p = packet();
    let mut n = notes();
    n.questions.push("PRIVATE REVIEW QUESTION".into());
    let result = render(&p, &n).unwrap();
    assert!(result.contains("https://github.com/owner/project/pull/42"));
    assert!(!result.contains("PRIVATE REVIEW QUESTION"));
    assert_eq!(result.matches("### Bug Fixes").count(), 1);
}

#[test]
fn documentation_links_must_match_supplied_catalog_exactly() {
    let mut p = packet();
    let mut n = notes();
    n.bullets[0].text = "See [configuration](https://example.invalid/config/)".into();
    assert!(validate(&p, &n).is_err());
    p.documentation_links.insert(
        "configuration".into(),
        "https://example.invalid/config/".into(),
    );
    validate(&p, &n).unwrap();
    assert!(
        render(&p, &n)
            .unwrap()
            .contains("[configuration](https://example.invalid/config/)")
    );
    n.bullets[0].text = "See [configuration](https://example.invalid/config/#invented)".into();
    assert!(validate(&p, &n).is_err());
    n.bullets[0].text = "See [configuration](javascript:alert)".into();
    p.documentation_links
        .insert("unsafe".into(), "javascript:alert".into());
    assert!(validate(&p, &n).is_err());
}

#[test]
fn github_transport_flattens_pages_and_collector_reads_all_discussion_types() {
    use release_scribe::github::{Api, fetch_context, flatten_pages};
    use serde_json::{Value, json};
    use std::cell::RefCell;
    struct Fake {
        calls: RefCell<Vec<String>>,
        release: bool,
    }
    impl Api for Fake {
        fn get(&self, endpoint: &str, paginated: bool) -> anyhow::Result<Value> {
            self.calls.borrow_mut().push(endpoint.into());
            if !paginated {
                return Ok(json!({"number":42,"title":"Feature","body":"Description",
                    "head":{"ref": if self.release { "release-please--main" } else { "feature" }}}));
            }
            Ok(json!([{"id":1,"body":"Context","created_at":"2026-01-01T00:00:00Z"}]))
        }
    }
    assert_eq!(
        flatten_pages(json!([[1, 2], [3]])).unwrap(),
        json!([1, 2, 3])
    );
    assert!(flatten_pages(json!([{"items":[]}])).is_err());
    let api = Fake {
        calls: RefCell::new(vec![]),
        release: false,
    };
    let context = fetch_context(&packet(), &api);
    assert_eq!(api.calls.borrow().len(), 4);
    assert_eq!(
        context.pull_requests[&42]["discussion"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    let release_api = Fake {
        calls: RefCell::new(vec![]),
        release: true,
    };
    let context = fetch_context(&packet(), &release_api);
    assert_eq!(release_api.calls.borrow().len(), 1);
    assert!(context.pull_requests[&42].get("body").is_none());
}

#[test]
fn unrelated_pr_context_and_future_comments_do_not_enter_historical_packet() {
    use serde_json::json;
    let d = init();
    let base = commit(d.path(), "code", "old", "base");
    commit(
        d.path(),
        "code",
        "new",
        "Merge pull request #42 from owner/feature",
    );
    let github = GithubContext {
        repository: "owner/project".into(),
        pull_requests: BTreeMap::from([
            (
                42,
                json!({"discussion":[{"created_at":"2099-01-01T00:00:00Z","body":"FUTURE ANSWER"}]}),
            ),
            (99, json!({"body":"UNRELATED ANSWER"})),
        ]),
        limitations: vec![],
    };
    let p = collect(d.path(), &base, "HEAD", "owner/project", &[], &[], github).unwrap();
    let text = serde_json::to_string(&p).unwrap();
    assert!(!text.contains("FUTURE ANSWER"));
    assert!(!text.contains("UNRELATED ANSWER"));
}
