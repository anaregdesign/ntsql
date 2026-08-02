use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode};

use ntsql_contract::{FixtureArtifact, LegalReviewLedger, ProvenanceLedger};
use serde_json::Value;

const CARGO_CYCLONEDX_VERSION: &str = "0.5.9";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("governance check failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args().skip(1);
    let command = arguments
        .next()
        .ok_or_else(|| invalid_data("expected `fixtures` or `sbom <path>`"))?;

    match command.as_str() {
        "fixtures" => {
            ensure_no_more_arguments(arguments)?;
            validate_repository_fixtures()
        }
        "sbom" => {
            let path = arguments
                .next()
                .ok_or_else(|| invalid_data("the sbom command requires a path"))?;
            ensure_no_more_arguments(arguments)?;
            validate_sbom(Path::new(&path))
        }
        _ => Err(invalid_data("expected `fixtures` or `sbom <path>`")),
    }
}

fn ensure_no_more_arguments(
    mut arguments: impl Iterator<Item = String>,
) -> Result<(), Box<dyn Error>> {
    if arguments.next().is_some() {
        return Err(invalid_data("unexpected additional argument"));
    }
    Ok(())
}

fn validate_repository_fixtures() -> Result<(), Box<dyn Error>> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let workspace_root = workspace_root.canonicalize()?;
    let provenance: ProvenanceLedger =
        read_json(&workspace_root.join("contracts/compatibility/provenance.json"))?;
    let legal_reviews: LegalReviewLedger =
        read_json(&workspace_root.join("contracts/compatibility/legal-reviews.json"))?;
    let fixture_paths = discover_fixture_files(&workspace_root)?;
    let fixtures = fixture_paths
        .iter()
        .map(|path| {
            Ok(FixtureArtifact {
                artifact_path: repository_relative_path(&workspace_root, path)?,
                content_digest: sha256_digest(path)?,
            })
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;

    let violations = legal_reviews
        .validate()
        .into_iter()
        .chain(provenance.validate(&legal_reviews))
        .chain(provenance.validate_fixture_inventory(&legal_reviews, &fixtures))
        .collect::<Vec<_>>();

    if !violations.is_empty() {
        for violation in &violations {
            eprintln!("{}: {}", violation.code, violation.message);
        }
        return Err(invalid_data(format!(
            "fixture governance found {} violation(s)",
            violations.len()
        )));
    }

    println!("fixture governance ok ({} fixture files)", fixtures.len());
    Ok(())
}

fn discover_fixture_files(workspace_root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut pending_directories = vec![workspace_root.to_path_buf()];
    let mut fixture_paths = Vec::new();

    while let Some(directory) = pending_directories.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;

            if file_type.is_dir() {
                if directory == workspace_root && is_ignored_root_directory(&entry.file_name()) {
                    continue;
                }
                pending_directories.push(path);
            } else if is_fixture_path(workspace_root, &path) {
                if !file_type.is_file() {
                    return Err(invalid_data(format!(
                        "fixture path must be a regular file: {}",
                        path.display()
                    )));
                }
                fixture_paths.push(path);
            }
        }
    }

    fixture_paths.sort();
    Ok(fixture_paths)
}

fn is_ignored_root_directory(name: &OsStr) -> bool {
    name == OsStr::new(".git") || name == OsStr::new(".vscode") || name == OsStr::new("target")
}

fn is_fixture_path(workspace_root: &Path, path: &Path) -> bool {
    path.strip_prefix(workspace_root).is_ok_and(|relative| {
        relative.components().any(|component| {
            matches!(component, Component::Normal(name) if name == OsStr::new("fixtures"))
        })
    })
}

fn repository_relative_path(workspace_root: &Path, path: &Path) -> Result<String, Box<dyn Error>> {
    let relative = path.strip_prefix(workspace_root)?;
    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid_data("fixture paths must be valid UTF-8")),
            _ => Err(invalid_data("fixture paths must be repository relative")),
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    Ok(components.join("/"))
}

fn sha256_digest(path: &Path) -> Result<String, Box<dyn Error>> {
    if let Some(digest) = run_digest_command("sha256sum", &["--"], path)? {
        return Ok(digest);
    }
    if let Some(digest) = run_digest_command("shasum", &["-a", "256", "--"], path)? {
        return Ok(digest);
    }
    Err(invalid_data("fixture hashing requires sha256sum or shasum"))
}

fn run_digest_command(
    program: &str,
    arguments: &[&str],
    path: &Path,
) -> Result<Option<String>, Box<dyn Error>> {
    let output = match Command::new(program).args(arguments).arg(path).output() {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Box::new(error)),
    };

    if !output.status.success() {
        return Err(invalid_data(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let stdout = String::from_utf8(output.stdout)?;
    let digest = stdout
        .split_whitespace()
        .next()
        .ok_or_else(|| invalid_data(format!("{program} returned no digest")))?;
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(invalid_data(format!(
            "{program} returned an invalid SHA-256 digest"
        )));
    }

    Ok(Some(format!("sha256:{}", digest.to_ascii_lowercase())))
}

fn validate_sbom(path: &Path) -> Result<(), Box<dyn Error>> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(invalid_data("SBOM must be a non-empty regular file"));
    }

    let sbom: Value = read_json(path)?;
    require_string(&sbom, "/bomFormat", "CycloneDX")?;
    require_string(&sbom, "/specVersion", "1.5")?;
    require_nonempty_string(&sbom, "/serialNumber")?;
    require_nonempty_string(&sbom, "/metadata/timestamp")?;

    let tools = require_array(&sbom, "/metadata/tools")?;
    if !tools.iter().any(|tool| {
        tool.get("name").and_then(Value::as_str) == Some("cargo-cyclonedx")
            && tool.get("version").and_then(Value::as_str) == Some(CARGO_CYCLONEDX_VERSION)
    }) {
        return Err(invalid_data(format!(
            "SBOM must identify cargo-cyclonedx {CARGO_CYCLONEDX_VERSION}"
        )));
    }

    let components = require_array(&sbom, "/components")?;
    if components.is_empty() {
        return Err(invalid_data("SBOM must contain dependency components"));
    }
    for component in components {
        let name = component
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid_data("every SBOM component requires a name"))?;
        component
            .get("version")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid_data(format!("SBOM component {name} requires a version")))?;
        let licenses = component
            .get("licenses")
            .and_then(Value::as_array)
            .filter(|values| !values.is_empty())
            .ok_or_else(|| invalid_data(format!("SBOM component {name} requires a license")))?;
        if !licenses.iter().all(valid_license_entry) {
            return Err(invalid_data(format!(
                "SBOM component {name} has an invalid license entry"
            )));
        }
        let hashes = component
            .get("hashes")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid_data(format!("SBOM component {name} requires hashes")))?;
        if !hashes.iter().any(valid_sha256_entry) {
            return Err(invalid_data(format!(
                "SBOM component {name} requires a SHA-256 hash"
            )));
        }
    }

    println!("SBOM governance ok ({} components)", components.len());
    Ok(())
}

fn require_string(value: &Value, pointer: &str, expected: &str) -> Result<(), Box<dyn Error>> {
    if value.pointer(pointer).and_then(Value::as_str) != Some(expected) {
        return Err(invalid_data(format!(
            "SBOM {pointer} must equal {expected}"
        )));
    }
    Ok(())
}

fn require_nonempty_string<'a>(value: &'a Value, pointer: &str) -> Result<&'a str, Box<dyn Error>> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|candidate| !candidate.trim().is_empty())
        .ok_or_else(|| invalid_data(format!("SBOM {pointer} must be a non-empty string")))
}

fn require_array<'a>(value: &'a Value, pointer: &str) -> Result<&'a [Value], Box<dyn Error>> {
    value
        .pointer(pointer)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| invalid_data(format!("SBOM {pointer} must be an array")))
}

fn valid_license_entry(value: &Value) -> bool {
    value
        .get("expression")
        .and_then(Value::as_str)
        .is_some_and(|expression| !expression.trim().is_empty())
        || value
            .pointer("/license/id")
            .and_then(Value::as_str)
            .is_some_and(|identifier| !identifier.trim().is_empty())
}

fn valid_sha256_entry(value: &Value) -> bool {
    value.get("alg").and_then(Value::as_str) == Some("SHA-256")
        && value
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|digest| {
                digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, Box<dyn Error>> {
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn invalid_data(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}
