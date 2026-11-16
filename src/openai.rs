//! A bounded, single-call Responses API provider. Secrets never enter requests,
//! cache keys, command-line arguments, or diagnostic output.
use crate::{Notes, Request, load_response, read_json, save_response, validate};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

pub const MODEL: &str = "gpt-6.1-sol";
pub const ENDPOINT: &str = "https://api.openai.com/v1/responses";

pub fn effective_request(request: &Request) -> Result<Request> {
    let mut request = request.clone();
    ensure!(!request.model.is_empty(), "a model is required");
    for key in request.settings.keys() {
        ensure!(
            ["provider", "reasoning_effort", "max_output_tokens"].contains(&key.as_str()),
            "unsupported generation setting: {key}"
        );
    }
    if let Some(provider) = request.settings.get("provider") {
        ensure!(
            provider == "openai-responses-v1",
            "request uses a different provider"
        );
    }
    request
        .settings
        .insert("provider".into(), json!("openai-responses-v1"));
    request
        .settings
        .entry("reasoning_effort".into())
        .or_insert(json!("high"));
    request
        .settings
        .entry("max_output_tokens".into())
        .or_insert(json!(24000));
    let effort = request.settings["reasoning_effort"]
        .as_str()
        .context("reasoning_effort must be a string")?;
    ensure!(
        ["low", "medium", "high"].contains(&effort),
        "unsupported reasoning effort"
    );
    let limit = request.settings["max_output_tokens"]
        .as_u64()
        .context("max_output_tokens must be an integer")?;
    ensure!(
        (1024..=64000).contains(&limit),
        "max_output_tokens must be between 1024 and 64000"
    );
    Ok(request)
}

pub fn schema() -> Value {
    let string = json!({"type":"string"});
    let strings = json!({"type":"array","items":string});
    json!({"type":"object","additionalProperties":false,
        "required":["overview","bullets","coverage","questions"],
        "properties":{
            "overview":string,
            "bullets":{"type":"array","items":{"type":"object","additionalProperties":false,
                "required":["id","section","text","sources"],"properties":{
                    "id":string,"section":{"type":"string","enum":["Features","Bug Fixes","Improvements","Performance","Documentation","Experimental Features","Breaking Changes"]},
                    "text":string,"sources":strings}}},
            "coverage":{"type":"array","items":{"type":"object","additionalProperties":false,
                "required":["source","disposition","bullets","reason"],"properties":{
                    "source":string,"disposition":{"type":"string","enum":["included","excluded","unresolved"]},
                    "bullets":strings,"reason":string}}},
            "questions":strings}})
}

pub fn body(request: &Request) -> Result<Value> {
    let request = effective_request(request)?;
    Ok(json!({"model":request.model,"store":false,
        "instructions":format!("{}\n\nEDITORIAL GUIDELINES:\n{}",request.instructions,request.guidelines),
        "input":serde_json::to_string(&json!({"evidence":request.evidence,"prior_state":request.prior_state}))?,
        "reasoning":{"effort":request.settings["reasoning_effort"]},
        "max_output_tokens":request.settings["max_output_tokens"],
        "text":{"format":{"type":"json_schema","name":"release_notes","strict":true,"schema":schema()}}}))
}

pub fn parse(response: &Value) -> Result<Notes> {
    Ok(serde_json::from_value(structured_output(response)?)?)
}

pub(crate) fn structured_output(response: &Value) -> Result<Value> {
    ensure!(
        response["status"] == "completed",
        "OpenAI response was not completed; no automatic retry was made"
    );
    let mut text = String::new();
    for item in response["output"]
        .as_array()
        .context("OpenAI response has no output")?
    {
        if let Some(content) = item["content"].as_array() {
            for block in content {
                if block["type"] == "refusal" {
                    bail!("OpenAI refused this request; no response was cached");
                }
                if block["type"] == "output_text" {
                    text.push_str(block["text"].as_str().context("invalid output text")?);
                }
            }
        }
    }
    ensure!(!text.is_empty(), "OpenAI response has no structured notes");
    serde_json::from_str(&text).context("OpenAI response did not match the notes schema")
}

pub struct Generation {
    pub request: Request,
    pub notes: Notes,
    pub metadata: Value,
}

pub fn generate(
    request: &Request,
    cache: &Path,
    call: impl FnOnce(&Value) -> Result<Value>,
) -> Result<Generation> {
    crate::check_request(request)?;
    let request = effective_request(request)?;
    let key = crate::request_key(&request)?;
    if let Some(notes) = load_response(cache, &request)? {
        let metadata_path = cache.join(format!("{key}.usage.json"));
        let mut metadata: Value = if metadata_path.exists() {
            read_json(&metadata_path)?
        } else {
            json!({})
        };
        metadata["cache_hit"] = json!(true);
        return Ok(Generation {
            request,
            notes,
            metadata,
        });
    }
    let response = call(&body(&request)?)?;
    let notes = parse(&response)?;
    validate(&request.evidence, &notes)?;
    save_response(cache, &request, &notes)?;
    let metadata = json!({"provider":"openai-responses-v1","model_requested":request.model,
        "model_served":response["model"],"response_id":response["id"],"usage":response["usage"],
        "cache_hit":false,"input_key":key});
    fs::write(
        cache.join(format!("{key}.usage.json")),
        serde_json::to_vec_pretty(&metadata)?,
    )?;
    Ok(Generation {
        request,
        notes,
        metadata,
    })
}

/// curl is available on the supported CI runners; the key is supplied over stdin.
pub fn call(body: &Value) -> Result<Value> {
    let key = std::env::var("OPENAI_API_KEY")
        .context("Set OPENAI_API_KEY in the job's GitHub Actions secrets or local environment")?;
    call_with("curl", &key, body)
}

pub fn call_with(binary: &str, key: &str, body: &Value) -> Result<Value> {
    ensure!(
        !key.is_empty()
            && key
                .bytes()
                .all(|b| b.is_ascii_graphic() && !b"\"\\".contains(&b)),
        "OPENAI_API_KEY is missing or has an invalid format"
    );
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("request.json");
    let output = directory.path().join("response.json");
    fs::write(&input, serde_json::to_vec(body)?)?;
    let mut child = Command::new(binary)
        .args([
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--connect-timeout",
            "30",
            "--max-time",
            "600",
            "--request",
            "POST",
            "--config",
            "-",
            "--header",
            "Content-Type: application/json",
            "--data-binary",
        ])
        .arg(format!("@{}", input.display()))
        .arg("--output")
        .arg(&output)
        .args(["--write-out", "%{http_code}", ENDPOINT])
        .env_remove("OPENAI_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting the HTTPS transport")?;
    child
        .stdin
        .take()
        .context("transport stdin")?
        .write_all(format!("header = \"Authorization: Bearer {key}\"\n").as_bytes())?;
    let result = child.wait_with_output()?;
    ensure!(
        result.status.success(),
        "OpenAI HTTPS request failed; no automatic retry was made"
    );
    let status: u16 = String::from_utf8(result.stdout)?
        .trim()
        .parse()
        .context("invalid HTTP status")?;
    ensure!(
        status == 200,
        "OpenAI returned HTTP {}; response body withheld to avoid exposing credentials",
        status
    );
    read_json(&output).context("invalid OpenAI response")
}
