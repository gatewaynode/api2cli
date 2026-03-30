use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::process::ChildStdin;
use tokio::sync::Mutex;
use tracing::debug;

#[derive(Clone, Debug)]
pub enum Passthrough {
    Body,
    Full,
}

impl std::str::FromStr for Passthrough {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "body" => Ok(Passthrough::Body),
            "full" => Ok(Passthrough::Full),
            other => Err(format!("invalid passthrough mode '{other}': expected 'body' or 'full'")),
        }
    }
}

#[derive(Clone)]
pub enum Forwarder {
    Stdout,
    Subprocess(Arc<Mutex<ChildStdin>>),
}

impl Forwarder {
    pub async fn forward(
        &self,
        passthrough: &Passthrough,
        method: &str,
        path: &str,
        headers: HashMap<String, String>,
        body: Vec<u8>,
    ) -> Result<(), std::io::Error> {
        let payload = build_payload(passthrough, method, path, headers, body);

        let n = payload.len();
        match self {
            Forwarder::Stdout => {
                let mut stdout = tokio::io::stdout();
                stdout.write_all(&payload).await?;
                stdout.flush().await?;
                debug!("Forwarded {n} bytes to stdout");
            }
            Forwarder::Subprocess(stdin) => {
                let mut stdin = stdin.lock().await;
                stdin.write_all(&payload).await?;
                stdin.flush().await?;
                debug!("Forwarded {n} bytes to subprocess stdin");
            }
        }
        Ok(())
    }
}

pub(crate) fn build_payload(
    passthrough: &Passthrough,
    method: &str,
    path: &str,
    headers: HashMap<String, String>,
    body: Vec<u8>,
) -> Vec<u8> {
    match passthrough {
        Passthrough::Body => {
            let mut out = body;
            out.push(b'\n');
            out
        }
        Passthrough::Full => {
            let body_str = String::from_utf8_lossy(&body).to_string();
            let envelope = serde_json::json!({
                "method": method,
                "path": path,
                "headers": headers,
                "body": body_str,
            });
            let mut out = envelope.to_string().into_bytes();
            out.push(b'\n');
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Passthrough::from_str
    // -----------------------------------------------------------------------

    #[test]
    fn test_passthrough_parse_body() {
        let p: Passthrough = "body".parse().unwrap();
        assert!(matches!(p, Passthrough::Body));
    }

    #[test]
    fn test_passthrough_parse_full() {
        let p: Passthrough = "full".parse().unwrap();
        assert!(matches!(p, Passthrough::Full));
    }

    #[test]
    fn test_passthrough_parse_invalid_names_error() {
        let err = "json".parse::<Passthrough>().unwrap_err();
        assert!(err.contains("json"), "error should name the bad value");
        assert!(err.contains("body") && err.contains("full"),
            "error should list valid options");
    }

    #[test]
    fn test_passthrough_parse_empty_string_errors() {
        assert!("".parse::<Passthrough>().is_err());
    }

    // -----------------------------------------------------------------------
    // build_payload — body mode
    // -----------------------------------------------------------------------

    #[test]
    fn test_body_passthrough_is_raw_body_plus_newline() {
        let result = build_payload(
            &Passthrough::Body, "POST", "/x", HashMap::new(), b"hello".to_vec(),
        );
        assert_eq!(result, b"hello\n");
    }

    #[test]
    fn test_body_passthrough_empty_body_is_just_newline() {
        let result = build_payload(
            &Passthrough::Body, "GET", "/", HashMap::new(), vec![],
        );
        assert_eq!(result, b"\n");
    }

    #[test]
    fn test_body_passthrough_does_not_include_method_or_path() {
        let result = build_payload(
            &Passthrough::Body, "DELETE", "/should/not/appear", HashMap::new(), b"data".to_vec(),
        );
        let text = String::from_utf8(result).unwrap();
        assert!(!text.contains("DELETE"));
        assert!(!text.contains("/should/not/appear"));
    }

    // -----------------------------------------------------------------------
    // build_payload — full mode
    // -----------------------------------------------------------------------

    #[test]
    fn test_full_passthrough_is_valid_json_with_newline() {
        let result = build_payload(
            &Passthrough::Full, "POST", "/api", HashMap::new(), b"body".to_vec(),
        );
        assert_eq!(*result.last().unwrap(), b'\n');
        let v: serde_json::Value = serde_json::from_slice(&result[..result.len() - 1]).unwrap();
        assert_eq!(v["method"], "POST");
        assert_eq!(v["path"], "/api");
        assert_eq!(v["body"], "body");
    }

    #[test]
    fn test_full_passthrough_includes_headers() {
        let mut headers = HashMap::new();
        headers.insert("content-type".to_string(), "application/json".to_string());
        let result = build_payload(
            &Passthrough::Full, "PUT", "/h", headers, b"{}".to_vec(),
        );
        let v: serde_json::Value =
            serde_json::from_slice(&result[..result.len() - 1]).unwrap();
        assert_eq!(v["headers"]["content-type"], "application/json");
    }

    #[test]
    fn test_full_passthrough_empty_body_field_is_empty_string() {
        let result = build_payload(
            &Passthrough::Full, "GET", "/ping", HashMap::new(), vec![],
        );
        let v: serde_json::Value =
            serde_json::from_slice(&result[..result.len() - 1]).unwrap();
        assert_eq!(v["body"], "");
    }
}
