//! Sample provider adapter: a normal executable speaking the stdio protocol.
//!
//! It echoes the last user message back as word-by-word streamed deltas. Real
//! adapters do the same dance but call an HTTP/WS/SSE provider or spawn a CLI
//! (e.g. claude-code) in place of the echo.

use std::io::{BufRead, Write};

use mothership_adapter_host::protocol::{Model, Outbound, Request};

fn main() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut stdout = std::io::stdout();
    let mut line = String::new();

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break; // host closed stdin
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        match serde_json::from_str::<Request>(trimmed)? {
            Request::Initialize { id } => emit(&mut stdout, &Outbound::Ack { id })?,
            Request::GetIdentity { id } => emit(
                &mut stdout,
                &Outbound::Identity {
                    id,
                    provider_id: "echo".to_string(),
                    provider_label: "Echo Provider".to_string(),
                },
            )?,
            Request::GetModels { id } => emit(
                &mut stdout,
                &Outbound::Models {
                    id,
                    models: vec![Model {
                        id: "echo-1".to_string(),
                        label: "Echo 1".to_string(),
                        recommended: true,
                    }],
                },
            )?,
            Request::ChatStart { id, messages, .. } => {
                let last = messages
                    .last()
                    .map(|message| message.content.clone())
                    .unwrap_or_default();
                for word in last.split_whitespace() {
                    emit(&mut stdout, &Outbound::Delta {
                        id,
                        text: format!("{word} "),
                    })?;
                }
                emit(&mut stdout, &Outbound::Done { id })?;
            }
            Request::ChatCancel { id } => emit(&mut stdout, &Outbound::Done { id })?,
        }
    }

    Ok(())
}

fn emit(out: &mut impl Write, message: &Outbound) -> anyhow::Result<()> {
    let line = serde_json::to_string(message)?;
    out.write_all(line.as_bytes())?;
    out.write_all(b"\n")?;
    out.flush()?;
    Ok(())
}
