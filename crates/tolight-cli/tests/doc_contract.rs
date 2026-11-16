use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

fn request_limits(value: &Value, limits: &mut Vec<u64>) {
    match value {
        Value::Object(fields) => {
            if fields
                .get("handler")
                .is_some_and(|handler| handler == "request_body")
            {
                limits.push(
                    fields["max_size"]
                        .as_u64()
                        .expect("numeric Caddy request limit"),
                );
            }
            for child in fields.values() {
                request_limits(child, limits);
            }
        }
        Value::Array(children) => {
            for child in children {
                request_limits(child, limits);
            }
        }
        _ => (),
    }
}

#[tokio::test]
async fn public_proxy_accepts_the_full_32_kib_protocol_body_limit() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let binary = std::env::var_os("TOLIGHT_CADDY_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let local = root.join(".local/tools/caddy");
            if local.is_file() {
                local
            } else {
                PathBuf::from("caddy")
            }
        });
    let mut command = tokio::process::Command::new(binary);
    command
        .arg("adapt")
        .arg("--config")
        .arg(root.join("deploy/Caddyfile.example"))
        .current_dir(&root)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("TOLIGHT_") {
            command.env_remove(name);
        }
    }
    let result = tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .expect("Caddy adapter deadline")
        .expect("Caddy must be prepared by the development environment");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let adapted: Value = serde_json::from_slice(&result.stdout).unwrap();
    let mut limits = vec![];
    request_limits(&adapted, &mut limits);
    assert_eq!(
        limits,
        [32 * 1024],
        "decimal KB must not reject valid protocol bodies"
    );
}
