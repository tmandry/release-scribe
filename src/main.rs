use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use release_scribe::*;
use std::{collections::BTreeMap, fs, path::PathBuf};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}
#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Action {
    /// Generate with OpenAI, reusing a validated exact cache hit before any paid call.
    Generate {
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        cache: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Prepare an isolated evaluation; --apply permits one paid call and preview PR writes.
    Evaluate {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        apply: bool,
    },
    /// Print the complete request's cache key without a model invocation.
    Fingerprint {
        #[arg(long)]
        request: PathBuf,
    },
    /// Collect current PR context via read-only, paginated GitHub GET requests.
    Github {
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Read git history into a bounded model request. Never writes to the source repo.
    Collect {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        base: String,
        #[arg(long)]
        target: String,
        #[arg(long)]
        github_repo: String,
        #[arg(long)]
        guidelines: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        model: String,
        #[arg(long)]
        github_context: Option<PathBuf>,
        /// Verified documentation labels and exact HTTPS URLs, as a JSON object.
        #[arg(long)]
        documentation_catalog: Option<PathBuf>,
        #[arg(long)]
        prior_state: Option<PathBuf>,
        #[arg(long)]
        settings: Option<PathBuf>,
        #[arg(long)]
        omit: Vec<String>,
        #[arg(long)]
        context: Vec<String>,
        #[arg(long, default_value_t = 800_000)]
        max_bytes: usize,
    },
    /// Validate a response, save it immutably, and render notes and reviewer questions.
    Accept {
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        response: PathBuf,
        #[arg(long)]
        cache: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Restore an exact validated result without a model invocation.
    Restore {
        #[arg(long)]
        request: PathBuf,
        #[arg(long)]
        cache: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Action::Generate {
            request,
            cache,
            output,
        } => {
            ensure!(!output.exists(), "output directory already exists");
            let request: Request = read_json(&request)?;
            check_request(&request)?;
            let result = openai::generate(&request, &cache, openai::call)?;
            write_output(&output, &result.request, &result.notes)?;
            fs::write(
                output.join("generation.json"),
                serde_json::to_vec_pretty(&result.metadata)?,
            )?;
        }
        Action::Evaluate {
            repo,
            config,
            output,
            apply,
        } => {
            let config: evaluation::Config = read_json(&config)?;
            let result =
                evaluation::run(&repo, &config, &output, apply, &github::Gh, openai::call)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Action::Fingerprint { request } => {
            let request: Request = read_json(&request)?;
            check_request(&request)?;
            println!("{}", request_key(&request)?);
        }
        Action::Github { request, output } => {
            let request: Request = read_json(&request)?;
            check_request(&request)?;
            ensure!(!output.exists(), "output already exists");
            let context = github::fetch_context(&request.evidence, &github::Gh);
            fs::write(output, serde_json::to_vec_pretty(&context)?)?;
        }
        Action::Collect {
            repo,
            base,
            target,
            github_repo,
            guidelines,
            output,
            model,
            github_context,
            documentation_catalog,
            prior_state,
            settings,
            omit,
            context,
            max_bytes,
        } => {
            let github = match github_context {
                Some(p) => read_json(&p)?,
                None => GithubContext {
                    limitations: vec!["GitHub context not provided".into()],
                    ..Default::default()
                },
            };
            let mut evidence =
                collect(&repo, &base, &target, &github_repo, &omit, &context, github)?;
            if let Some(path) = documentation_catalog {
                evidence.documentation_links = read_json(&path)?;
            }
            let request = Request {
                prompt_version: PROMPT_VERSION.into(),
                instructions: default_instructions(),
                model,
                settings: match settings {
                    Some(p) => read_json(&p)?,
                    None => BTreeMap::new(),
                },
                guidelines: fs::read_to_string(guidelines)?,
                prior_state: match prior_state {
                    Some(p) => read_json(&p)?,
                    None => serde_json::Value::Null,
                },
                evidence,
            };
            println!("{}", write_request(&request, &output, max_bytes)?);
        }
        Action::Accept {
            request,
            response,
            cache,
            output,
        } => {
            let request: Request = read_json(&request)?;
            check_request(&request)?;
            let notes: Notes = read_json(&response)?;
            validate(&request.evidence, &notes)?;
            ensure!(!output.exists(), "output directory already exists");
            save_response(&cache, &request, &notes)?;
            write_output(&output, &request, &notes)?;
        }
        Action::Restore {
            request,
            cache,
            output,
        } => {
            let request: Request = read_json(&request)?;
            check_request(&request)?;
            let notes = load_response(&cache, &request)?
                .ok_or_else(|| anyhow::anyhow!("cache miss; no model was invoked"))?;
            ensure!(!output.exists(), "output directory already exists");
            write_output(&output, &request, &notes)?;
        }
    }
    Ok(())
}

fn write_output(path: &std::path::Path, request: &Request, notes: &Notes) -> Result<()> {
    let markdown = render(&request.evidence, notes)?;
    fs::create_dir_all(path)?;
    fs::write(path.join("notes.md"), markdown)?;
    fs::write(
        path.join("coverage.json"),
        serde_json::to_vec_pretty(&notes.coverage)?,
    )?;
    fs::write(
        path.join("review.md"),
        notes
            .questions
            .iter()
            .map(|q| format!("* {q}\n"))
            .collect::<String>(),
    )?;
    Ok(())
}
