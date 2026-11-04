//! Slice 10: queue retry policy edge (`wdmm_queue_retry_json`).
//!
//! Dart asks once per failed task attempt; Rust answers with the verdict and
//! the numbers to persist. The rules themselves live in
//! `wdmm_queue::policy` — this file only parses the JSON request and serialises
//! the reply.
//!
//! Request (`dart-cutover-slices.md` §Slice 10):
//!
//! ```text
//! {"attempts":n,"in_background":bool,"category":"dio|vfs_data|state|other",
//!  "message":"...","cancelled":bool,"jitter_ms":n}
//! ```
//!
//! Every member is optional: `attempts` defaults to 0, `category` to `other`,
//! `message` to empty, the booleans to false and `jitter_ms` to 0.
//!
//! Reply:
//!
//! ```text
//! {"ok":true,"action":"cancel|retry|fail","attempts":n,"delay_ms":m,
//!  "offline":bool,"cap":n}
//! ```

use std::os::raw::c_char;

use serde_json::{json, Value};
use wdmm_queue::policy::{decide, FailureCategory, RetryDecision, RetryInput};

use super::json_reply;
use crate::{ptr_to_str, FfiString};

/// Parse the request JSON into a [`RetryInput`].
fn parse_input(json: Option<Value>) -> Result<RetryInput, String> {
    let value = json.unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let object = value
        .as_object()
        .ok_or_else(|| "input json must be an object".to_string())?;

    let attempts = match object.get("attempts") {
        None | Some(Value::Null) => 0,
        Some(raw) => raw
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| "attempts must be a non-negative integer".to_string())?,
    };
    let in_background = match object.get("in_background") {
        None | Some(Value::Null) => false,
        Some(raw) => raw
            .as_bool()
            .ok_or_else(|| "in_background must be a bool".to_string())?,
    };
    let category = match object.get("category") {
        None | Some(Value::Null) => FailureCategory::Other,
        Some(raw) => {
            let name = raw
                .as_str()
                .ok_or_else(|| "category must be a string".to_string())?;
            FailureCategory::parse(name)
                .ok_or_else(|| format!("unknown failure category: {name}"))?
        }
    };
    let message = match object.get("message") {
        None | Some(Value::Null) => String::new(),
        Some(raw) => raw
            .as_str()
            .ok_or_else(|| "message must be a string".to_string())?
            .to_string(),
    };
    let cancelled = match object.get("cancelled") {
        None | Some(Value::Null) => false,
        Some(raw) => raw
            .as_bool()
            .ok_or_else(|| "cancelled must be a bool".to_string())?,
    };
    let jitter_ms = match object.get("jitter_ms") {
        None | Some(Value::Null) => 0,
        Some(raw) => raw
            .as_u64()
            .ok_or_else(|| "jitter_ms must be a non-negative integer".to_string())?,
    };

    Ok(RetryInput {
        attempts,
        in_background,
        category,
        message,
        cancelled,
        jitter_ms,
    })
}

/// Wire shape of one decision.
fn decision_value(decision: &RetryDecision) -> Value {
    json!({
        "ok": true,
        "action": decision.action.as_str(),
        "attempts": decision.attempts,
        "delay_ms": decision.delay_ms,
        "offline": decision.offline,
        "cap": decision.cap,
    })
}

/// Decide what to do with one failed queue attempt.
#[no_mangle]
pub extern "C" fn wdmm_queue_retry_json(input_json: *const c_char) -> FfiString {
    let parsed = match ptr_to_str(input_json) {
        None => Value::Null,
        Some("") => Value::Null,
        Some(text) => match serde_json::from_str::<Value>(text) {
            Ok(value) => value,
            Err(e) => return FfiString::from(format!("error:invalid input json: {e}")),
        },
    };
    match parse_input(Some(parsed)) {
        Ok(input) => json_reply(Ok(decision_value(&decide(&input)))),
        Err(e) => FfiString::from(format!("error:{e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide_from(text: &str) -> Value {
        let input = parse_input(Some(serde_json::from_str(text).unwrap())).unwrap();
        decision_value(&decide(&input))
    }

    #[test]
    fn retry_reply_carries_the_new_count_and_the_backoff() {
        let value = decide_from(
            r#"{"attempts":0,"in_background":false,"category":"dio","message":"timeout"}"#,
        );
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["action"], json!("retry"));
        assert_eq!(value["attempts"], json!(1));
        assert_eq!(value["delay_ms"], json!(2000));
        assert_eq!(value["offline"], json!(false));
        assert_eq!(value["cap"], json!(5));
    }

    #[test]
    fn jitter_is_folded_into_the_delay() {
        let value = decide_from(r#"{"attempts":1,"message":"timeout","jitter_ms":750}"#);
        assert_eq!(value["delay_ms"], json!(4750));
    }

    #[test]
    fn background_offline_reply_carries_the_three_attempt_cap() {
        let value = decide_from(
            r#"{"attempts":1,"in_background":true,"category":"other",
                "message":"Failed host lookup: nas.local"}"#,
        );
        assert_eq!(value["action"], json!("retry"));
        assert_eq!(value["attempts"], json!(2));
        assert_eq!(value["delay_ms"], json!(10000));
        assert_eq!(value["offline"], json!(true));
        assert_eq!(value["cap"], json!(3));
    }

    #[test]
    fn exhausted_and_cancelled_replies_stay_on_the_old_count() {
        let exhausted = decide_from(r#"{"attempts":5,"category":"dio","message":"timeout"}"#);
        assert_eq!(exhausted["action"], json!("fail"));
        assert_eq!(exhausted["attempts"], json!(5));
        assert_eq!(exhausted["delay_ms"], json!(0));

        let cancelled = decide_from(r#"{"attempts":2,"cancelled":true}"#);
        assert_eq!(cancelled["action"], json!("cancel"));
        assert_eq!(cancelled["attempts"], json!(2));
        assert_eq!(cancelled["delay_ms"], json!(0));
    }

    #[test]
    fn missing_members_fall_back_to_documented_defaults() {
        let value = decide_from("{}");
        assert_eq!(value["action"], json!("retry"));
        assert_eq!(value["attempts"], json!(1));
        assert_eq!(value["cap"], json!(5));
    }

    #[test]
    fn malformed_requests_are_rejected_by_name() {
        assert_eq!(
            parse_input(Some(
                serde_json::from_str(r#"{"category":"bogus"}"#).unwrap()
            ))
            .unwrap_err(),
            "unknown failure category: bogus"
        );
        assert_eq!(
            parse_input(Some(serde_json::from_str(r#"{"attempts":-1}"#).unwrap())).unwrap_err(),
            "attempts must be a non-negative integer"
        );
        assert_eq!(
            parse_input(Some(
                serde_json::from_str(r#"{"in_background":1}"#).unwrap()
            ))
            .unwrap_err(),
            "in_background must be a bool"
        );
        assert!(parse_input(Some(json!([]))).is_err());
    }
}
