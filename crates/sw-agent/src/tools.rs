//! The agent's tools: their schemas and their implementations.
//!
//! Every tool is bound to the run's site. The model cannot name another site for a write,
//! and every path it gives goes through the site store's sandbox.

use serde_json::{Value, json};
use sw_llm::{ToolCall, ToolDef};
use sw_sites::{Author, DomainChecker, SiteError, SiteStore};

/// Largest file `read_file` returns to the model, in bytes.
const MAX_READ_BYTES: usize = 200 * 1024;

/// What a tool needs to run.
#[derive(Debug, Clone)]
pub struct ToolContext {
    pub store: SiteStore,
    pub domains: DomainChecker,
    /// The site this run is bound to.
    pub site: String,
    /// Whether the run was started to create `site` (so `create_site` is allowed).
    pub may_create: bool,
    /// Public HTTP port used by `check_domain`.
    pub http_port: u16,
}

/// The outcome of one tool call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    /// JSON text handed back to the model.
    pub content: String,
    pub is_error: bool,
    /// Something the run itself must act on.
    pub effect: Option<Effect>,
}

/// Side effects the runner handles on the agent's behalf.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// The agent asked to delete the site; a person has to confirm.
    ConfirmDelete,
    /// The site was created, so the run is no longer allowed to create one.
    Created,
}

impl ToolOutcome {
    fn ok(value: Value) -> Self {
        Self {
            content: value.to_string(),
            is_error: false,
            effect: None,
        }
    }

    fn error(message: impl Into<String>) -> Self {
        Self {
            content: json!({ "error": message.into() }).to_string(),
            is_error: true,
            effect: None,
        }
    }
}

fn schema(properties: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false })
}

/// The tool definitions sent to the model.
pub fn definitions() -> Vec<ToolDef> {
    let def = |name: &str, description: &str, parameters: Value| ToolDef {
        name: name.to_owned(),
        description: description.to_owned(),
        parameters,
    };
    let none = || schema(json!({}), &[]);
    let path = json!({ "type": "string", "description": "Path relative to the site root, for example css/site.css" });
    vec![
        def(
            "list_sites",
            "List all sites on this server with their URLs.",
            none(),
        ),
        def(
            "site_info",
            "Show this site's title, hostnames, URL and release history.",
            none(),
        ),
        def(
            "create_site",
            "Create this conversation's site. Only needed (and only allowed) when the site does not exist yet.",
            schema(
                json!({
                    "name": { "type": "string", "description": "The site name given in the instructions" },
                    "title": { "type": "string", "description": "Short human-readable title, for example \"Washington Hikes\"" },
                }),
                &["name", "title"],
            ),
        ),
        def(
            "list_files",
            "List the files in the site's working tree with their sizes.",
            none(),
        ),
        def(
            "read_file",
            "Read a text file from the working tree.",
            schema(json!({ "path": path }), &["path"]),
        ),
        def(
            "write_file",
            "Create or replace a file in the working tree with the complete new content. Not live until publish_site.",
            schema(
                json!({ "path": path, "content": { "type": "string", "description": "The whole file" } }),
                &["path", "content"],
            ),
        ),
        def(
            "delete_file",
            "Delete a file from the working tree.",
            schema(json!({ "path": path }), &["path"]),
        ),
        def(
            "validate_site",
            "Check the working tree: index.html exists, links and assets resolve, sizes are within limits. Returns problems to fix.",
            none(),
        ),
        def(
            "publish_site",
            "Validate, commit and make the working tree live. Refused if validation finds errors. Rolls back automatically if the new release cannot be served.",
            schema(
                json!({ "message": { "type": "string", "description": "One line describing this release" } }),
                &["message"],
            ),
        ),
        def(
            "rollback_site",
            "Switch the live site to an earlier release and reset the working tree to it.",
            schema(
                json!({ "release": { "type": "string", "description": "Commit sha (or prefix) of the release; omit for the one before the current release" } }),
                &[],
            ),
        ),
        def(
            "delete_site",
            "Ask the user to confirm deleting this site. Does not delete anything itself: the site is removed only after the user confirms.",
            none(),
        ),
        def(
            "check_domain",
            "Check whether a hostname resolves in DNS and whether requests for it reach this server.",
            schema(json!({ "hostname": { "type": "string" } }), &["hostname"]),
        ),
    ]
}

/// Run one tool call. Never panics and never returns an `Err`: every failure becomes an
/// error result the model can read and react to.
pub async fn execute(ctx: &ToolContext, call: &ToolCall) -> ToolOutcome {
    if let Some(raw) = call.arguments.get("_invalid_json") {
        return ToolOutcome::error(format!(
            "the arguments were not valid JSON (they may have been cut off): {}",
            crate::events::shorten(raw.as_str().unwrap_or(""))
        ));
    }
    let args = &call.arguments;
    let text = |key: &str| args.get(key).and_then(Value::as_str);
    let required = |key: &str| {
        text(key)
            .ok_or_else(|| ToolOutcome::error(format!("missing required string argument {key:?}")))
    };

    let result: Result<ToolOutcome, ToolOutcome> = async {
        Ok(match call.name.as_str() {
            "list_sites" => {
                let sites = ctx.store.list(false).await.map_err(fail)?;
                let rows: Vec<Value> = sites
                    .iter()
                    .map(|s| {
                        json!({
                            "name": s.name.as_str(),
                            "title": s.title,
                            "published": s.current_release.is_some(),
                            "url": ctx.store.domains().public_url(&ctx.store.domains().site_hostname(&s.name)),
                        })
                    })
                    .collect();
                ToolOutcome::ok(json!({ "sites": rows }))
            }
            "site_info" => {
                let info = ctx.store.info(&ctx.site).await.map_err(fail)?;
                let releases: Vec<Value> = info
                    .releases
                    .iter()
                    .take(20)
                    .map(|r| json!({ "sha": &r.sha[..r.sha.len().min(8)], "message": r.message, "author": r.author, "live": Some(r.id) == info.site.current_release }))
                    .collect();
                ToolOutcome::ok(json!({
                    "name": info.site.name.as_str(),
                    "title": info.site.title,
                    "url": info.url,
                    "hostnames": info.hostnames.iter().map(|h| h.hostname.as_str()).collect::<Vec<_>>(),
                    "published": info.site.current_release.is_some(),
                    "releases": releases,
                }))
            }
            "create_site" => {
                let name = required("name")?;
                let title = required("title")?;
                if !ctx.may_create {
                    return Err(ToolOutcome::error(format!("the site {:?} already exists; edit it with the file tools", ctx.site)));
                }
                if name != ctx.site {
                    return Err(ToolOutcome::error(format!("this conversation can only create the site named {:?}", ctx.site)));
                }
                ctx.store.create(name, title, Author::Agent).await.map_err(fail)?;
                let info = ctx.store.info(name).await.map_err(fail)?;
                let mut outcome = ToolOutcome::ok(json!({ "created": name, "url": info.url, "next": "write the files, then validate_site and publish_site" }));
                outcome.effect = Some(Effect::Created);
                outcome
            }
            "list_files" => {
                let files = ctx.store.list_files(&ctx.site).await.map_err(fail)?;
                ToolOutcome::ok(json!({ "files": files }))
            }
            "read_file" => {
                let path = required("path")?;
                let data = ctx.store.read_file(&ctx.site, path).await.map_err(fail)?;
                if data.len() > MAX_READ_BYTES {
                    return Err(ToolOutcome::error(format!("{path} is {} bytes, too large to read", data.len())));
                }
                match String::from_utf8(data) {
                    Ok(content) => ToolOutcome::ok(json!({ "path": path, "content": content })),
                    Err(_) => ToolOutcome::error(format!("{path} is a binary file and cannot be read as text")),
                }
            }
            "write_file" => {
                let path = required("path")?;
                let content = required("content")?;
                let entry = ctx.store.write_file(&ctx.site, path, content.as_bytes().to_vec()).await.map_err(fail)?;
                ToolOutcome::ok(json!({ "written": entry.path, "bytes": entry.size }))
            }
            "delete_file" => {
                let path = required("path")?;
                ctx.store.delete_file(&ctx.site, path).await.map_err(fail)?;
                ToolOutcome::ok(json!({ "deleted": path }))
            }
            "validate_site" => {
                let report = ctx.store.validate(&ctx.site).await.map_err(fail)?;
                ToolOutcome::ok(json!({
                    "ok": report.ok(),
                    "files": report.files,
                    "bytes": report.bytes,
                    "problems": report.problems,
                    "next": if report.ok() { "ready to publish" } else { "fix every error, then validate again" },
                }))
            }
            "publish_site" => {
                let message = text("message").unwrap_or("Update site");
                let outcome = ctx.store.publish(&ctx.site, message, Author::Agent).await.map_err(fail)?;
                ToolOutcome::ok(json!({
                    "published": outcome.changed,
                    "note": if outcome.changed { "the new release is live" } else { "nothing changed since the live release" },
                    "release": &outcome.release.sha[..outcome.release.sha.len().min(8)],
                    "url": outcome.url,
                }))
            }
            "rollback_site" => {
                let release = ctx.store.rollback(&ctx.site, text("release"), Author::Agent).await.map_err(fail)?;
                ToolOutcome::ok(json!({
                    "live_release": &release.sha[..release.sha.len().min(8)],
                    "message": release.message,
                    "note": "the working tree now matches this release",
                }))
            }
            "delete_site" => {
                // Check the site exists, so the user is not asked about nothing.
                ctx.store.get(&ctx.site).await.map_err(fail)?;
                let mut outcome = ToolOutcome::ok(json!({
                    "status": "needs_confirmation",
                    "note": "The user has been asked to confirm by typing the site name. Nothing has been deleted. \
                             Tell them: the site goes offline at once, can be restored for a few days, and its DNS records are left in place.",
                }));
                outcome.effect = Some(Effect::ConfirmDelete);
                outcome
            }
            "check_domain" => {
                let hostname = required("hostname")?;
                ToolOutcome::ok(serde_json::to_value(ctx.domains.check(hostname, ctx.http_port).await).unwrap_or(Value::Null))
            }
            other => ToolOutcome::error(format!("there is no tool named {other:?}")),
        })
    }
    .await;
    result.unwrap_or_else(|outcome| outcome)
}

/// Turn a store error into a result the model can act on. Server-side failures are
/// logged in full but described to the model only in general terms.
fn fail(error: SiteError) -> ToolOutcome {
    if error.is_user_error() || matches!(error, SiteError::HealthCheck { .. }) {
        ToolOutcome::error(error.to_string())
    } else {
        tracing::error!(%error, "tool failed with a server error");
        ToolOutcome::error(
            "the server hit an internal error running this tool; try once more, then stop and tell the user",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_has_a_strict_object_schema() {
        let defs = definitions();
        let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "list_sites",
                "site_info",
                "create_site",
                "list_files",
                "read_file",
                "write_file",
                "delete_file",
                "validate_site",
                "publish_site",
                "rollback_site",
                "delete_site",
                "check_domain"
            ]
        );
        for def in &defs {
            assert_eq!(def.parameters["type"], "object", "{}", def.name);
            assert_eq!(
                def.parameters["additionalProperties"], false,
                "{}",
                def.name
            );
            assert!(!def.description.is_empty());
            for required in def.parameters["required"].as_array().unwrap() {
                assert!(
                    def.parameters["properties"]
                        .get(required.as_str().unwrap())
                        .is_some(),
                    "{}",
                    def.name
                );
            }
        }
        // The agent must never get a shell or a network tool.
        for forbidden in ["shell", "exec", "bash", "fetch", "http", "browse"] {
            assert!(!names.iter().any(|n| n.contains(forbidden)), "{forbidden}");
        }
    }
}
