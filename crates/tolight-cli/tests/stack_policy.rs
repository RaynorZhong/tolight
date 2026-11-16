use serde_json::Value;
use std::{fs, path::Path};

fn visit(root: &Path, directory: &Path, violations: &mut Vec<String>) {
    for entry in fs::read_dir(directory).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let relative = path.strip_prefix(root).unwrap();
        let name = entry.file_name();
        if [
            ".git",
            ".local",
            ".agents",
            ".codex",
            "target",
            "node_modules",
            "public-dist",
            "__pycache__",
        ]
        .iter()
        .any(|excluded| name == *excluded)
        {
            continue;
        }
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            visit(root, &path, violations);
        } else if kind.is_file() {
            let allowed = match path.extension().and_then(|value| value.to_str()) {
                Some("py") => matches!(
                    relative.to_str(),
                    Some("examples/python/minimal_client.py" | "examples/python/check_vectors.py")
                ),
                Some("js" | "mjs" | "cjs" | "jsx") => relative == Path::new("public/app.js"),
                _ => true,
            };
            if !allowed {
                violations.push(relative.display().to_string());
            }
            if path.extension().is_some_and(|extension| extension == "sh") {
                let source = fs::read_to_string(&path).unwrap();
                if source.to_lowercase().contains("python")
                    && relative != Path::new("scripts/quality-gate.sh")
                {
                    violations.push(format!("{} starts a Python helper", relative.display()));
                }
            }
        }
    }
}

#[test]
fn repository_keeps_rust_and_typescript_with_only_independent_signing_python() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut violations = vec![];
    visit(&root, &root, &mut violations);
    assert!(violations.is_empty(), "stack drift: {violations:#?}");
    for source in ["frontend/app.ts", "tests/web/app.test.ts"] {
        let source = fs::read_to_string(root.join(source)).unwrap();
        assert!(!source.contains("@ts-nocheck"));
    }
    let package: Value =
        serde_json::from_str(&fs::read_to_string(root.join("package.json")).unwrap()).unwrap();
    assert!(
        package.get("dependencies").is_none(),
        "the static page has no runtime npm dependencies"
    );
    let development = package["devDependencies"].as_object().unwrap();
    assert_eq!(development.len(), 2);
    for dependency in ["typescript", "@types/node"] {
        let version = development[dependency].as_str().unwrap();
        assert!(
            version
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_digit()),
            "pin {dependency}"
        );
    }
    let config: Value =
        serde_json::from_str(&fs::read_to_string(root.join("tsconfig.json")).unwrap()).unwrap();
    assert_eq!(config["compilerOptions"]["strict"], true);
    assert_ne!(config["compilerOptions"]["noCheck"], true);
    assert!(root.join("package-lock.json").is_file());
    assert!(
        fs::read_to_string(root.join(".gitignore"))
            .unwrap()
            .lines()
            .any(|line| line == "/public/app.js")
    );
}
