//! `sitewright prompt`: start an agent run and follow it in the terminal.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};
use sw_llm::sse::SseParser;

use crate::client::AdminClient;

/// Start a run, print its progress as it happens, and handle a delete confirmation if
/// the agent asked for one. Fails unless the run succeeds.
pub async fn run(
    client: &AdminClient,
    site: &str,
    prompt: &str,
    new: bool,
    provider: Option<&str>,
) -> anyhow::Result<()> {
    let run: Value = client
        .post(
            "/v1/runs",
            &json!({ "site": site, "prompt": prompt, "new": new, "provider": provider }),
        )
        .await?;
    let id = run["id"]
        .as_i64()
        .context("the daemon did not return a run id")?;

    let response = client.stream(&format!("/v1/runs/{id}/events")).await?;
    let mut body = response.bytes_stream();
    let mut parser = SseParser::new();
    let mut printer = Printer::default();
    let mut interrupted = false;
    'follow: loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c(), if !interrupted => {
                signal?;
                interrupted = true;
                eprintln!("\ncancelling (the run stops after its current step)...");
                let _: Value = client.post(&format!("/v1/runs/{id}/cancel"), &json!({})).await?;
            }
            chunk = body.next() => {
                let Some(chunk) = chunk else { break 'follow };
                let chunk = chunk.context("lost the connection to the daemon")?;
                for event in parser.feed(&chunk) {
                    if let Ok(value) = serde_json::from_str::<Value>(&event.data)
                        && printer.print(&value)
                    {
                        break 'follow;
                    }
                }
            }
        }
    }

    let run: Value = client.get(&format!("/v1/runs/{id}")).await?;
    let cost = run["cost_usd"]
        .as_f64()
        .map(|c| format!(", ${c:.4}"))
        .unwrap_or_default();
    eprintln!(
        "\n[run {id}: {} after {} model calls, {} tokens in, {} out{cost}]",
        run["status"].as_str().unwrap_or("?"),
        run["turns"],
        run["input_tokens"],
        run["output_tokens"],
    );

    if printer.confirm_delete {
        confirm_delete(client, site).await?;
    }
    match run["status"].as_str() {
        Some("succeeded") => Ok(()),
        Some(status) => bail!(
            "the run ended as {status}: {}",
            run["message"].as_str().unwrap_or("")
        ),
        None => bail!("the run's status is unknown"),
    }
}

async fn confirm_delete(client: &AdminClient, site: &str) -> anyhow::Result<()> {
    if !std::io::stdin().is_terminal() {
        eprintln!(
            "The agent asked to delete {site}. Nothing was deleted; confirm with `sitewright site delete {site}`."
        );
        return Ok(());
    }
    print!(
        "\nThe agent asks to delete {site}. It goes offline at once and can be restored for a few days.\n\
         Type the site name to confirm, or press Enter to keep it: "
    );
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    if answer.trim() == site {
        let _: Value = client
            .post(
                &format!("/v1/sites/{site}/delete"),
                &json!({ "confirm": site }),
            )
            .await?;
        println!("Deleted {site}. Undo with `sitewright site restore {site}`.");
    } else {
        println!("Kept {site}.");
    }
    Ok(())
}

/// Renders run events as terminal output.
#[derive(Debug, Default)]
struct Printer {
    /// Whether text deltas have been printed for the current turn.
    streaming_text: bool,
    confirm_delete: bool,
}

impl Printer {
    /// Print one event. Returns true when the run is over.
    fn print(&mut self, event: &Value) -> bool {
        let field = |key: &str| event[key].as_str().unwrap_or("");
        match field("type") {
            "started" => eprintln!(
                "[{} on {} / {}]",
                field("site"),
                field("provider"),
                field("model")
            ),
            "text_delta" => {
                print!("{}", field("delta"));
                let _ = std::io::stdout().flush();
                self.streaming_text = true;
            }
            "text" => {
                if self.streaming_text {
                    println!();
                } else {
                    println!("{}", field("text"));
                }
                self.streaming_text = false;
            }
            "tool_call" => {
                self.end_line();
                let args = &event["arguments"];
                let detail = ["path", "message", "hostname", "title", "release"]
                    .iter()
                    .find_map(|key| args[*key].as_str())
                    .map(|v| format!(" {v}"))
                    .unwrap_or_default();
                eprintln!("  > {}{detail}", field("name"));
            }
            "tool_result" => {
                if event["is_error"].as_bool() == Some(true) {
                    let content: Value =
                        serde_json::from_str(field("content")).unwrap_or(Value::Null);
                    eprintln!(
                        "    ! {}",
                        content["error"].as_str().unwrap_or(field("content"))
                    );
                }
            }
            "confirmation" => self.confirm_delete = field("action") == "delete_site",
            "done" => {
                self.end_line();
                if field("status") != "succeeded" {
                    eprintln!("{}", field("message"));
                }
                return true;
            }
            _ => {}
        }
        false
    }

    fn end_line(&mut self) {
        if self.streaming_text {
            println!();
            self.streaming_text = false;
        }
    }
}
