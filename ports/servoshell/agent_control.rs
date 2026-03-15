/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Sender, bounded};
use log::{error, warn};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use servo::{EventLoopWaker, WebViewId};
use url::Url;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub(crate) enum AgentControlMessage {
    Status {
        response: Sender<Result<Value, String>>,
    },
    Open {
        url: Url,
        response: Sender<Result<Value, String>>,
    },
    Click {
        selector: String,
        response: Sender<Result<Value, String>>,
    },
    Fill {
        selector: String,
        value: String,
        response: Sender<Result<Value, String>>,
    },
    Eval {
        script: String,
        response: Sender<Result<Value, String>>,
    },
    DomBatch {
        script: String,
        response: Sender<Result<Value, String>>,
    },
    Html {
        response: Sender<Result<Value, String>>,
    },
    Snapshot {
        response: Sender<Result<Value, String>>,
    },
    Text {
        selector: String,
        response: Sender<Result<Value, String>>,
    },
    Screenshot {
        path: PathBuf,
        response: Sender<Result<Value, String>>,
    },
    Shutdown {
        response: Sender<Result<Value, String>>,
    },
    ResolvedClick {
        webview_id: WebViewId,
        response: Sender<Result<Value, String>>,
        result: Result<Value, String>,
    },
    ResolvedFillTarget {
        webview_id: WebViewId,
        selector: String,
        value: String,
        response: Sender<Result<Value, String>>,
        result: Result<Value, String>,
    },
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub(crate) enum AgentControlRequest {
    Status,
    Open {
        url: String,
    },
    Click {
        selector: String,
    },
    Fill {
        selector: String,
        value: String,
    },
    Eval {
        script: String,
    },
    DomBatch {
        script: String,
    },
    Html,
    Snapshot,
    Text {
        selector: String,
    },
    Screenshot {
        path: PathBuf,
    },
    Shutdown,
}

pub(crate) fn start_server(
    port: u16,
    command_sender: Sender<AgentControlMessage>,
    event_loop_waker: Box<dyn EventLoopWaker>,
) {
    let _ = thread::Builder::new()
        .name(format!("servo-agent-control-{port}"))
        .spawn(move || {
            let listener = match TcpListener::bind(("127.0.0.1", port)) {
                Ok(listener) => listener,
                Err(error) => {
                    error!("Failed to bind Nickelium agent control port {port}: {error}");
                    return;
                },
            };

            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(error) => {
                        warn!("Failed to accept Nickelium agent control connection: {error}");
                        continue;
                    },
                };

                if let Err(error) =
                    handle_connection(stream, &command_sender, event_loop_waker.as_ref())
                {
                    warn!("Nickelium agent control request failed: {error}");
                }
            }
        });
}

fn handle_connection(
    mut stream: TcpStream,
    command_sender: &Sender<AgentControlMessage>,
    event_loop_waker: &dyn EventLoopWaker,
) -> Result<(), String> {
    let request = read_request(&stream)?;
    let (response_sender, response_receiver) = bounded(1);
    let message = into_message(request, response_sender)?;
    command_sender
        .send(message)
        .map_err(|error| format!("Failed to queue agent control request: {error}"))?;
    event_loop_waker.wake();

    let response = match response_receiver.recv_timeout(COMMAND_TIMEOUT) {
        Ok(Ok(result)) => json!({
            "ok": true,
            "result": result,
        }),
        Ok(Err(error)) => json!({
            "ok": false,
            "error": error,
        }),
        Err(error) => json!({
            "ok": false,
            "error": format!("Timed out waiting for Nickelium agent control response: {error}"),
        }),
    };

    serde_json::to_writer(&mut stream, &response)
        .map_err(|error| format!("Failed to encode agent control response: {error}"))?;
    stream
        .write_all(b"\n")
        .map_err(|error| format!("Failed to write agent control response: {error}"))?;
    Ok(())
}

fn read_request(stream: &TcpStream) -> Result<AgentControlRequest, String> {
    let mut line = String::new();
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|error| format!("Failed to clone agent control stream: {error}"))?,
    );
    reader
        .read_line(&mut line)
        .map_err(|error| format!("Failed to read agent control request: {error}"))?;

    serde_json::from_str::<AgentControlRequest>(line.trim_end())
        .map_err(|error| format!("Failed to parse agent control request JSON: {error}"))
}

fn into_message(
    request: AgentControlRequest,
    response: Sender<Result<Value, String>>,
) -> Result<AgentControlMessage, String> {
    match request {
        AgentControlRequest::Status => Ok(AgentControlMessage::Status { response }),
        AgentControlRequest::Open { url } => Ok(AgentControlMessage::Open {
            url: Url::parse(&url).map_err(|error| format!("Invalid URL `{url}`: {error}"))?,
            response,
        }),
        AgentControlRequest::Click { selector } => {
            Ok(AgentControlMessage::Click { selector, response })
        },
        AgentControlRequest::Fill { selector, value } => Ok(AgentControlMessage::Fill {
            selector,
            value,
            response,
        }),
        AgentControlRequest::Eval { script } => Ok(AgentControlMessage::Eval { script, response }),
        AgentControlRequest::DomBatch { script } => {
            Ok(AgentControlMessage::DomBatch { script, response })
        },
        AgentControlRequest::Html => Ok(AgentControlMessage::Html { response }),
        AgentControlRequest::Snapshot => Ok(AgentControlMessage::Snapshot { response }),
        AgentControlRequest::Text { selector } => {
            Ok(AgentControlMessage::Text { selector, response })
        },
        AgentControlRequest::Screenshot { path } => {
            Ok(AgentControlMessage::Screenshot { path, response })
        },
        AgentControlRequest::Shutdown => Ok(AgentControlMessage::Shutdown { response }),
    }
}
