/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::agent_control::AgentControlRequest;
use crate::prefs::default_config_dir;

const DEFAULT_INSTANCE: &str = "default";
const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
const ELEMENT_KEY: &str = "element-6066-11e4-a52e-4f735466cecf";
const STARTUP_SETTLE_DELAY: Duration = Duration::from_millis(150);
const SERVER_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SELECTOR_POLL_INTERVAL: Duration = Duration::from_millis(50);
const PRIMARY_PROGRAM_NAME: &str = "nickelium";
const LEGACY_PROGRAM_NAME: &str = "servo-agent";

#[derive(Debug, Clone)]
enum AgentCommand {
    Boot,
    Open { url: String },
    Start,
    Click { selector: String },
    DomBatch {
        script: String,
        timeout: Duration,
    },
    Fill { selector: String, value: String },
    Eval { script: String },
    Help,
    Html,
    Snapshot,
    Screenshot { path: PathBuf },
    Status,
    Shutdown,
    Text { selector: String },
    Wait { selector: String, timeout: Duration },
    Workflow { path: String },
}

#[derive(Debug)]
struct AgentCli {
    instance: String,
    command: AgentCommand,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AgentState {
    pid: Option<u32>,
    port: Option<u16>,
    control_port: Option<u16>,
    session_id: Option<String>,
}

#[derive(Debug)]
struct AgentPaths {
    root: PathBuf,
    profile_dir: PathBuf,
    state_file: PathBuf,
}

#[derive(Debug, Deserialize)]
struct WorkflowFile {
    #[serde(default)]
    keep_alive: bool,
    steps: Vec<WorkflowStep>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum WorkflowStep {
    Open {
        url: String,
    },
    Click {
        selector: String,
    },
    DomBatch {
        script: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Fill {
        selector: String,
        value: String,
    },
    Wait {
        selector: String,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    Text {
        selector: String,
    },
    Eval {
        script: String,
    },
    Html,
    Snapshot,
    Screenshot {
        path: PathBuf,
    },
}

pub(crate) fn maybe_run_current_binary() -> bool {
    if !current_executable_stem()
        .as_deref()
        .is_some_and(is_agent_program_name)
    {
        return false;
    }

    let raw_args: Vec<String> = env::args().skip(1).collect();
    if raw_args.is_empty() {
        print_help();
        return true;
    }

    let cli = match parse_cli(&raw_args) {
        Ok(Some(cli)) => cli,
        Ok(None) => return false,
        Err(error) => {
            eprintln!("{}: {error}", agent_program_name());
            std::process::exit(1);
        },
    };

    if let Err(error) = run(cli) {
        eprintln!("{}: {error}", agent_program_name());
        std::process::exit(1);
    }

    true
}

fn run(cli: AgentCli) -> Result<(), String> {
    let paths = paths_for_instance(&cli.instance)?;
    let mut state = load_state(&paths)?;

    match cli.command {
        AgentCommand::Help => {
            print_help();
            Ok(())
        },
        AgentCommand::Boot => {
            ensure_browser(&cli.instance, &paths, &mut state, false)?;
            print_json(json!({
                "ok": true,
                "instance": cli.instance,
                "command": "boot",
                "pid": state.pid,
                "port": state.port,
                "control_port": state.control_port,
                "profile_dir": paths.profile_dir,
            }));
            Ok(())
        },
        AgentCommand::Status => {
            let mut response = json!({
                "ok": true,
                "instance": cli.instance,
                "pid": state.pid,
                "port": state.port,
                "control_port": state.control_port,
                "session_id": state.session_id,
                "state_root": paths.root,
                "profile_dir": paths.profile_dir,
            });
            if let Some(client) = direct_client(&state) {
                merge_json_object(&mut response, client.request(AgentControlRequest::Status)?);
            } else if let Some(object) = response.as_object_mut() {
                object.insert(
                    "running".into(),
                    Value::Bool(state.control_port.is_some_and(port_is_reachable)),
                );
            }
            print_json(response);
            Ok(())
        },
        AgentCommand::Shutdown => {
            if let Some(client) = direct_client(&state) {
                let _ = client.request(AgentControlRequest::Shutdown);
            } else if let Some(pid) = state.pid {
                terminate_process(pid)?;
            }
            state.pid = None;
            state.port = None;
            state.control_port = None;
            state.session_id = None;
            save_state(&paths, &state)?;
            print_json(json!({
                "ok": true,
                "instance": cli.instance,
                "running": false,
            }));
            Ok(())
        },
        AgentCommand::Start => {
            ensure_browser(&cli.instance, &paths, &mut state, false)?;
            let result = if let Some(direct_client) = direct_client(&state) {
                run_direct_command(&cli.instance, &direct_client, AgentCommand::Start)?
            } else {
                let client = webdriver_client(&state)?;
                run_command(
                    &cli.instance,
                    &paths,
                    &mut state,
                    &client,
                    AgentCommand::Start,
                )?
            };
            print_json(result);
            Ok(())
        },
        AgentCommand::Workflow { path } => {
            let workflow = read_workflow(&path)?;
            let workflow_is_direct = workflow_is_direct_compatible(&workflow);
            ensure_browser(&cli.instance, &paths, &mut state, !workflow_is_direct)?;
            let results = if workflow_is_direct_compatible(&workflow) {
                if let Some(direct_client) = direct_client(&state) {
                    run_direct_workflow(&cli.instance, &paths, &mut state, &direct_client, workflow)?
                } else {
                    let client = webdriver_client(&state)?;
                    run_workflow(&cli.instance, &paths, &mut state, &client, workflow)?
                }
            } else {
                let client = webdriver_client(&state)?;
                run_workflow(&cli.instance, &paths, &mut state, &client, workflow)?
            };
            print_json(results);
            Ok(())
        },
        command => {
            ensure_browser(
                &cli.instance,
                &paths,
                &mut state,
                !is_direct_command(&command),
            )?;
            let result = if is_direct_command(&command) {
                if let Some(direct_client) = direct_client(&state) {
                    run_direct_command(&cli.instance, &direct_client, command)?
                } else {
                    let client = webdriver_client(&state)?;
                    run_command(&cli.instance, &paths, &mut state, &client, command)?
                }
            } else {
                let client = webdriver_client(&state)?;
                run_command(&cli.instance, &paths, &mut state, &client, command)?
            };
            print_json(result);
            Ok(())
        },
    }
}

fn webdriver_client(state: &AgentState) -> Result<WebDriverClient, String> {
    WebDriverClient::new(
        state
            .port
            .ok_or_else(|| "Instance does not have an assigned WebDriver port".to_string())?,
    )
}

fn direct_client(state: &AgentState) -> Option<DirectAgentClient> {
    state
        .control_port
        .filter(|port| port_is_reachable(*port))
        .map(DirectAgentClient::new)
}

fn is_direct_command(command: &AgentCommand) -> bool {
    matches!(
        command,
        AgentCommand::Start |
            AgentCommand::Open { .. } |
            AgentCommand::Click { .. } |
            AgentCommand::Fill { .. } |
            AgentCommand::DomBatch { .. } |
            AgentCommand::Eval { .. } |
            AgentCommand::Html |
            AgentCommand::Snapshot |
            AgentCommand::Screenshot { .. } |
            AgentCommand::Text { .. } |
            AgentCommand::Wait { .. }
    )
}

fn workflow_is_direct_compatible(workflow: &WorkflowFile) -> bool {
    workflow.steps.iter().all(|step| {
        matches!(
            step,
            WorkflowStep::Open { .. } |
                WorkflowStep::Click { .. } |
                WorkflowStep::DomBatch { .. } |
                WorkflowStep::Fill { .. } |
                WorkflowStep::Eval { .. } |
                WorkflowStep::Html |
                WorkflowStep::Snapshot |
                WorkflowStep::Screenshot { .. } |
                WorkflowStep::Text { .. } |
                WorkflowStep::Wait { .. }
        )
    })
}

fn run_direct_workflow(
    instance: &str,
    paths: &AgentPaths,
    state: &mut AgentState,
    client: &DirectAgentClient,
    workflow: WorkflowFile,
) -> Result<Value, String> {
    let mut results = Vec::with_capacity(workflow.steps.len());

    for step in workflow.steps {
        let command = match step {
            WorkflowStep::Click { selector } => AgentCommand::Click { selector },
            WorkflowStep::Fill { selector, value } => AgentCommand::Fill { selector, value },
            WorkflowStep::Open { url } => AgentCommand::Open { url },
            WorkflowStep::DomBatch { script, timeout_ms } => AgentCommand::DomBatch {
                script,
                timeout: Duration::from_millis(
                    timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT.as_millis() as u64),
                ),
            },
            WorkflowStep::Wait {
                selector,
                timeout_ms,
            } => AgentCommand::Wait {
                selector,
                timeout: Duration::from_millis(
                    timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT.as_millis() as u64),
                ),
            },
            WorkflowStep::Text { selector } => AgentCommand::Text { selector },
            WorkflowStep::Eval { script } => AgentCommand::Eval { script },
            WorkflowStep::Html => AgentCommand::Html,
            WorkflowStep::Snapshot => AgentCommand::Snapshot,
            WorkflowStep::Screenshot { path } => AgentCommand::Screenshot { path },
        };
        results.push(run_direct_command(instance, client, command)?);
    }

    if !workflow.keep_alive {
        let _ = client.request(AgentControlRequest::Shutdown);
        state.pid = None;
        state.port = None;
        state.control_port = None;
        state.session_id = None;
        save_state(paths, state)?;
    }

    Ok(json!({
        "ok": true,
        "instance": instance,
        "command": "workflow",
        "keep_alive": workflow.keep_alive,
        "results": results,
    }))
}

fn run_direct_command(
    instance: &str,
    client: &DirectAgentClient,
    command: AgentCommand,
) -> Result<Value, String> {
    match command {
        AgentCommand::Start => Ok(with_direct_metadata(
            instance,
            "start",
            client.request(AgentControlRequest::Status)?,
        )),
        AgentCommand::Open { url } => run_direct_open(instance, client, &url),
        AgentCommand::Click { selector } => {
            let mut result = with_direct_metadata(
                instance,
                "click",
                client.request(AgentControlRequest::Click {
                    selector: selector.clone(),
                })?,
            );
            if let Some(object) = result.as_object_mut() {
                object.insert("selector".into(), Value::String(selector));
            }
            Ok(result)
        },
        AgentCommand::DomBatch { script, .. } => Ok(with_direct_metadata(
            instance,
            "dom_batch",
            client.request(AgentControlRequest::DomBatch { script })?,
        )),
        AgentCommand::Fill { selector, value } => {
            let mut result = with_direct_metadata(
                instance,
                "fill",
                client.request(AgentControlRequest::Fill {
                    selector: selector.clone(),
                    value: value.clone(),
                })?,
            );
            if let Some(object) = result.as_object_mut() {
                object.insert("selector".into(), Value::String(selector));
            }
            Ok(result)
        },
        AgentCommand::Eval { script } => Ok(with_direct_metadata(
            instance,
            "eval",
            client.request(AgentControlRequest::Eval { script })?,
        )),
        AgentCommand::Html => Ok(with_direct_metadata(
            instance,
            "html",
            client.request(AgentControlRequest::Html)?,
        )),
        AgentCommand::Snapshot => Ok(flatten_direct_snapshot(
            instance,
            client.request(AgentControlRequest::Snapshot)?,
        )),
        AgentCommand::Screenshot { path } => Ok(with_direct_metadata(
            instance,
            "screenshot",
            client.request(AgentControlRequest::Screenshot { path })?,
        )),
        AgentCommand::Text { selector } => Ok(with_direct_metadata(
            instance,
            "text",
            client.request(AgentControlRequest::Text { selector })?,
        )),
        AgentCommand::Wait { selector, timeout } => run_direct_wait(instance, client, &selector, timeout),
        _ => Err("Command is not supported by the direct Nickelium control path".into()),
    }
}

fn flatten_direct_snapshot(instance: &str, result: Value) -> Value {
    let mut response = json!({
        "ok": true,
        "instance": instance,
        "command": "snapshot",
    });
    merge_json_object(&mut response, result.clone());
    if let Some(object) = response.as_object_mut() {
        if let Some(value) = object.remove("value") {
            if let Some(snapshot) = value.as_object() {
                for (key, nested_value) in snapshot {
                    object.insert(key.clone(), nested_value.clone());
                }
            } else {
                object.insert("value".into(), value);
            }
        }
    }
    response
}

fn run_direct_open(instance: &str, client: &DirectAgentClient, url: &str) -> Result<Value, String> {
    let normalized_url = normalize_url(url);
    let previous_url = client
        .request(AgentControlRequest::Status)
        .ok()
        .and_then(|status| status.get("url").and_then(Value::as_str).map(str::to_owned));

    client.request(AgentControlRequest::Open {
        url: normalized_url.clone(),
    })?;
    thread::sleep(SERVER_POLL_INTERVAL);

    let result =
        wait_for_direct_navigation_ready(client, previous_url.as_deref(), &normalized_url)?;
    Ok(with_direct_metadata(instance, "open", result))
}

fn wait_for_direct_navigation_ready(
    client: &DirectAgentClient,
    previous_url: Option<&str>,
    requested_url: &str,
) -> Result<Value, String> {
    let start = Instant::now();
    let readiness_script = r#"
(() => ({
  href: String(location.href),
  readyState: document.readyState,
  title: document.title || ""
}))()
"#;

    loop {
        match client.request(AgentControlRequest::Eval {
            script: readiness_script.into(),
        }) {
            Ok(result) => {
                let Some(value) = result.get("value") else {
                    return Err(format!(
                        "Nickelium direct open readiness probe returned unexpected payload: {result}"
                    ));
                };
                let href = value
                    .get("href")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let ready_state = value
                    .get("readyState")
                    .and_then(Value::as_str)
                    .unwrap_or("loading");
                let title = value
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let url_changed = previous_url.is_none_or(|previous| href != previous);
                let reached_target = href == requested_url;
                let is_ready = ready_state != "loading" &&
                    href != "about:blank" &&
                    (url_changed || reached_target);
                if is_ready {
                    return Ok(json!({
                        "url": href,
                        "title": title,
                        "load_status": ready_state,
                    }));
                }
            },
            Err(_) => {},
        }

        if start.elapsed() >= DEFAULT_WAIT_TIMEOUT {
            return Err(format!(
                "Timed out waiting for Nickelium page readiness on `{requested_url}` after {} ms",
                DEFAULT_WAIT_TIMEOUT.as_millis()
            ));
        }

        thread::sleep(SELECTOR_POLL_INTERVAL);
    }
}

fn run_direct_wait(
    instance: &str,
    client: &DirectAgentClient,
    selector: &str,
    timeout: Duration,
) -> Result<Value, String> {
    let start = Instant::now();
    let selector_json = serde_json::to_string(selector)
        .map_err(|error| format!("Failed to serialize selector `{selector}`: {error}"))?;
    let script =
        format!("(() => Boolean(document.querySelector({selector_json})))()");

    loop {
        let result = client.request(AgentControlRequest::Eval {
            script: script.clone(),
        })?;
        let found = result.get("value").and_then(Value::as_bool).unwrap_or(false);
        if found {
            return Ok(json!({
                "ok": true,
                "instance": instance,
                "command": "wait",
                "selector": selector,
                "timeout_ms": timeout.as_millis(),
            }));
        }

        if start.elapsed() >= timeout {
            return Err(format!(
                "Timed out waiting for selector `{selector}` after {} ms",
                timeout.as_millis()
            ));
        }

        thread::sleep(SELECTOR_POLL_INTERVAL);
    }
}

fn with_direct_metadata(instance: &str, command: &str, result: Value) -> Value {
    let mut response = json!({
        "ok": true,
        "instance": instance,
        "command": command,
    });
    merge_json_object(&mut response, result);
    response
}

fn merge_json_object(target: &mut Value, source: Value) {
    let Some(target_object) = target.as_object_mut() else {
        return;
    };

    let Some(source_object) = source.as_object() else {
        target_object.insert("value".into(), source);
        return;
    };

    for (key, value) in source_object {
        target_object.insert(key.clone(), value.clone());
    }
}

fn run_command(
    instance: &str,
    paths: &AgentPaths,
    state: &mut AgentState,
    client: &WebDriverClient,
    command: AgentCommand,
) -> Result<Value, String> {
    match command {
        AgentCommand::Open { url } => {
            let url = normalize_url(&url);
            with_session(instance, paths, state, client, |client, session_id| {
                run_session_command(
                    instance,
                    client,
                    session_id,
                    AgentCommand::Open { url: url.clone() },
                )
            })
        },
        AgentCommand::Start => with_session(instance, paths, state, client, |client, session_id| {
            run_session_command(instance, client, session_id, AgentCommand::Start)
        }),
        AgentCommand::Click { selector } => with_session(instance, paths, state, client, |client, session_id| {
            run_session_command(
                instance,
                client,
                session_id,
                AgentCommand::Click {
                    selector: selector.clone(),
                },
            )
        }),
        AgentCommand::DomBatch { script, timeout } => with_session(instance, paths, state, client, |client, session_id| {
            run_session_command(
                instance,
                client,
                session_id,
                AgentCommand::DomBatch {
                    script: script.clone(),
                    timeout,
                },
            )
        }),
        AgentCommand::Fill { selector, value } => with_session(instance, paths, state, client, |client, session_id| {
            run_session_command(
                instance,
                client,
                session_id,
                AgentCommand::Fill {
                    selector: selector.clone(),
                    value: value.clone(),
                },
            )
        }),
        AgentCommand::Eval { script } => with_session(instance, paths, state, client, |client, session_id| {
            run_session_command(
                instance,
                client,
                session_id,
                AgentCommand::Eval {
                    script: script.clone(),
                },
            )
        }),
        AgentCommand::Html => with_session(instance, paths, state, client, |client, session_id| {
            run_session_command(instance, client, session_id, AgentCommand::Html)
        }),
        AgentCommand::Help => unreachable!("handled earlier"),
        AgentCommand::Snapshot => with_session(instance, paths, state, client, |client, session_id| {
            run_session_command(instance, client, session_id, AgentCommand::Snapshot)
        }),
        AgentCommand::Screenshot { path } => with_session(instance, paths, state, client, |client, session_id| {
            run_session_command(
                instance,
                client,
                session_id,
                AgentCommand::Screenshot {
                    path: path.clone(),
                },
            )
        }),
        AgentCommand::Text { selector } => with_session(instance, paths, state, client, |client, session_id| {
            run_session_command(
                instance,
                client,
                session_id,
                AgentCommand::Text {
                    selector: selector.clone(),
                },
            )
        }),
        AgentCommand::Wait { selector, timeout } => with_session(instance, paths, state, client, |client, session_id| {
            run_session_command(
                instance,
                client,
                session_id,
                AgentCommand::Wait {
                    selector: selector.clone(),
                    timeout,
                },
            )
        }),
        AgentCommand::Workflow { .. } => unreachable!("handled earlier"),
        AgentCommand::Boot | AgentCommand::Status | AgentCommand::Shutdown => unreachable!("handled earlier"),
    }
}

fn run_workflow(
    instance: &str,
    paths: &AgentPaths,
    state: &mut AgentState,
    client: &WebDriverClient,
    workflow: WorkflowFile,
) -> Result<Value, String> {
    let mut results = Vec::with_capacity(workflow.steps.len());
    state.session_id = None;
    save_state(paths, state)?;
    let mut session_id = ensure_session(paths, state, client)?;

    for (index, step) in workflow.steps.into_iter().enumerate() {
        let command = match step {
            WorkflowStep::Open { url } => AgentCommand::Open { url },
            WorkflowStep::Click { selector } => AgentCommand::Click { selector },
            WorkflowStep::DomBatch { script, timeout_ms } => AgentCommand::DomBatch {
                script,
                timeout: Duration::from_millis(
                    timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT.as_millis() as u64),
                ),
            },
            WorkflowStep::Fill { selector, value } => AgentCommand::Fill { selector, value },
            WorkflowStep::Wait {
                selector,
                timeout_ms,
            } => AgentCommand::Wait {
                selector,
                timeout: Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_WAIT_TIMEOUT.as_millis() as u64)),
            },
            WorkflowStep::Text { selector } => AgentCommand::Text { selector },
            WorkflowStep::Eval { script } => AgentCommand::Eval { script },
            WorkflowStep::Html => AgentCommand::Html,
            WorkflowStep::Snapshot => AgentCommand::Snapshot,
            WorkflowStep::Screenshot { path } => AgentCommand::Screenshot { path },
        };

        match run_session_command(instance, client, &session_id, command.clone()) {
            Ok(result) => results.push(result),
            Err(error) if is_recoverable_session_error(&error) => {
                state.session_id = None;
                save_state(paths, state)?;
                session_id = ensure_session(paths, state, client)?;
                let retried = run_session_command(instance, client, &session_id, command)
                    .map_err(|retry_error| {
                        format!(
                            "Workflow step {} failed after session restart: {retry_error}",
                            index + 1
                        )
                    })?;
                results.push(retried);
            },
            Err(error) => {
                return Err(format!("Workflow step {} failed: {error}", index + 1));
            },
        }
    }

    if !workflow.keep_alive {
        if let Some(pid) = state.pid.take() {
            terminate_process(pid)?;
        }
        state.session_id = None;
        save_state(paths, state)?;
    }

    Ok(json!({
        "ok": true,
        "instance": instance,
        "command": "workflow",
        "keep_alive": workflow.keep_alive,
        "results": results,
    }))
}

fn run_session_command(
    instance: &str,
    client: &WebDriverClient,
    session_id: &str,
    command: AgentCommand,
) -> Result<Value, String> {
    match command {
        AgentCommand::Open { url } => {
            let url = normalize_url(&url);
            client.post(&format!("/session/{session_id}/url"), json!({ "url": url }))?;
            let current_url = client.string_get(&format!("/session/{session_id}/url"))?;
            let title = client.string_get(&format!("/session/{session_id}/title"))?;
            Ok(json!({
                "ok": true,
                "instance": instance,
                "command": "open",
                "url": current_url,
                "title": title,
            }))
        },
        AgentCommand::Start => {
            let url = client.string_get(&format!("/session/{session_id}/url"))?;
            let title = client.string_get(&format!("/session/{session_id}/title"))?;
            Ok(json!({
                "ok": true,
                "instance": instance,
                "command": "start",
                "url": url,
                "title": title,
            }))
        },
        AgentCommand::Click { selector } => {
            let element_id = find_element(client, session_id, &selector)?;
            client.post(
                &format!("/session/{session_id}/element/{element_id}/click"),
                json!({}),
            )?;
            Ok(json!({
                "ok": true,
                "instance": instance,
                "command": "click",
                "selector": selector,
            }))
        },
        AgentCommand::DomBatch { script, timeout } => {
            apply_script_timeout(client, session_id, timeout)?;
            let response = client.post(
                &format!("/session/{session_id}/execute/async"),
                json!({
                    "script": wrap_async_workflow_script(&script),
                    "args": [],
                }),
            )?;
            let value = extract_value(&response);
            if let Some(error) = value.get("error").and_then(Value::as_str) {
                return Err(format!("DOM batch failed: {error}"));
            }
            Ok(json!({
                "ok": true,
                "instance": instance,
                "command": "dom_batch",
                "value": value.get("value").cloned().unwrap_or(Value::Null),
            }))
        },
        AgentCommand::Fill { selector, value } => {
            let element_id = find_element(client, session_id, &selector)?;
            client.post(
                &format!("/session/{session_id}/element/{element_id}/clear"),
                json!({}),
            )?;
            client.post(
                &format!("/session/{session_id}/element/{element_id}/value"),
                json!({
                    "text": value,
                    "value": string_to_webdriver_keys(&value),
                }),
            )?;
            Ok(json!({
                "ok": true,
                "instance": instance,
                "command": "fill",
                "selector": selector,
            }))
        },
        AgentCommand::Eval { script } => {
            let response = client.post(
                &format!("/session/{session_id}/execute/sync"),
                json!({
                    "script": script,
                    "args": [],
                }),
            )?;
            Ok(json!({
                "ok": true,
                "instance": instance,
                "command": "eval",
                "value": extract_value(&response).clone(),
            }))
        },
        AgentCommand::Html => {
            let html = client.string_get(&format!("/session/{session_id}/source"))?;
            Ok(json!({
                "ok": true,
                "instance": instance,
                "command": "html",
                "html": html,
            }))
        },
        AgentCommand::Snapshot => {
            let url = client.string_get(&format!("/session/{session_id}/url"))?;
            let title = client.string_get(&format!("/session/{session_id}/title"))?;
            let html = client.string_get(&format!("/session/{session_id}/source"))?;
            let text_response = client.post(
                &format!("/session/{session_id}/execute/sync"),
                json!({
                    "script": "return document.body ? document.body.innerText : '';",
                    "args": [],
                }),
            )?;

            Ok(json!({
                "ok": true,
                "instance": instance,
                "command": "snapshot",
                "url": url,
                "title": title,
                "text": extract_value(&text_response).clone(),
                "html": html,
            }))
        },
        AgentCommand::Screenshot { path } => {
            let response = client.get(&format!("/session/{session_id}/screenshot"))?;
            let encoded = extract_string(extract_value(&response), "screenshot data")?;
            let image = BASE64_STANDARD
                .decode(encoded)
                .map_err(|error| format!("Failed to decode screenshot: {error}"))?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("Failed to create screenshot directory: {error}"))?;
            }
            fs::write(&path, image)
                .map_err(|error| format!("Failed to write screenshot {}: {error}", path.display()))?;
            Ok(json!({
                "ok": true,
                "instance": instance,
                "command": "screenshot",
                "path": path,
            }))
        },
        AgentCommand::Text { selector } => {
            let element_id = find_element(client, session_id, &selector)?;
            let text = client.string_get(&format!("/session/{session_id}/element/{element_id}/text"))?;
            Ok(json!({
                "ok": true,
                "instance": instance,
                "command": "text",
                "selector": selector,
                "text": text,
            }))
        },
        AgentCommand::Wait { selector, timeout } => {
            wait_for_selector(client, session_id, &selector, timeout)?;
            Ok(json!({
                "ok": true,
                "instance": instance,
                "command": "wait",
                "selector": selector,
                "timeout_ms": timeout.as_millis(),
            }))
        },
        AgentCommand::Boot | AgentCommand::Help | AgentCommand::Workflow { .. } | AgentCommand::Status | AgentCommand::Shutdown => {
            unreachable!("session command should not receive non-session actions")
        },
    }
}

fn with_session<T, F>(
    instance: &str,
    paths: &AgentPaths,
    state: &mut AgentState,
    client: &WebDriverClient,
    command: F,
) -> Result<T, String>
where
    F: Fn(&WebDriverClient, &str) -> Result<T, String>,
{
    let session_id = match ensure_session(paths, state, client) {
        Ok(session_id) => session_id,
        Err(error) if is_orphaned_session_error(&error) => {
            restart_instance(instance, paths, state)?;
            let retry_client = WebDriverClient::new(
                state
                    .port
                    .ok_or_else(|| "Recovered instance is missing a WebDriver port".to_string())?,
            )?;
            let session_id = ensure_session(paths, state, &retry_client)?;
            return command(&retry_client, &session_id);
        },
        Err(error) => return Err(error),
    };
    match command(client, &session_id) {
        Ok(result) => Ok(result),
        Err(error) if is_recoverable_session_error(&error) => {
            state.session_id = None;
            save_state(paths, state)?;
            let session_id = ensure_session(paths, state, client)?;
            command(client, &session_id)
                .map_err(|retry_error| format!("Command failed after session restart: {retry_error}"))
        },
        Err(error) => Err(format!("Command `{}` failed: {error}", instance)),
    }
}

fn ensure_browser(
    instance: &str,
    paths: &AgentPaths,
    state: &mut AgentState,
    needs_webdriver: bool,
) -> Result<(), String> {
    let pid_live = state.pid.is_none_or(process_is_alive);
    let control_live = state.control_port.is_some_and(port_is_reachable);
    let webdriver_live = state.port.is_some_and(port_is_reachable);
    let state_is_live = pid_live && control_live && (!needs_webdriver || webdriver_live);
    if state_is_live {
        return Ok(());
    }

    let preferred_port = state.port;
    let preferred_control_port = state.control_port;
    if pid_live {
        if let Some(pid) = state.pid {
            terminate_process(pid)?;
            thread::sleep(Duration::from_millis(250));
        }
    }

    state.pid = None;
    state.port = None;
    state.control_port = None;
    state.session_id = None;
    let port = if needs_webdriver {
        Some(choose_port(instance, preferred_port)?)
    } else {
        None
    };
    let control_port = choose_control_port(instance, preferred_control_port, port)?;
    fs::create_dir_all(&paths.profile_dir)
        .map_err(|error| format!("Failed to create profile directory: {error}"))?;

    let mut command = Command::new(browser_binary_path()?);
    command
        .arg("--config-dir")
        .arg(&paths.profile_dir)
        .arg("--agent-control-port")
        .arg(control_port.to_string())
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(port) = port {
        command.arg("--webdriver").arg(port.to_string());
    }
    let child = command
        .spawn()
        .map_err(|error| format!("Failed to start Servo agent browser: {error}"))?;

    state.pid = Some(child.id());
    state.port = port;
    state.control_port = Some(control_port);
    save_state(paths, state)?;

    wait_for_direct_server(control_port, DEFAULT_START_TIMEOUT)?;
    if let Some(port) = state.port {
        wait_for_server(port, DEFAULT_START_TIMEOUT)?;
    }
    thread::sleep(STARTUP_SETTLE_DELAY);
    Ok(())
}

fn restart_instance(
    instance: &str,
    paths: &AgentPaths,
    state: &mut AgentState,
) -> Result<(), String> {
    let needs_webdriver = state.port.is_some();
    if let Some(pid) = state.pid {
        terminate_process(pid)?;
        thread::sleep(Duration::from_millis(250));
    }
    state.pid = None;
    state.port = None;
    state.control_port = None;
    state.session_id = None;
    save_state(paths, state)?;
    ensure_browser(instance, paths, state, needs_webdriver)
}

fn ensure_session(
    paths: &AgentPaths,
    state: &mut AgentState,
    client: &WebDriverClient,
) -> Result<String, String> {
    if let Some(session_id) = state.session_id.clone() {
        if client.get(&format!("/session/{session_id}/url")).is_ok() {
            return Ok(session_id);
        }
    }

    let response = client.post(
        "/session",
        json!({
            "capabilities": {
                "alwaysMatch": {}
            }
        }),
    )?;
    let value = extract_value(&response);
    let session_id = value
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("WebDriver did not return a session id: {response}"))?
        .to_owned();
    state.session_id = Some(session_id.clone());
    save_state(paths, state)?;
    Ok(session_id)
}

fn wait_for_selector(
    client: &WebDriverClient,
    session_id: &str,
    selector: &str,
    timeout: Duration,
) -> Result<(), String> {
    let start = Instant::now();
    loop {
        match find_element(client, session_id, selector) {
            Ok(_) => return Ok(()),
            Err(error) if error.contains("no such element") => {},
            Err(error) => return Err(error),
        }

        if start.elapsed() >= timeout {
            return Err(format!(
                "Timed out waiting for selector `{selector}` after {} ms",
                timeout.as_millis()
            ));
        }
        thread::sleep(SELECTOR_POLL_INTERVAL);
    }
}

fn find_element(client: &WebDriverClient, session_id: &str, selector: &str) -> Result<String, String> {
    let response = client.post(
        &format!("/session/{session_id}/element"),
        json!({
            "using": "css selector",
            "value": selector,
        }),
    )?;
    let element = extract_value(&response);
    element
        .get(ELEMENT_KEY)
        .or_else(|| element.as_object().and_then(|object| object.values().next()))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("WebDriver did not return an element reference for `{selector}`"))
}

fn wait_for_server(port: u16, timeout: Duration) -> Result<(), String> {
    let client = WebDriverClient::new(port)?;
    let start = Instant::now();
    loop {
        if let Ok(status) = client.get("/status") {
            let ready = extract_value(&status)
                .get("ready")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if ready {
                return Ok(());
            }
        }
        if start.elapsed() >= timeout {
            return Err(format!(
                "Timed out waiting for WebDriver on port {port} after {} ms",
                timeout.as_millis()
            ));
        }
        thread::sleep(SERVER_POLL_INTERVAL);
    }
}

fn wait_for_direct_server(port: u16, timeout: Duration) -> Result<(), String> {
    let client = DirectAgentClient::new(port);
    let start = Instant::now();
    loop {
        if client.request(AgentControlRequest::Status).is_ok() {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            return Err(format!(
                "Timed out waiting for Nickelium direct control on port {port} after {} ms",
                timeout.as_millis()
            ));
        }
        thread::sleep(SERVER_POLL_INTERVAL);
    }
}

fn choose_port(instance: &str, preferred: Option<u16>) -> Result<u16, String> {
    choose_port_in_range(instance, preferred, 7000)
}

fn choose_control_port(
    instance: &str,
    preferred: Option<u16>,
    webdriver_port: Option<u16>,
) -> Result<u16, String> {
    if let Some(port) = preferred {
        if Some(port) != webdriver_port && port_is_available(port) {
            return Ok(port);
        }
    }

    choose_port_in_range(instance, None, 7700)
}

fn choose_port_in_range(instance: &str, preferred: Option<u16>, base: u16) -> Result<u16, String> {
    if let Some(port) = preferred {
        if port_is_available(port) {
            return Ok(port);
        }
    }

    let mut hash = 0u16;
    for byte in instance.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(byte.into());
    }
    let start = base.saturating_add(hash % 500);

    for offset in 0..250u16 {
        let port = start.saturating_add(offset);
        if port_is_available(port) {
            return Ok(port);
        }
    }

    Err(format!("Failed to find a free TCP port starting at {base}"))
}

fn port_is_available(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}

fn port_is_reachable(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(200),
    )
    .is_ok()
}

fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, 0) == 0 ||
            std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn terminate_process(pid: u32) -> Result<(), String> {
    #[cfg(unix)]
    unsafe {
        if libc::kill(pid as i32, libc::SIGTERM) != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(format!("Failed to terminate process {pid}: {error}"));
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
    }

    Ok(())
}

fn paths_for_instance(instance: &str) -> Result<AgentPaths, String> {
    if !instance
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(format!(
            "Invalid instance name `{instance}`. Use ASCII letters, digits, '-' or '_' only"
        ));
    }

    let root = default_config_dir()
        .unwrap_or_else(|| env::current_dir().expect("Current directory should be readable"))
        .join("nickelium")
        .join("agent")
        .join(instance);
    fs::create_dir_all(&root)
        .map_err(|error| format!("Failed to create instance directory {}: {error}", root.display()))?;

    Ok(AgentPaths {
        profile_dir: root.join("profile"),
        state_file: root.join("state.json"),
        root,
    })
}

fn load_state(paths: &AgentPaths) -> Result<AgentState, String> {
    if !paths.state_file.exists() {
        return Ok(AgentState::default());
    }

    let data = fs::read(&paths.state_file)
        .map_err(|error| format!("Failed to read state file {}: {error}", paths.state_file.display()))?;
    serde_json::from_slice(&data)
        .map_err(|error| format!("Failed to parse state file {}: {error}", paths.state_file.display()))
}

fn save_state(paths: &AgentPaths, state: &AgentState) -> Result<(), String> {
    let data = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("Failed to serialize agent state: {error}"))?;
    fs::write(&paths.state_file, data)
        .map_err(|error| format!("Failed to write state file {}: {error}", paths.state_file.display()))
}

fn parse_cli(args: &[String]) -> Result<Option<AgentCli>, String> {
    let mut instance = DEFAULT_INSTANCE.to_string();
    let mut timeout = DEFAULT_WAIT_TIMEOUT;
    let mut remaining = Vec::new();
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "--instance" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--instance requires a value".to_string());
                };
                instance = value.clone();
                index += 2;
            },
            "--timeout-ms" => {
                let Some(value) = args.get(index + 1) else {
                    return Err("--timeout-ms requires a value".to_string());
                };
                let timeout_ms = value
                    .parse::<u64>()
                    .map_err(|error| format!("Invalid --timeout-ms value `{value}`: {error}"))?;
                timeout = Duration::from_millis(timeout_ms);
                index += 2;
            },
            arg => {
                remaining.push(arg.to_owned());
                index += 1;
            },
        }
    }

    let Some(command) = remaining.first() else {
        return Ok(Some(AgentCli {
            instance,
            command: AgentCommand::Help,
        }));
    };

    let command = match command.as_str() {
        "boot" => AgentCommand::Boot,
        "start" | "launch" => AgentCommand::Start,
        "open" | "goto" | "navigate" => {
            let url = required_arg(&remaining, 1, "open <url>")?.to_owned();
            AgentCommand::Open { url }
        },
        "click" => AgentCommand::Click {
            selector: join_args(&remaining, 1, "click <selector>")?,
        },
        "fill" | "type" => {
            let selector = required_arg(&remaining, 1, "fill <selector> <text>")?.to_owned();
            let value = join_args(&remaining, 2, "fill <selector> <text>")?;
            AgentCommand::Fill { selector, value }
        },
        "eval" | "js" => AgentCommand::Eval {
            script: join_args(&remaining, 1, "eval <script>")?,
        },
        "help" => AgentCommand::Help,
        "html" | "source" => AgentCommand::Html,
        "snapshot" => AgentCommand::Snapshot,
        "screenshot" => AgentCommand::Screenshot {
            path: PathBuf::from(required_arg(&remaining, 1, "screenshot <path>")?),
        },
        "status" => AgentCommand::Status,
        "shutdown" | "close" => AgentCommand::Shutdown,
        "text" => AgentCommand::Text {
            selector: join_args(&remaining, 1, "text <selector>")?,
        },
        "wait" => AgentCommand::Wait {
            selector: join_args(&remaining, 1, "wait <selector>")?,
            timeout,
        },
        "workflow" | "run" => AgentCommand::Workflow {
            path: required_arg(&remaining, 1, "workflow <path|- for stdin>")?.to_owned(),
        },
        _ => return Ok(None),
    };

    Ok(Some(AgentCli { instance, command }))
}

fn required_arg<'a>(args: &'a [String], index: usize, usage: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("Missing argument. Usage: {} {usage}", agent_program_name()))
}

fn join_args(args: &[String], start: usize, usage: &str) -> Result<String, String> {
    if args.len() <= start {
        return Err(format!(
            "Missing argument. Usage: {} {usage}",
            agent_program_name()
        ));
    }
    Ok(args[start..].join(" "))
}

fn normalize_url(url: &str) -> String {
    if url.starts_with("http://") ||
        url.starts_with("https://") ||
        url.starts_with("about:") ||
        url.starts_with("data:") ||
        url.starts_with("file:")
    {
        url.to_owned()
    } else {
        format!("https://{url}")
    }
}

fn string_to_webdriver_keys(value: &str) -> Vec<String> {
    value.chars().map(|ch| ch.to_string()).collect()
}

fn apply_script_timeout(
    client: &WebDriverClient,
    session_id: &str,
    timeout: Duration,
) -> Result<(), String> {
    client.post(
        &format!("/session/{session_id}/timeouts"),
        json!({
            "script": timeout.as_millis(),
        }),
    )?;
    Ok(())
}

fn wrap_async_workflow_script(script: &str) -> String {
    format!(
        r#"
const __nickeliumDone = arguments[arguments.length - 1];
(async () => {{
{script}
}})().then(
  value => __nickeliumDone({{ value }}),
  error => __nickeliumDone({{
    error: String(error && (error.stack || error.message || error))
  }})
);
"#
    )
}

fn extract_value(response: &Value) -> &Value {
    response.get("value").unwrap_or(response)
}

fn extract_string<'a>(value: &'a Value, label: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("Expected {label} to be a string but got {value}"))
}

fn is_recoverable_session_error(error: &str) -> bool {
    error.contains("invalid session id") || error.contains("no such window")
}

fn is_orphaned_session_error(error: &str) -> bool {
    error.contains("Session is already started")
}

fn print_json(value: Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(&value).expect("JSON serialization should not fail")
    );
}

fn print_help() {
    eprintln!(
        "nickelium commands:\n  \
boot\n  \
start\n  \
open <url>\n  \
click <selector>\n  \
fill <selector> <text>\n  \
wait <selector> [--timeout-ms N]\n  \
text <selector>\n  \
eval <script>\n  \
html\n  \
snapshot\n  \
screenshot <path>\n  \
workflow <path|- for stdin>\n  \
status\n  \
shutdown\n\nLegacy alias: servo-agent\n\nGlobal options:\n  \
--instance <name>   isolate state, profile, and WebDriver port per agent instance\n  \
--timeout-ms <n>    timeout override for `wait`\n\nAny non-command invocation falls through to the normal ServoShell CLI."
    );
}

fn read_workflow(path: &str) -> Result<WorkflowFile, String> {
    let bytes = if path == "-" {
        let mut bytes = Vec::new();
        std::io::stdin()
            .read_to_end(&mut bytes)
            .map_err(|error| format!("Failed to read workflow from stdin: {error}"))?;
        bytes
    } else {
        fs::read(path).map_err(|error| format!("Failed to read workflow {}: {error}", path))?
    };

    serde_json::from_slice(&bytes)
        .map_err(|error| format!("Failed to parse workflow JSON from {path}: {error}"))
}

fn current_executable_stem() -> Option<String> {
    env::args_os()
        .next()
        .and_then(|path| Path::new(&path).file_stem().map(|stem| stem.to_owned()))
        .and_then(|stem| stem.into_string().ok())
}

fn is_agent_program_name(name: &str) -> bool {
    matches!(name, PRIMARY_PROGRAM_NAME | LEGACY_PROGRAM_NAME)
}

fn agent_program_name() -> &'static str {
    if current_executable_stem().as_deref() == Some(LEGACY_PROGRAM_NAME) {
        LEGACY_PROGRAM_NAME
    } else {
        PRIMARY_PROGRAM_NAME
    }
}

fn browser_binary_path() -> Result<PathBuf, String> {
    let current =
        env::current_exe().map_err(|error| format!("Failed to locate Nickelium binary: {error}"))?;
    let Some(stem) = current.file_stem().and_then(|stem| stem.to_str()) else {
        return Ok(current);
    };

    if !is_agent_program_name(stem) || stem == PRIMARY_PROGRAM_NAME {
        return Ok(current);
    }

    for browser_name in [
        if cfg!(windows) {
            "nickelium.exe"
        } else {
            "nickelium"
        },
        if cfg!(windows) {
            "servoshell.exe"
        } else {
            "servoshell"
        },
    ] {
        let browser_binary = current.with_file_name(browser_name);
        if browser_binary.exists() {
            return Ok(browser_binary);
        }
    }

    Ok(current)
}

struct DirectAgentClient {
    port: u16,
}

impl DirectAgentClient {
    fn new(port: u16) -> Self {
        Self { port }
    }

    fn request(&self, request: AgentControlRequest) -> Result<Value, String> {
        let mut stream = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], self.port)),
            Duration::from_secs(5),
        )
        .map_err(|error| format!("Nickelium direct control request failed: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(120)))
            .map_err(|error| format!("Failed to set direct-control read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|error| format!("Failed to set direct-control write timeout: {error}"))?;

        let body = serde_json::to_string(&request)
            .map_err(|error| format!("Failed to serialize direct-control request: {error}"))?;
        stream
            .write_all(body.as_bytes())
            .map_err(|error| format!("Failed to write direct-control request: {error}"))?;
        stream
            .write_all(b"\n")
            .map_err(|error| format!("Failed to terminate direct-control request: {error}"))?;

        let mut response = String::new();
        let mut reader = BufReader::new(stream);
        reader
            .read_line(&mut response)
            .map_err(|error| format!("Failed to read direct-control response: {error}"))?;
        let response: Value = serde_json::from_str(response.trim())
            .map_err(|error| format!("Failed to parse direct-control response JSON: {error}"))?;
        if response.get("ok").and_then(Value::as_bool) == Some(true) {
            return Ok(response.get("result").cloned().unwrap_or(Value::Null));
        }

        Err(response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("Nickelium direct-control request failed")
            .to_owned())
    }
}

struct WebDriverClient {
    port: u16,
}

impl WebDriverClient {
    fn new(port: u16) -> Result<Self, String> {
        Ok(Self { port })
    }

    fn get(&self, path: &str) -> Result<Value, String> {
        self.request("GET", path, None)
    }

    fn post(&self, path: &str, body: Value) -> Result<Value, String> {
        self.request("POST", path, Some(body))
    }

    fn string_get(&self, path: &str) -> Result<String, String> {
        let response = self.get(path)?;
        extract_string(extract_value(&response), path).map(str::to_owned)
    }

    fn request(&self, method: &str, path: &str, body: Option<Value>) -> Result<Value, String> {
        let body = body.map(|body| body.to_string()).unwrap_or_default();
        let mut stream = TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], self.port)),
            Duration::from_secs(5),
        )
        .map_err(|error| format!("WebDriver HTTP request failed: {error}"))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|error| format!("Failed to set read timeout: {error}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .map_err(|error| format!("Failed to set write timeout: {error}"))?;

        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAccept: application/json\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            self.port,
            body.len(),
            body,
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|error| format!("Failed to write WebDriver request: {error}"))?;

        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .map_err(|error| format!("Failed to read WebDriver response: {error}"))?;

        let (status, bytes) = parse_http_response(&response)?;
        let value = if bytes.is_empty() {
            json!({})
        } else {
            serde_json::from_slice::<Value>(&bytes)
                .map_err(|error| format!("Failed to decode WebDriver response: {error}"))?
        };

        if !(200..300).contains(&status) {
            return Err(format_webdriver_error(status, &value));
        }

        Ok(value)
    }
}

fn parse_http_response(response: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let Some(header_end) = response.windows(4).position(|window| window == b"\r\n\r\n") else {
        return Err("Malformed HTTP response from WebDriver".to_string());
    };

    let headers = &response[..header_end];
    let body = &response[header_end + 4..];
    let headers_str =
        std::str::from_utf8(headers).map_err(|error| format!("Invalid HTTP headers: {error}"))?;
    let mut header_lines = headers_str.split("\r\n");
    let status_line = header_lines
        .next()
        .ok_or_else(|| "Missing HTTP status line".to_string())?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("Malformed HTTP status line: {status_line}"))?
        .parse::<u16>()
        .map_err(|error| format!("Invalid HTTP status code in `{status_line}`: {error}"))?;

    let is_chunked = header_lines.any(|line| {
        line.to_ascii_lowercase()
            .starts_with("transfer-encoding: chunked")
    });

    if is_chunked {
        return decode_chunked_body(body).map(|decoded| (status, decoded));
    }

    Ok((status, body.to_vec()))
}

fn decode_chunked_body(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoded = Vec::new();

    loop {
        let Some(line_end) = body.windows(2).position(|window| window == b"\r\n") else {
            return Err("Malformed chunked response from WebDriver".to_string());
        };
        let line = std::str::from_utf8(&body[..line_end])
            .map_err(|error| format!("Invalid chunk header: {error}"))?;
        let size_hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|error| format!("Invalid chunk size `{size_hex}`: {error}"))?;
        body = &body[line_end + 2..];

        if size == 0 {
            return Ok(decoded);
        }

        if body.len() < size + 2 {
            return Err("Chunked response ended early".to_string());
        }

        decoded.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
}

fn format_webdriver_error(status: u16, response: &Value) -> String {
    let value = extract_value(response);
    let kind = value
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or("webdriver error");
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("No message");
    format!("HTTP {status}: {kind}: {message}")
}
