//! Platform bridge client (Rust → Flutter → Kotlin) for actions Rust cannot do:
//! SAF/MediaStore writes, SAF URI reads.
//!
//! Protocol: Rust pushes a [`PlatformCall`] onto the channel; `wdmm-ffi` turns it
//! into a `platform_request` queue event. Dart performs the platform action and
//! answers with `wdmm_queue_platform_result(req_id, result_json)`. The executor
//! awaits the oneshot, so a task simply parks in `AwaitingPlatform` until the
//! answer (or [`PLATFORM_TIMEOUT`]) arrives.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

/// How long a job waits for the Flutter/Kotlin side before failing the task.
pub const PLATFORM_TIMEOUT: Duration = Duration::from_secs(300);

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

fn next_request_id() -> String {
    let n = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    format!("pr-{n}")
}

/// One platform action. Serialised as `{"action":"save_to_tree", ...}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PlatformAction {
    /// Save a finished temp file into the user-picked SAF tree.
    SaveToTree {
        temp_path: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
    },
    /// Save a finished temp file into the system gallery (MediaStore, images/video).
    SaveToGallery {
        temp_path: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
    },
    /// Save a finished temp file into the public Downloads collection.
    SaveToDownloads {
        temp_path: String,
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mime: Option<String>,
    },
    /// Copy a SAF URI into app-private storage and return the local path.
    Materialize { uri: String },
}

/// A pending platform request; `wdmm-ffi` forwards it to Dart and completes
/// `reply` when Dart calls back.
pub struct PlatformCall {
    pub id: String,
    pub action: PlatformAction,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

/// Handle used by executors to ask the platform layer for something.
#[derive(Clone)]
pub struct PlatformClient {
    tx: mpsc::Sender<PlatformCall>,
}

impl PlatformClient {
    pub fn new(tx: mpsc::Sender<PlatformCall>) -> Self {
        Self { tx }
    }

    /// Creates a client plus the receiver the FFI layer drains.
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<PlatformCall>) {
        let (tx, rx) = mpsc::channel(capacity.max(1));
        (Self { tx }, rx)
    }

    /// A client whose requests always fail — for headless tests and for jobs
    /// that should not need the platform (cache downloads).
    pub fn disabled() -> Self {
        let (tx, mut rx) = mpsc::channel::<PlatformCall>(1);
        tokio::spawn(async move {
            while let Some(call) = rx.recv().await {
                let _ = call.reply.send(Err("platform unavailable".into()));
            }
        });
        Self { tx }
    }

    /// Ask the platform layer to perform `action` and wait for its answer.
    pub async fn request(&self, action: PlatformAction) -> Result<Value, String> {
        let (reply, rx) = oneshot::channel();
        let call = PlatformCall {
            id: next_request_id(),
            action,
            reply,
        };
        self.tx
            .send(call)
            .await
            .map_err(|_| "platform channel closed".to_string())?;
        match tokio::time::timeout(PLATFORM_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err("platform reply dropped".into()),
            Err(_) => Err("timeout.platform".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_round_trips_through_the_channel() {
        let (client, mut rx) = PlatformClient::channel(4);
        let handle = tokio::spawn(async move {
            if let Some(call) = rx.recv().await {
                let _ = call.reply.send(Ok(serde_json::json!({"uri": "content://x"})));
            }
        });
        let reply = client
            .request(PlatformAction::SaveToTree {
                temp_path: "/tmp/a.flac".into(),
                name: "a.flac".into(),
                mime: Some("audio/flac".into()),
            })
            .await
            .expect("reply");
        assert_eq!(reply["uri"], "content://x");
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn disabled_client_fails_requests() {
        let client = PlatformClient::disabled();
        let err = client
            .request(PlatformAction::Materialize {
                uri: "content://y".into(),
            })
            .await
            .expect_err("must fail");
        assert!(err.contains("platform unavailable"));
    }

    #[test]
    fn actions_serialise_with_a_tagged_action_field() {
        let value = serde_json::to_value(PlatformAction::Materialize {
            uri: "content://z".into(),
        })
        .unwrap();
        assert_eq!(value["action"], "materialize");
        assert_eq!(value["uri"], "content://z");
    }
}
