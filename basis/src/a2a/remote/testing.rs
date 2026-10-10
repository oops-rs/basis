//! A remote agent for unit tests: a loopback listener serving an Agent Card
//! and nothing else — enough for a connect to succeed.

use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

use serde_json::json;

use crate::a2a::A2aAgent;

/// A loopback port nothing listens on now, but something can bind later.
pub(crate) fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("address")
        .port()
}

/// An agent configured at `port` on loopback, listening or not.
pub(crate) fn agent_at(name: &str, port: u16) -> A2aAgent {
    A2aAgent::new(mentra::a2a::A2aAgentConfig::new(
        name,
        format!("http://127.0.0.1:{port}"),
    ))
}

/// Starts serving a card on `port`, for the rest of the test process.
pub(crate) fn serve_card(port: u16) {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind the reserved port");
    let card = json!({
        "name": "Reviewer",
        "description": "Reviews diffs.",
        "version": "1.0.0",
        "supportedInterfaces": [
            {"url": format!("http://127.0.0.1:{port}/rpc"), "protocolBinding": "JSONRPC", "protocolVersion": "1.0"},
        ],
        "capabilities": {},
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain"],
        "skills": [],
    })
    .to_string();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut head = Vec::new();
            let mut byte = [0_u8; 1];
            while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap_or(0) == 1 {
                head.push(byte[0]);
            }
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n{card}",
                card.len()
            );
        }
    });
}
