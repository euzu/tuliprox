//! Run a local program with the event JSON on stdin.
//!
//! The escape hatch that means nobody has to wait for a channel to be added
//! upstream. This runs arbitrary code as the tuliprox process user, so it is
//! opt-in and never configured by default.
//!
//! The program is executed directly, never through a shell, so there are no
//! quoting rules to get wrong and no shell-injection surface from event
//! content.

use crate::channel::{ChannelCapabilities, Delivery, NotificationChannel, RenderedMessage, SendFuture};
use log::debug;
use shared::model::notification::{EventId, Severity};
use tokio::io::AsyncWriteExt;
use tuliprox_core::model::{ChannelRouting, CommandMessagingConfig};

pub struct CommandChannel {
    config: CommandMessagingConfig,
}

impl CommandChannel {
    pub fn new(config: CommandMessagingConfig) -> Self { Self { config } }
}

impl NotificationChannel for CommandChannel {
    fn id(&self) -> &'static str { "command" }

    fn template_for(&self, event: EventId) -> Option<&str> { self.config.templates.get(&event).map(String::as_str) }

    fn routing(&self) -> &ChannelRouting { &self.config.routing }

    fn wants(&self, event: EventId, severity: Severity) -> bool { self.config.routing.accepts(event, severity) }

    fn send<'a>(&'a self, msg: &'a RenderedMessage<'a>) -> SendFuture<'a> {
        Box::pin(async move {
            // Without a template, hand over the whole event as JSON so a
            // script can pick out whatever it needs.
            let payload = if msg.templated {
                msg.body.clone()
            } else {
                serde_json::to_string(msg.event).unwrap_or_else(|_| msg.body.clone())
            };

            let mut command = tokio::process::Command::new(&self.config.program);
            command
                .args(&self.config.args)
                // Useful for a script that only wants to branch on the event.
                .env("TULIPROX_EVENT_ID", msg.event.id.as_str())
                .env("TULIPROX_EVENT_SEVERITY", msg.event.severity.wire_name())
                .env("TULIPROX_EVENT_TITLE", &msg.event.title)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped());

            let mut child = match command.spawn() {
                Ok(child) => child,
                // A missing binary or a bad permission bit will fail
                // identically forever, so retrying is pointless.
                Err(err) => return Delivery::permanent(format!("could not run `{}`: {err}", self.config.program)),
            };

            if let Some(mut stdin) = child.stdin.take() {
                if let Err(err) = stdin.write_all(payload.as_bytes()).await {
                    return Delivery::retry(format!("could not write to `{}` stdin: {err}", self.config.program));
                }
                // Dropping closes the pipe, so the child sees EOF and can
                // finish instead of blocking on a read forever.
                drop(stdin);
            }

            match tokio::time::timeout(self.config.timeout, child.wait_with_output()).await {
                Ok(Ok(output)) if output.status.success() => {
                    debug!("Notification handled by `{}`", self.config.program);
                    Delivery::Delivered
                }
                Ok(Ok(output)) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let stderr = stderr.trim();
                    Delivery::retry(format!(
                        "`{}` exited with {}: {}",
                        self.config.program,
                        output.status,
                        if stderr.is_empty() { "(no stderr)" } else { stderr }
                    ))
                }
                Ok(Err(err)) => Delivery::retry(format!("`{}` failed: {err}", self.config.program)),
                Err(_) => Delivery::retry(format!(
                    "`{}` did not finish within {}s",
                    self.config.program,
                    self.config.timeout.as_secs()
                )),
            }
        })
    }

    fn capabilities(&self) -> ChannelCapabilities { ChannelCapabilities::default() }
}
