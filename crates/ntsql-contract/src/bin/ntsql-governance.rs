use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

use ntsql_contract::{
    BehaviorSpecificationAdmissionLedger, FeatureMatrix, FixtureArtifact,
    ImplementationAdmissionContext, LegalDecisionAuthority, LegalDecisionVerificationContext,
    LegalReviewLedger, ProvenanceLedger, ProvenanceSourceKind, ProvenanceUse,
    SpecificationReviewAuthority, SpecificationReviewVerificationContext, TargetMatrix,
};
use serde_json::Value;

const CARGO_CYCLONEDX_VERSION: &str = "0.5.9";
const CRATES_IO_REGISTRY_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";
const RUST_DIST_MANIFEST_PREFIX: &str = "https://static.rust-lang.org/dist/channel-rust-";
const REQUIRED_TOOLCHAIN_COMPONENTS: [&str; 2] = ["clippy", "rustfmt"];
const REQUIRED_TOOLCHAIN_PROFILE: &str = "minimal";

struct AuthorityInput {
    path: PathBuf,
    candidate_repository: String,
    candidate_commit_sha: String,
}

struct ImplementationAuthorityInput {
    legal_path: PathBuf,
    specification_path: PathBuf,
    candidate_repository: String,
    candidate_commit_sha: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemoteArchiveKind {
    GovernanceTool,
    GitHubAction,
    ToolchainManifest,
}

impl RemoteArchiveKind {
    fn description(self) -> &'static str {
        match self {
            Self::GovernanceTool => "governance tool",
            Self::GitHubAction => "GitHub Action",
            Self::ToolchainManifest => "Rust toolchain manifest",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PinnedToolchain {
    channel: String,
    profile: String,
    components: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpectedRemoteArchive {
    kind: RemoteArchiveKind,
    description: String,
    source_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RemoteArchive {
    kind: RemoteArchiveKind,
    record_id: String,
    source_url: String,
    content_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LockedPackage {
    name: String,
    version: String,
    source: Option<String>,
    checksum: Option<String>,
}

#[derive(Default)]
struct LockedPackageBuilder {
    name: Option<String>,
    version: Option<String>,
    source: Option<String>,
    checksum: Option<String>,
}

impl LockedPackageBuilder {
    fn finish(self) -> Result<LockedPackage, Box<dyn Error>> {
        Ok(LockedPackage {
            name: self
                .name
                .ok_or_else(|| invalid_data("Cargo.lock package is missing its name"))?,
            version: self
                .version
                .ok_or_else(|| invalid_data("Cargo.lock package is missing its version"))?,
            source: self.source,
            checksum: self.checksum,
        })
    }
}

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
    let command = arguments.next().ok_or_else(|| {
        invalid_data(
            "expected `fixtures [<authority> <repository> <commit>]`, `implementation-admission <feature> <target> [<legal-authority> <specification-authority> <repository> <commit>]`, `legal-reviews <authority> <repository> <commit>`, `provenance-offline`, `provenance-online`, or `sbom <path>`",
        )
    })?;

    match command.as_str() {
        "fixtures" => {
            let authority_input = parse_optional_authority(arguments, "fixtures")?;
            validate_repository_fixtures(authority_input.as_ref())
        }
        "legal-reviews" => {
            let authority_path = arguments.next().ok_or_else(|| {
                invalid_data("the legal-reviews command requires an authority path")
            })?;
            let candidate_repository = arguments.next().ok_or_else(|| {
                invalid_data("the legal-reviews command requires a trusted repository")
            })?;
            let candidate_commit_sha = arguments.next().ok_or_else(|| {
                invalid_data("the legal-reviews command requires a trusted commit")
            })?;
            ensure_no_more_arguments(arguments)?;
            validate_legal_reviews(
                Path::new(&authority_path),
                &candidate_repository,
                &candidate_commit_sha,
            )
        }
        "implementation-admission" => {
            let feature_id = arguments.next().ok_or_else(|| {
                invalid_data("implementation-admission requires an exact feature id")
            })?;
            let target_id = arguments.next().ok_or_else(|| {
                invalid_data("implementation-admission requires an exact target id")
            })?;
            let authority_input = parse_optional_implementation_authorities(arguments)?;
            validate_implementation_admission(&feature_id, &target_id, authority_input.as_ref())
        }
        "provenance-offline" => {
            ensure_no_more_arguments(arguments)?;
            validate_offline_provenance()
        }
        "provenance-online" => {
            ensure_no_more_arguments(arguments)?;
            validate_online_provenance()
        }
        "sbom" => {
            let path = arguments
                .next()
                .ok_or_else(|| invalid_data("the sbom command requires a path"))?;
            ensure_no_more_arguments(arguments)?;
            validate_sbom(Path::new(&path))
        }
        _ => Err(invalid_data(
            "expected `fixtures [<authority> <repository> <commit>]`, `implementation-admission <feature> <target> [<legal-authority> <specification-authority> <repository> <commit>]`, `legal-reviews <authority> <repository> <commit>`, `provenance-offline`, `provenance-online`, or `sbom <path>`",
        )),
    }
}

fn parse_optional_authority(
    mut arguments: impl Iterator<Item = String>,
    command: &str,
) -> Result<Option<AuthorityInput>, Box<dyn Error>> {
    let authority_path = arguments.next();
    let candidate_repository = arguments.next();
    let candidate_commit_sha = arguments.next();
    ensure_no_more_arguments(arguments)?;
    match (authority_path, candidate_repository, candidate_commit_sha) {
        (None, None, None) => Ok(None),
        (Some(path), Some(candidate_repository), Some(candidate_commit_sha)) => {
            Ok(Some(AuthorityInput {
                path: PathBuf::from(path),
                candidate_repository,
                candidate_commit_sha,
            }))
        }
        _ => Err(invalid_data(format!(
            "{command} requires authority, repository, and commit together"
        ))),
    }
}

fn parse_optional_implementation_authorities(
    mut arguments: impl Iterator<Item = String>,
) -> Result<Option<ImplementationAuthorityInput>, Box<dyn Error>> {
    let legal_path = arguments.next();
    let specification_path = arguments.next();
    let candidate_repository = arguments.next();
    let candidate_commit_sha = arguments.next();
    ensure_no_more_arguments(arguments)?;
    match (
        legal_path,
        specification_path,
        candidate_repository,
        candidate_commit_sha,
    ) {
        (None, None, None, None) => Ok(None),
        (
            Some(legal_path),
            Some(specification_path),
            Some(candidate_repository),
            Some(candidate_commit_sha),
        ) => Ok(Some(ImplementationAuthorityInput {
            legal_path: PathBuf::from(legal_path),
            specification_path: PathBuf::from(specification_path),
            candidate_repository,
            candidate_commit_sha,
        })),
        _ => Err(invalid_data(
            "implementation-admission requires legal authority, specification authority, repository, and commit together",
        )),
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

fn validate_repository_fixtures(
    authority_input: Option<&AuthorityInput>,
) -> Result<(), Box<dyn Error>> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let workspace_root = workspace_root.canonicalize()?;
    let provenance: ProvenanceLedger =
        read_json(&workspace_root.join("contracts/compatibility/provenance.json"))?;
    let legal_reviews: LegalReviewLedger =
        read_json(&workspace_root.join("contracts/compatibility/legal-reviews.json"))?;
    let authority = authority_input
        .map(|input| read_external_authority(&workspace_root, &input.path))
        .transpose()?;
    let verification = authority_input
        .zip(authority.as_ref())
        .map(|(input, authority)| LegalDecisionVerificationContext {
            authority,
            candidate_repository: &input.candidate_repository,
            candidate_commit_sha: &input.candidate_commit_sha,
        });
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
        .validate_for_governed_use(&provenance, verification)
        .into_iter()
        .chain(provenance.validate(&legal_reviews))
        .chain(provenance.validate_fixture_inventory(&legal_reviews, verification, &fixtures))
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

fn validate_legal_reviews(
    authority_path: &Path,
    candidate_repository: &str,
    candidate_commit_sha: &str,
) -> Result<(), Box<dyn Error>> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let legal_reviews: LegalReviewLedger =
        read_json(&workspace_root.join("contracts/compatibility/legal-reviews.json"))?;
    let provenance: ProvenanceLedger =
        read_json(&workspace_root.join("contracts/compatibility/provenance.json"))?;
    let authority = read_external_authority(&workspace_root, authority_path)?;
    let violations = legal_reviews.validate_authenticated_decisions(
        &provenance,
        LegalDecisionVerificationContext {
            authority: &authority,
            candidate_repository,
            candidate_commit_sha,
        },
    );

    if !violations.is_empty() {
        for violation in &violations {
            eprintln!("{}: {}", violation.code, violation.message);
        }
        return Err(invalid_data(format!(
            "authenticated legal-review governance found {} violation(s)",
            violations.len()
        )));
    }

    println!("authenticated legal-review governance ok");
    Ok(())
}

fn validate_implementation_admission(
    feature_id: &str,
    target_id: &str,
    authority_input: Option<&ImplementationAuthorityInput>,
) -> Result<(), Box<dyn Error>> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let admissions: BehaviorSpecificationAdmissionLedger = read_json(
        &workspace_root.join("contracts/compatibility/behavior-specification-admissions.json"),
    )?;
    let targets: TargetMatrix =
        read_json(&workspace_root.join("contracts/compatibility/targets.json"))?;
    let features: FeatureMatrix =
        read_json(&workspace_root.join("contracts/compatibility/features.json"))?;
    let provenance: ProvenanceLedger =
        read_json(&workspace_root.join("contracts/compatibility/provenance.json"))?;
    let legal_reviews: LegalReviewLedger =
        read_json(&workspace_root.join("contracts/compatibility/legal-reviews.json"))?;
    let legal_authority = authority_input
        .map(|input| read_external_authority(&workspace_root, &input.legal_path))
        .transpose()?;
    let specification_authority = authority_input
        .map(|input| {
            read_external_specification_authority(&workspace_root, &input.specification_path)
        })
        .transpose()?;
    let legal_verification =
        authority_input
            .zip(legal_authority.as_ref())
            .map(|(input, authority)| LegalDecisionVerificationContext {
                authority,
                candidate_repository: &input.candidate_repository,
                candidate_commit_sha: &input.candidate_commit_sha,
            });
    let specification_review_verification = authority_input
        .zip(specification_authority.as_ref())
        .map(
            |(input, authority)| SpecificationReviewVerificationContext {
                authority,
                candidate_repository: &input.candidate_repository,
                candidate_commit_sha: &input.candidate_commit_sha,
            },
        );
    let violations = features.validate_implementation_inputs(
        feature_id,
        target_id,
        ImplementationAdmissionContext {
            targets: &targets,
            admissions: &admissions,
            provenance: &provenance,
            legal_reviews: &legal_reviews,
            legal_verification,
            specification_review_verification,
        },
    );

    if !violations.is_empty() {
        for violation in &violations {
            eprintln!("{}: {}", violation.code, violation.message);
        }
        return Err(invalid_data(format!(
            "implementation admission found {} violation(s)",
            violations.len()
        )));
    }

    println!("implementation admission is valid for {feature_id} on {target_id}");
    Ok(())
}

fn validate_offline_provenance() -> Result<(), Box<dyn Error>> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let provenance: ProvenanceLedger =
        read_json(&workspace_root.join("contracts/compatibility/provenance.json"))?;
    let repository_artifacts = verify_repository_artifacts(&workspace_root, &provenance)?;
    let direct_dependencies = verify_direct_dependency_provenance(&workspace_root, &provenance)?;
    let toolchain = validate_pinned_toolchain(&workspace_root)?;
    let workflow = fs::read_to_string(workspace_root.join(".github/workflows/governance.yml"))?;
    let remote_archives = resolve_remote_archive_records(&workflow, &toolchain, &provenance)?;

    println!(
        "offline provenance ok ({repository_artifacts} repository artifacts, {direct_dependencies} direct dependencies, {} remote archives, Rust {})",
        remote_archives.len(),
        toolchain.channel
    );
    Ok(())
}

fn verify_repository_artifacts(
    workspace_root: &Path,
    provenance: &ProvenanceLedger,
) -> Result<usize, Box<dyn Error>> {
    let mut verified = 0;
    for record in &provenance.records {
        let Some(artifact_path) = record.artifact_path.as_deref() else {
            continue;
        };
        verify_repository_artifact(workspace_root, artifact_path, &record.content_digest)
            .map_err(|error| invalid_data(format!("provenance {}: {error}", record.id)))?;
        verified += 1;
    }
    Ok(verified)
}

fn verify_repository_artifact(
    workspace_root: &Path,
    artifact_path: &str,
    expected_digest: &str,
) -> Result<(), Box<dyn Error>> {
    let resolved_path = resolve_repository_artifact(workspace_root, artifact_path)?;
    let actual_digest = sha256_digest(&resolved_path)?;
    if !actual_digest.eq_ignore_ascii_case(expected_digest) {
        return Err(invalid_data(format!(
            "repository artifact {artifact_path} digest mismatch: expected {expected_digest}, found {actual_digest}"
        )));
    }
    Ok(())
}

fn resolve_repository_artifact(
    workspace_root: &Path,
    artifact_path: &str,
) -> Result<PathBuf, Box<dyn Error>> {
    let relative_path = Path::new(artifact_path);
    if artifact_path.is_empty()
        || artifact_path.contains('\\')
        || relative_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(invalid_data(format!(
            "repository artifact path must contain only relative normal components: {artifact_path}"
        )));
    }

    let workspace_root = workspace_root.canonicalize()?;
    let mut resolved_path = workspace_root.clone();
    for component in relative_path.components() {
        let Component::Normal(name) = component else {
            return Err(invalid_data(
                "repository artifact path changed during validation",
            ));
        };
        resolved_path.push(name);
        let metadata = fs::symlink_metadata(&resolved_path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "repository artifact {artifact_path} cannot be inspected at {}: {error}",
                    resolved_path.display()
                ),
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(invalid_data(format!(
                "repository artifact path must not contain symlinks: {artifact_path}"
            )));
        }
    }

    let metadata = fs::metadata(&resolved_path)?;
    if !metadata.is_file() {
        return Err(invalid_data(format!(
            "repository artifact must be a regular file: {artifact_path}"
        )));
    }
    let resolved_path = resolved_path.canonicalize()?;
    if !resolved_path.starts_with(&workspace_root) {
        return Err(invalid_data(format!(
            "repository artifact resolves outside the workspace: {artifact_path}"
        )));
    }
    Ok(resolved_path)
}

fn verify_direct_dependency_provenance(
    workspace_root: &Path,
    provenance: &ProvenanceLedger,
) -> Result<usize, Box<dyn Error>> {
    let metadata = read_cargo_metadata(workspace_root)?;
    let lockfile = fs::read_to_string(workspace_root.join("Cargo.lock"))?;
    verify_direct_dependency_records(&metadata, &lockfile, provenance)
}

fn verify_direct_dependency_records(
    cargo_metadata: &Value,
    lockfile: &str,
    provenance: &ProvenanceLedger,
) -> Result<usize, Box<dyn Error>> {
    let direct_dependencies = direct_registry_dependencies(cargo_metadata)?;
    let locked_packages = parse_locked_packages(lockfile)?;
    let provenance_records = provenance
        .records
        .iter()
        .filter(|record| {
            record
                .intended_uses
                .contains(&ProvenanceUse::DependencyInclusion)
        })
        .collect::<Vec<_>>();
    let mut matched_record_ids = BTreeSet::new();

    for dependency in &direct_dependencies {
        let matching_packages = locked_packages
            .iter()
            .filter(|package| {
                package.name == *dependency
                    && package.source.as_deref() == Some(CRATES_IO_REGISTRY_SOURCE)
            })
            .collect::<Vec<_>>();
        let package = match matching_packages.as_slice() {
            [package] => *package,
            [] => {
                return Err(invalid_data(format!(
                    "direct dependency {dependency} has no exact crates.io package in Cargo.lock"
                )));
            }
            _ => {
                return Err(invalid_data(format!(
                    "direct dependency {dependency} resolves to multiple crates.io packages in Cargo.lock"
                )));
            }
        };
        let checksum = package.checksum.as_deref().ok_or_else(|| {
            invalid_data(format!(
                "direct dependency {dependency} has no Cargo.lock checksum"
            ))
        })?;
        if !is_sha256_hex(checksum) {
            return Err(invalid_data(format!(
                "direct dependency {dependency} has an invalid Cargo.lock checksum"
            )));
        }

        let expected_url = crates_io_archive_url(&package.name, &package.version)?;
        let matching_records = provenance_records
            .iter()
            .copied()
            .filter(|record| record.source_url.as_deref() == Some(expected_url.as_str()))
            .collect::<Vec<_>>();
        let record = match matching_records.as_slice() {
            [record] => *record,
            [] => {
                return Err(invalid_data(format!(
                    "direct dependency {} {} has no provenance record for {expected_url}",
                    package.name, package.version
                )));
            }
            _ => {
                return Err(invalid_data(format!(
                    "direct dependency {} {} has multiple provenance records",
                    package.name, package.version
                )));
            }
        };

        if record.source_kind != ProvenanceSourceKind::Dependency {
            return Err(invalid_data(format!(
                "direct dependency provenance {} must use the dependency source kind",
                record.id
            )));
        }
        let expected_digest = format!("sha256:{checksum}");
        if !record.content_digest.eq_ignore_ascii_case(&expected_digest) {
            return Err(invalid_data(format!(
                "direct dependency provenance {} does not match the Cargo.lock checksum for {} {}",
                record.id, package.name, package.version
            )));
        }
        if !matched_record_ids.insert(record.id.as_str()) {
            return Err(invalid_data(format!(
                "direct dependency provenance {} was matched more than once",
                record.id
            )));
        }
    }

    if let Some(record) = provenance_records
        .iter()
        .find(|record| !matched_record_ids.contains(record.id.as_str()))
    {
        return Err(invalid_data(format!(
            "unknown direct dependency provenance record: {}",
            record.id
        )));
    }

    Ok(direct_dependencies.len())
}

fn read_cargo_metadata(workspace_root: &Path) -> Result<Value, Box<dyn Error>> {
    let cargo_program = option_env!("CARGO")
        .ok_or_else(|| invalid_data("the Cargo executable path is unavailable"))?;
    let output = Command::new(cargo_program)
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--locked",
            "--offline",
            "--manifest-path",
        ])
        .arg(workspace_root.join("Cargo.toml"))
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                invalid_data(format!(
                    "required Cargo metadata tool is unavailable: {cargo_program}"
                ))
            } else {
                Box::new(error) as Box<dyn Error>
            }
        })?;
    if !output.status.success() {
        return Err(invalid_data(format!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn direct_registry_dependencies(metadata: &Value) -> Result<BTreeSet<String>, Box<dyn Error>> {
    let workspace_members = metadata
        .get("workspace_members")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_data("cargo metadata requires workspace_members"))?
        .iter()
        .map(|member| {
            member
                .as_str()
                .ok_or_else(|| invalid_data("cargo metadata workspace member must be a string"))
        })
        .collect::<Result<BTreeSet<_>, Box<dyn Error>>>()?;
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_data("cargo metadata requires packages"))?;
    let mut dependencies = BTreeSet::new();

    for package in packages {
        let package_id = package
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_data("cargo metadata package requires an id"))?;
        if !workspace_members.contains(package_id) {
            continue;
        }
        let package_dependencies = package
            .get("dependencies")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                invalid_data(format!(
                    "cargo metadata workspace package {package_id} requires dependencies"
                ))
            })?;
        for dependency in package_dependencies {
            let source = dependency
                .get("source")
                .ok_or_else(|| invalid_data("cargo metadata dependency requires source"))?;
            match source {
                Value::Null => {}
                Value::String(source) if source == CRATES_IO_REGISTRY_SOURCE => {
                    let name = dependency
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            invalid_data("cargo metadata registry dependency requires a name")
                        })?;
                    validate_crate_name(name)?;
                    dependencies.insert(name.to_owned());
                }
                Value::String(source) => {
                    return Err(invalid_data(format!(
                        "direct dependency uses unsupported external source: {source}"
                    )));
                }
                _ => {
                    return Err(invalid_data(
                        "cargo metadata dependency source must be a string or null",
                    ));
                }
            }
        }
    }

    Ok(dependencies)
}

fn parse_toml_string(value: &str, description: &str) -> Result<String, Box<dyn Error>> {
    let literal = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| invalid_data(format!("{description} must be a literal string")))?;
    if literal.is_empty() || literal.contains('\\') {
        return Err(invalid_data(format!(
            "{description} must be a non-empty literal string"
        )));
    }
    Ok(literal.to_owned())
}

fn validate_pinned_toolchain(workspace_root: &Path) -> Result<PinnedToolchain, Box<dyn Error>> {
    match fs::symlink_metadata(workspace_root.join("rust-toolchain")) {
        Ok(_) => {
            return Err(invalid_data(
                "legacy rust-toolchain is forbidden because it can override rust-toolchain.toml",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let toolchain_document = fs::read_to_string(workspace_root.join("rust-toolchain.toml"))?;
    let toolchain = parse_pinned_toolchain(&toolchain_document)?;
    let cargo_manifest = fs::read_to_string(workspace_root.join("Cargo.toml"))?;
    validate_workspace_rust_version(&cargo_manifest, &toolchain.channel)?;
    Ok(toolchain)
}

fn parse_pinned_toolchain(document: &str) -> Result<PinnedToolchain, Box<dyn Error>> {
    let mut in_toolchain_section = false;
    let mut saw_toolchain_section = false;
    let mut channel = None;
    let mut profile = None;
    let mut components = None;

    for (line_index, source_line) in document.lines().enumerate() {
        let line = source_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            if line != "[toolchain]" || saw_toolchain_section {
                return Err(invalid_data(format!(
                    "rust-toolchain.toml line {} contains an unsupported or duplicate section",
                    line_index + 1
                )));
            }
            saw_toolchain_section = true;
            in_toolchain_section = true;
            continue;
        }
        if !in_toolchain_section {
            return Err(invalid_data(format!(
                "rust-toolchain.toml line {} appears outside [toolchain]",
                line_index + 1
            )));
        }
        let (key, value) = line.split_once('=').ok_or_else(|| {
            invalid_data(format!(
                "rust-toolchain.toml line {} requires key = value",
                line_index + 1
            ))
        })?;
        let key = key.trim();
        let value = value.trim();
        match key {
            "channel" => {
                let parsed = parse_toml_string(value, "rust-toolchain.toml channel")?;
                if channel.replace(parsed).is_some() {
                    return Err(invalid_data("rust-toolchain.toml repeats channel"));
                }
            }
            "profile" => {
                let parsed = parse_toml_string(value, "rust-toolchain.toml profile")?;
                if profile.replace(parsed).is_some() {
                    return Err(invalid_data("rust-toolchain.toml repeats profile"));
                }
            }
            "components" => {
                let parsed = parse_toml_string_array(value, "rust-toolchain.toml components")?;
                if components.replace(parsed).is_some() {
                    return Err(invalid_data("rust-toolchain.toml repeats components"));
                }
            }
            _ => {
                return Err(invalid_data(format!(
                    "rust-toolchain.toml contains unsupported key: {key}"
                )));
            }
        }
    }

    let channel = channel.ok_or_else(|| invalid_data("rust-toolchain.toml requires channel"))?;
    if !is_exact_rust_release(&channel) {
        return Err(invalid_data(
            "rust-toolchain.toml channel must be an exact stable x.y.z release",
        ));
    }
    let profile = profile.ok_or_else(|| invalid_data("rust-toolchain.toml requires profile"))?;
    if profile != REQUIRED_TOOLCHAIN_PROFILE {
        return Err(invalid_data(format!(
            "rust-toolchain.toml profile must be {REQUIRED_TOOLCHAIN_PROFILE}"
        )));
    }
    let components =
        components.ok_or_else(|| invalid_data("rust-toolchain.toml requires components"))?;
    if !components
        .iter()
        .map(String::as_str)
        .eq(REQUIRED_TOOLCHAIN_COMPONENTS)
    {
        return Err(invalid_data(
            "rust-toolchain.toml components must be exactly [\"clippy\", \"rustfmt\"]",
        ));
    }

    Ok(PinnedToolchain {
        channel,
        profile,
        components,
    })
}

fn parse_toml_string_array(value: &str, description: &str) -> Result<Vec<String>, Box<dyn Error>> {
    let inner = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .ok_or_else(|| invalid_data(format!("{description} must be a literal string array")))?;
    if inner.trim().is_empty() {
        return Ok(Vec::new());
    }
    inner
        .split(',')
        .map(|item| parse_toml_string(item.trim(), description))
        .collect()
}

fn is_exact_rust_release(value: &str) -> bool {
    let mut components = value.split('.');
    let Some(major) = components.next() else {
        return false;
    };
    let Some(minor) = components.next() else {
        return false;
    };
    let Some(patch) = components.next() else {
        return false;
    };
    components.next().is_none()
        && major == "1"
        && [minor, patch].iter().all(|component| {
            !component.is_empty()
                && (component.len() == 1 || !component.starts_with('0'))
                && component.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn validate_workspace_rust_version(
    cargo_manifest: &str,
    expected: &str,
) -> Result<(), Box<dyn Error>> {
    let mut in_workspace_package = false;
    let mut rust_version = None;

    for source_line in cargo_manifest.lines() {
        let line = source_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') {
            in_workspace_package = line == "[workspace.package]";
            continue;
        }
        if !in_workspace_package {
            continue;
        }
        let Some(value) = line.strip_prefix("rust-version") else {
            continue;
        };
        let value = value
            .trim_start()
            .strip_prefix('=')
            .ok_or_else(|| invalid_data("workspace rust-version requires ="))?
            .trim();
        let parsed = parse_toml_string(value, "workspace rust-version")?;
        if rust_version.replace(parsed).is_some() {
            return Err(invalid_data("workspace repeats rust-version"));
        }
    }

    let rust_version =
        rust_version.ok_or_else(|| invalid_data("workspace requires rust-version"))?;
    if rust_version != expected {
        return Err(invalid_data(format!(
            "workspace rust-version {rust_version} does not match pinned toolchain {expected}"
        )));
    }
    Ok(())
}

fn parse_locked_packages(lockfile: &str) -> Result<Vec<LockedPackage>, Box<dyn Error>> {
    let mut packages = Vec::new();
    let mut current: Option<LockedPackageBuilder> = None;

    for line in lockfile.lines().map(str::trim) {
        if line == "[[package]]" {
            if let Some(builder) = current.take() {
                packages.push(builder.finish()?);
            }
            current = Some(LockedPackageBuilder::default());
            continue;
        }
        let Some(builder) = current.as_mut() else {
            continue;
        };
        if let Some(value) = lockfile_string_field(line, "name")? {
            builder.name = Some(value);
        } else if let Some(value) = lockfile_string_field(line, "version")? {
            builder.version = Some(value);
        } else if let Some(value) = lockfile_string_field(line, "source")? {
            builder.source = Some(value);
        } else if let Some(value) = lockfile_string_field(line, "checksum")? {
            builder.checksum = Some(value);
        }
    }
    if let Some(builder) = current {
        packages.push(builder.finish()?);
    }
    if packages.is_empty() {
        return Err(invalid_data("Cargo.lock contains no package records"));
    }
    Ok(packages)
}

fn lockfile_string_field(line: &str, field: &str) -> Result<Option<String>, Box<dyn Error>> {
    let prefix = format!("{field} = ");
    let Some(value) = line.strip_prefix(&prefix) else {
        return Ok(None);
    };
    Ok(Some(parse_toml_string(
        value,
        &format!("Cargo.lock {field}"),
    )?))
}

fn crates_io_archive_url(name: &str, version: &str) -> Result<String, Box<dyn Error>> {
    validate_crate_name(name)?;
    if version.is_empty()
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
    {
        return Err(invalid_data(format!(
            "crate {name} has an invalid locked version: {version}"
        )));
    }
    Ok(format!(
        "https://static.crates.io/crates/{name}/{name}-{version}.crate"
    ))
}

fn validate_crate_name(name: &str) -> Result<(), Box<dyn Error>> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid_data(format!("invalid crate package name: {name}")));
    }
    Ok(())
}

fn validate_online_provenance() -> Result<(), Box<dyn Error>> {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let provenance: ProvenanceLedger =
        read_json(&workspace_root.join("contracts/compatibility/provenance.json"))?;
    let workflow = fs::read_to_string(workspace_root.join(".github/workflows/governance.yml"))?;
    let toolchain = validate_pinned_toolchain(&workspace_root)?;
    let archives = resolve_remote_archive_records(&workflow, &toolchain, &provenance)?;

    verify_remote_archives(OsStr::new("curl"), &archives)?;
    println!("online provenance ok ({} remote archives)", archives.len());
    Ok(())
}

fn resolve_remote_archive_records(
    workflow: &str,
    toolchain: &PinnedToolchain,
    provenance: &ProvenanceLedger,
) -> Result<Vec<RemoteArchive>, Box<dyn Error>> {
    let mut expected_archives = discover_workflow_archives(workflow)?;
    expected_archives.push(toolchain_manifest_archive(toolchain));
    let provenance_records = provenance
        .records
        .iter()
        .filter(|record| {
            record
                .intended_uses
                .contains(&ProvenanceUse::SupplyChainVerification)
        })
        .collect::<Vec<_>>();
    let mut matched_record_ids = BTreeSet::new();
    let mut archives = Vec::new();

    for expected in expected_archives {
        let matching_records = provenance_records
            .iter()
            .copied()
            .filter(|record| record.source_url.as_deref() == Some(expected.source_url.as_str()))
            .collect::<Vec<_>>();
        let record = match matching_records.as_slice() {
            [record] => *record,
            [] => {
                return Err(invalid_data(format!(
                    "{} {} has no provenance record for {}",
                    expected.kind.description(),
                    expected.description,
                    expected.source_url
                )));
            }
            _ => {
                return Err(invalid_data(format!(
                    "{} {} has multiple provenance records",
                    expected.kind.description(),
                    expected.description
                )));
            }
        };
        if record.source_kind != ProvenanceSourceKind::Dependency {
            return Err(invalid_data(format!(
                "remote archive provenance {} must use the dependency source kind",
                record.id
            )));
        }
        if !is_prefixed_sha256(&record.content_digest) {
            return Err(invalid_data(format!(
                "remote archive provenance {} has an invalid SHA-256 digest",
                record.id
            )));
        }
        if !matched_record_ids.insert(record.id.as_str()) {
            return Err(invalid_data(format!(
                "remote archive provenance {} was matched more than once",
                record.id
            )));
        }
        archives.push(RemoteArchive {
            kind: expected.kind,
            record_id: record.id.clone(),
            source_url: expected.source_url,
            content_digest: record.content_digest.clone(),
        });
    }

    if let Some(record) = provenance_records
        .iter()
        .find(|record| !matched_record_ids.contains(record.id.as_str()))
    {
        return Err(invalid_data(format!(
            "unknown supply-chain provenance record: {}",
            record.id
        )));
    }

    Ok(archives)
}

fn discover_workflow_archives(
    workflow: &str,
) -> Result<Vec<ExpectedRemoteArchive>, Box<dyn Error>> {
    let mut archives = Vec::new();
    let mut action_count = 0;
    let mut tool_count = 0;

    for (line_index, source_line) in workflow.lines().enumerate() {
        let line = source_line.trim();
        let line = match line.strip_prefix("- ") {
            Some(value) => value,
            None => line,
        };
        if let Some(reference) = line.strip_prefix("uses:") {
            archives.push(github_action_archive(reference.trim()).map_err(|error| {
                invalid_data(format!(
                    "governance workflow line {}: {error}",
                    line_index + 1
                ))
            })?);
            action_count += 1;
            continue;
        }
        let command_line = match line.strip_prefix("run:") {
            Some(value) => value.trim(),
            None => line,
        };
        if let Some(command) = command_line.strip_prefix("cargo install ") {
            archives.push(governance_tool_archive(command).map_err(|error| {
                invalid_data(format!(
                    "governance workflow line {}: {error}",
                    line_index + 1
                ))
            })?);
            tool_count += 1;
        }
    }

    if action_count == 0 || tool_count == 0 {
        return Err(invalid_data(
            "governance workflow must contain pinned third-party Actions and governance tools",
        ));
    }
    let mut urls = BTreeSet::new();
    for archive in &archives {
        if !urls.insert(archive.source_url.as_str()) {
            return Err(invalid_data(format!(
                "governance workflow repeats remote archive {}",
                archive.source_url
            )));
        }
    }
    Ok(archives)
}

fn toolchain_manifest_archive(toolchain: &PinnedToolchain) -> ExpectedRemoteArchive {
    ExpectedRemoteArchive {
        kind: RemoteArchiveKind::ToolchainManifest,
        description: format!(
            "Rust {} profile {} with {}",
            toolchain.channel,
            toolchain.profile,
            toolchain.components.join(",")
        ),
        source_url: format!("{RUST_DIST_MANIFEST_PREFIX}{}.toml", toolchain.channel),
    }
}

fn github_action_archive(reference: &str) -> Result<ExpectedRemoteArchive, Box<dyn Error>> {
    let (repository, revision) = reference
        .split_once('@')
        .filter(|(_, revision)| !revision.contains('@'))
        .ok_or_else(|| invalid_data("GitHub Action must use owner/repository@commit syntax"))?;
    let (owner, name) = repository
        .split_once('/')
        .filter(|(_, name)| !name.contains('/'))
        .ok_or_else(|| invalid_data("GitHub Action must name one owner and repository"))?;
    validate_github_component(owner)?;
    validate_github_component(name)?;
    if revision.len() != 40
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(invalid_data(format!(
            "GitHub Action {repository} must be pinned to a lowercase 40-character commit SHA"
        )));
    }

    Ok(ExpectedRemoteArchive {
        kind: RemoteArchiveKind::GitHubAction,
        description: reference.to_owned(),
        source_url: format!("https://codeload.github.com/{repository}/tar.gz/{revision}"),
    })
}

fn validate_github_component(value: &str) -> Result<(), Box<dyn Error>> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_data(format!(
            "invalid GitHub repository component: {value}"
        )));
    }
    Ok(())
}

fn governance_tool_archive(command: &str) -> Result<ExpectedRemoteArchive, Box<dyn Error>> {
    let tokens = command.split_whitespace().collect::<Vec<_>>();
    let [package, version_flag, version, locked_flag] = tokens.as_slice() else {
        return Err(invalid_data(
            "governance tool installation must use `<crate> --version <exact> --locked`",
        ));
    };
    if *version_flag != "--version" || *locked_flag != "--locked" {
        return Err(invalid_data(
            "governance tool installation must use `<crate> --version <exact> --locked`",
        ));
    }
    let source_url = crates_io_archive_url(package, version)?;
    Ok(ExpectedRemoteArchive {
        kind: RemoteArchiveKind::GovernanceTool,
        description: format!("{package} {version}"),
        source_url,
    })
}

fn verify_remote_archives(
    curl_program: &OsStr,
    archives: &[RemoteArchive],
) -> Result<(), Box<dyn Error>> {
    let download_root = create_temporary_directory("ntsql-provenance-downloads")?;
    let verification = archives
        .iter()
        .enumerate()
        .try_for_each(|(index, archive)| {
            let output_path = download_root.join(format!("archive-{index}"));
            download_and_verify_archive(curl_program, archive, &output_path)
        });
    let cleanup = fs::remove_dir_all(&download_root);

    if let Err(error) = verification {
        if let Err(cleanup_error) = cleanup {
            return Err(invalid_data(format!(
                "{error}; failed to remove temporary downloads at {}: {cleanup_error}",
                download_root.display()
            )));
        }
        return Err(error);
    }
    cleanup.map_err(|error| {
        invalid_data(format!(
            "failed to remove temporary downloads at {}: {error}",
            download_root.display()
        ))
    })?;
    Ok(())
}

fn download_and_verify_archive(
    curl_program: &OsStr,
    archive: &RemoteArchive,
    output_path: &Path,
) -> Result<(), Box<dyn Error>> {
    let output = Command::new(curl_program)
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-redirs",
            "0",
            "--connect-timeout",
            "15",
            "--max-time",
            "120",
            "--write-out",
            "%{http_code}\n%{url_effective}",
            "--output",
        ])
        .arg(output_path)
        .arg("--url")
        .arg(&archive.source_url)
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                invalid_data(format!(
                    "required download tool {} is unavailable",
                    Path::new(curl_program).display()
                ))
            } else {
                Box::new(error) as Box<dyn Error>
            }
        })?;
    if !output.status.success() {
        return Err(invalid_data(format!(
            "{} provenance {} download failed: {}",
            archive.kind.description(),
            archive.record_id,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let response = String::from_utf8(output.stdout)?;
    let mut lines = response.lines();
    let status = lines
        .next()
        .ok_or_else(|| invalid_data("curl returned no HTTP status"))?;
    let effective_url = lines
        .next()
        .ok_or_else(|| invalid_data("curl returned no effective URL"))?;
    if lines.next().is_some() {
        return Err(invalid_data("curl returned unexpected response metadata"));
    }
    if status != "200" {
        let reason = if status.starts_with('3') {
            "redirects are not permitted"
        } else {
            "expected HTTP 200"
        };
        return Err(invalid_data(format!(
            "{} provenance {} returned HTTP {status}; {reason}",
            archive.kind.description(),
            archive.record_id
        )));
    }
    if effective_url != archive.source_url {
        return Err(invalid_data(format!(
            "{} provenance {} resolved to unexpected origin or URL: {effective_url}",
            archive.kind.description(),
            archive.record_id
        )));
    }
    let metadata = fs::symlink_metadata(output_path)?;
    if !metadata.file_type().is_file() || metadata.len() == 0 {
        return Err(invalid_data(format!(
            "{} provenance {} returned no regular archive bytes",
            archive.kind.description(),
            archive.record_id
        )));
    }
    let actual_digest = sha256_digest(output_path)?;
    if !actual_digest.eq_ignore_ascii_case(&archive.content_digest) {
        return Err(invalid_data(format!(
            "{} provenance {} digest mismatch: expected {}, found {actual_digest}",
            archive.kind.description(),
            archive.record_id,
            archive.content_digest
        )));
    }
    Ok(())
}

fn create_temporary_directory(prefix: &str) -> Result<PathBuf, Box<dyn Error>> {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    for attempt in 0..100 {
        let path =
            env::temp_dir().join(format!("{prefix}-{}-{nonce}-{attempt}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(Box::new(error)),
        }
    }
    Err(invalid_data(format!(
        "could not create a unique temporary directory for {prefix}"
    )))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_prefixed_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(is_sha256_hex)
}

fn read_external_authority(
    workspace_root: &Path,
    path: &Path,
) -> Result<LegalDecisionAuthority, Box<dyn Error>> {
    read_external_json_authority(workspace_root, path, "legal-decision")
}

fn read_external_specification_authority(
    workspace_root: &Path,
    path: &Path,
) -> Result<SpecificationReviewAuthority, Box<dyn Error>> {
    read_external_json_authority(workspace_root, path, "specification-review")
}

fn read_external_json_authority<T: serde::de::DeserializeOwned>(
    workspace_root: &Path,
    path: &Path,
    label: &str,
) -> Result<T, Box<dyn Error>> {
    let supplied_path = normalize_absolute_path(path)?;
    if supplied_path.starts_with(workspace_root) {
        return Err(invalid_data(format!(
            "{label} authority path must originate outside the candidate checkout"
        )));
    }
    let resolved_path = path.canonicalize()?;
    if resolved_path.starts_with(workspace_root) {
        return Err(invalid_data(format!(
            "{label} authority target must be outside the candidate checkout"
        )));
    }
    if !fs::metadata(&resolved_path)?.is_file() {
        return Err(invalid_data(format!(
            "{label} authority must be a regular file"
        )));
    }
    read_json(&resolved_path)
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(invalid_data("authority path traverses above its root"));
                }
            }
        }
    }
    Ok(normalized)
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
    Err(invalid_data("SHA-256 hashing requires sha256sum or shasum"))
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
    let sbom_directory = path
        .parent()
        .ok_or_else(|| invalid_data("SBOM path requires a parent directory"))?
        .canonicalize()?;

    let sbom: Value = read_json(path)?;
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()?;
    let workspace_packages = workspace_package_inventory(&read_cargo_metadata(&workspace_root)?)?;
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
        validate_sbom_component(component, &workspace_packages, &sbom_directory)?;
    }

    println!("SBOM governance ok ({} components)", components.len());
    Ok(())
}

struct WorkspacePackageIdentity {
    name: String,
    version: String,
    source_directory: PathBuf,
}

fn workspace_package_inventory(
    metadata: &Value,
) -> Result<BTreeMap<String, WorkspacePackageIdentity>, Box<dyn Error>> {
    let workspace_members = metadata
        .get("workspace_members")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_data("cargo metadata requires workspace_members"))?;
    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_data("cargo metadata requires packages"))?;
    let mut inventory = BTreeMap::new();

    for member in workspace_members {
        let package_id = member
            .as_str()
            .ok_or_else(|| invalid_data("cargo metadata workspace member must be a string"))?;
        let package = packages
            .iter()
            .find(|package| package.get("id").and_then(Value::as_str) == Some(package_id))
            .ok_or_else(|| {
                invalid_data(format!(
                    "cargo metadata workspace member {package_id} has no package"
                ))
            })?;
        let name = package
            .get("name")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                invalid_data(format!(
                    "cargo metadata workspace package {package_id} requires a name"
                ))
            })?;
        let version = package
            .get("version")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                invalid_data(format!(
                    "cargo metadata workspace package {package_id} requires a version"
                ))
            })?;
        let manifest_path = package
            .get("manifest_path")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| {
                invalid_data(format!(
                    "cargo metadata workspace package {package_id} requires a manifest path"
                ))
            })?;
        let source_directory = Path::new(manifest_path)
            .parent()
            .ok_or_else(|| {
                invalid_data(format!(
                    "cargo metadata workspace package {package_id} manifest requires a parent"
                ))
            })?
            .canonicalize()?;
        inventory.insert(
            package_id.to_owned(),
            WorkspacePackageIdentity {
                name: name.to_owned(),
                version: version.to_owned(),
                source_directory,
            },
        );
    }

    Ok(inventory)
}

fn validate_sbom_component(
    component: &Value,
    workspace_packages: &BTreeMap<String, WorkspacePackageIdentity>,
    sbom_directory: &Path,
) -> Result<(), Box<dyn Error>> {
    let name = component
        .get("name")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid_data("every SBOM component requires a name"))?;
    let version = component
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

    let workspace_package =
        component
            .get("bom-ref")
            .and_then(Value::as_str)
            .and_then(|package_id| {
                workspace_packages
                    .get(package_id)
                    .map(|package| (package_id, package))
            });
    if let Some((package_id, package)) = workspace_package {
        if name != package.name || version != package.version {
            return Err(invalid_data(format!(
                "SBOM workspace component {package_id} does not match Cargo metadata"
            )));
        }
        let purl = component
            .get("purl")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid_data(format!("SBOM workspace component {name} requires a purl"))
            })?;
        let purl_prefix = format!("pkg:cargo/{name}@{version}?download_url=file://");
        let source_path = purl.strip_prefix(&purl_prefix).ok_or_else(|| {
            invalid_data(format!(
                "SBOM workspace component {name} purl does not match Cargo metadata"
            ))
        })?;
        if source_path.is_empty() || source_path.contains(['?', '#', '&']) {
            return Err(invalid_data(format!(
                "SBOM workspace component {name} has an invalid file purl"
            )));
        }
        let component_source =
            sbom_directory
                .join(source_path)
                .canonicalize()
                .map_err(|error| {
                    invalid_data(format!(
                        "SBOM workspace component {name} source cannot be inspected: {error}"
                    ))
                })?;
        if component_source != package.source_directory {
            return Err(invalid_data(format!(
                "SBOM workspace component {name} source does not match Cargo metadata"
            )));
        }
        return Ok(());
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

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use ntsql_contract::ProvenanceRecord;

    use super::*;

    #[test]
    fn implementation_authority_arguments_are_all_or_nothing() -> Result<(), Box<dyn Error>> {
        assert!(parse_optional_implementation_authorities(std::iter::empty()).is_ok());
        let complete = ["legal.json", "technical.json", "anaregdesign/ntsql", "abc"]
            .into_iter()
            .map(str::to_owned);
        assert!(parse_optional_implementation_authorities(complete).is_ok());
        let incomplete = ["legal.json", "technical.json", "anaregdesign/ntsql"]
            .into_iter()
            .map(str::to_owned);
        assert!(parse_optional_implementation_authorities(incomplete).is_err());
        Ok(())
    }

    #[test]
    fn repository_artifact_verification_accepts_valid_digest() -> Result<(), Box<dyn Error>> {
        let test_root = temporary_test_root("valid-repository-artifact")?;
        let artifact_path = test_root.join("contracts/artifact.json");
        fs::create_dir_all(
            artifact_path
                .parent()
                .ok_or_else(|| invalid_data("no parent"))?,
        )?;
        fs::write(&artifact_path, b"repository-authored artifact")?;
        let digest = sha256_digest(&artifact_path)?;

        verify_repository_artifact(&test_root, "contracts/artifact.json", &digest)?;

        fs::remove_dir_all(&test_root)?;
        Ok(())
    }

    #[test]
    fn repository_artifact_verification_rejects_unsafe_or_invalid_inputs()
    -> Result<(), Box<dyn Error>> {
        let test_root = temporary_test_root("invalid-repository-artifacts")?;
        let artifact_path = test_root.join("artifact.json");
        fs::write(&artifact_path, b"repository-authored artifact")?;
        let link_path = test_root.join("artifact-link.json");
        symlink(&artifact_path, &link_path)?;

        let missing_error = verification_error(
            &test_root,
            "missing.json",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        )?;
        let mismatch_error = verification_error(
            &test_root,
            "artifact.json",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        )?;
        let traversal_error = verification_error(
            &test_root,
            "../artifact.json",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        )?;
        let symlink_error = verification_error(
            &test_root,
            "artifact-link.json",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
        )?;

        fs::remove_dir_all(&test_root)?;
        assert!(missing_error.contains("cannot be inspected"));
        assert!(mismatch_error.contains("digest mismatch"));
        assert!(traversal_error.contains("relative normal components"));
        assert!(symlink_error.contains("must not contain symlinks"));
        Ok(())
    }

    #[test]
    fn direct_dependency_verification_accepts_exact_lock_checksum() -> Result<(), Box<dyn Error>> {
        let metadata = test_cargo_metadata()?;
        let provenance = test_provenance_ledger(vec![test_dependency_record(
            "prov-crates-serde-1.0.229",
            "https://static.crates.io/crates/serde/serde-1.0.229.crate",
            "sha256:4148590afebada386688f18773da617792bf2ef03ffc1e4cbd2b1d45b023e0ba",
            ProvenanceUse::DependencyInclusion,
        )]);

        let verified = verify_direct_dependency_records(&metadata, test_lockfile(), &provenance)?;

        assert_eq!(verified, 1);
        Ok(())
    }

    #[test]
    fn direct_dependency_verification_rejects_missing_mismatched_and_unknown_records()
    -> Result<(), Box<dyn Error>> {
        let metadata = test_cargo_metadata()?;
        let valid_record = test_dependency_record(
            "prov-crates-serde-1.0.229",
            "https://static.crates.io/crates/serde/serde-1.0.229.crate",
            "sha256:4148590afebada386688f18773da617792bf2ef03ffc1e4cbd2b1d45b023e0ba",
            ProvenanceUse::DependencyInclusion,
        );
        let missing_error = result_error(verify_direct_dependency_records(
            &metadata,
            test_lockfile(),
            &test_provenance_ledger(Vec::new()),
        ))?;

        let mut mismatched_record = valid_record.clone();
        mismatched_record.content_digest =
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned();
        let mismatch_error = result_error(verify_direct_dependency_records(
            &metadata,
            test_lockfile(),
            &test_provenance_ledger(vec![mismatched_record]),
        ))?;

        let missing_lock_error = result_error(verify_direct_dependency_records(
            &metadata,
            "version = 4\n\n[[package]]\nname = \"other\"\nversion = \"1.0.0\"\n",
            &test_provenance_ledger(vec![valid_record.clone()]),
        ))?;

        let unknown_record = test_dependency_record(
            "prov-crates-extra-1.0.0",
            "https://static.crates.io/crates/extra/extra-1.0.0.crate",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            ProvenanceUse::DependencyInclusion,
        );
        let unknown_error = result_error(verify_direct_dependency_records(
            &metadata,
            test_lockfile(),
            &test_provenance_ledger(vec![valid_record, unknown_record]),
        ))?;

        let mut unsupported_source = test_cargo_metadata()?;
        let dependency_source = unsupported_source
            .pointer_mut("/packages/0/dependencies/1/source")
            .ok_or_else(|| invalid_data("test cargo metadata dependency source is missing"))?;
        *dependency_source = Value::String("git+https://example.invalid/dependency".to_owned());
        let unsupported_source_error =
            result_error(direct_registry_dependencies(&unsupported_source))?;

        assert!(missing_error.contains("has no provenance record"));
        assert!(mismatch_error.contains("does not match the Cargo.lock checksum"));
        assert!(missing_lock_error.contains("has no exact crates.io package"));
        assert!(unknown_error.contains("unknown direct dependency provenance record"));
        assert!(unsupported_source_error.contains("unsupported external source"));
        Ok(())
    }

    #[test]
    fn sbom_workspace_components_require_exact_cargo_identity() -> Result<(), Box<dyn Error>> {
        let test_root = temporary_test_root("sbom-workspace-identity")?;
        let package_directory = test_root.join("crates/example");
        let sbom_directory = test_root.join("crates/consumer");
        fs::create_dir_all(&package_directory)?;
        fs::create_dir_all(&sbom_directory)?;
        let package_id = format!("path+file://{}#0.1.0", package_directory.to_string_lossy());
        let metadata = serde_json::json!({
            "workspace_members": [&package_id],
            "packages": [{
                "id": &package_id,
                "name": "example",
                "version": "0.1.0",
                "manifest_path": package_directory.join("Cargo.toml")
            }]
        });
        let inventory = workspace_package_inventory(&metadata)?;
        let component = serde_json::json!({
            "bom-ref": &package_id,
            "name": "example",
            "version": "0.1.0",
            "purl": "pkg:cargo/example@0.1.0?download_url=file://../example",
            "licenses": [{"expression": "Apache-2.0"}]
        });

        validate_sbom_component(&component, &inventory, &sbom_directory)?;

        let mut mismatched_component = component;
        mismatched_component["version"] = Value::String("0.2.0".to_owned());
        let mismatch_error = result_error(validate_sbom_component(
            &mismatched_component,
            &inventory,
            &sbom_directory,
        ))?;
        fs::remove_dir_all(&test_root)?;
        assert!(mismatch_error.contains("does not match Cargo metadata"));
        Ok(())
    }

    #[test]
    fn sbom_external_components_require_sha256() -> Result<(), Box<dyn Error>> {
        let inventory = BTreeMap::new();
        let mut component = serde_json::json!({
            "bom-ref": "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.219",
            "name": "serde",
            "version": "1.0.219",
            "licenses": [{"license": {"id": "MIT"}}]
        });

        let missing_hash_error = result_error(validate_sbom_component(
            &component,
            &inventory,
            Path::new("."),
        ))?;
        assert!(missing_hash_error.contains("requires hashes"));

        component["hashes"] = serde_json::json!([{
            "alg": "SHA-256",
            "content": "4148590afebada386688f18773da617792bf2ef03ffc1e4cbd2b1d45b023e0ba"
        }]);
        validate_sbom_component(&component, &inventory, Path::new("."))?;
        Ok(())
    }

    #[test]
    fn pinned_toolchain_rejects_mutable_or_drifting_configuration() -> Result<(), Box<dyn Error>> {
        let valid = parse_pinned_toolchain(test_toolchain_file())?;
        let mutable_channel = result_error(parse_pinned_toolchain(
            "[toolchain]\nchannel = \"stable\"\nprofile = \"minimal\"\ncomponents = [\"clippy\", \"rustfmt\"]\n",
        ))?;
        let wrong_profile = result_error(parse_pinned_toolchain(
            "[toolchain]\nchannel = \"1.97.1\"\nprofile = \"default\"\ncomponents = [\"clippy\", \"rustfmt\"]\n",
        ))?;
        let missing_component = result_error(parse_pinned_toolchain(
            "[toolchain]\nchannel = \"1.97.1\"\nprofile = \"minimal\"\ncomponents = [\"clippy\"]\n",
        ))?;
        let unsupported_key = result_error(parse_pinned_toolchain(
            "[toolchain]\nchannel = \"1.97.1\"\nprofile = \"minimal\"\ncomponents = [\"clippy\", \"rustfmt\"]\ntargets = [\"x86_64-unknown-linux-gnu\"]\n",
        ))?;
        let drifting_msrv = result_error(validate_workspace_rust_version(
            "[workspace.package]\nrust-version = \"1.96.0\"\n",
            &valid.channel,
        ))?;
        let test_root = temporary_test_root("legacy-rust-toolchain")?;
        fs::write(test_root.join("rust-toolchain.toml"), test_toolchain_file())?;
        fs::write(
            test_root.join("Cargo.toml"),
            "[workspace.package]\nrust-version = \"1.97.1\"\n",
        )?;
        fs::write(test_root.join("rust-toolchain"), "stable\n")?;
        let legacy_file = result_error(validate_pinned_toolchain(&test_root))?;
        fs::remove_dir_all(&test_root)?;

        assert_eq!(valid.channel, "1.97.1");
        assert!(mutable_channel.contains("exact stable x.y.z release"));
        assert!(wrong_profile.contains("profile must be minimal"));
        assert!(missing_component.contains("components must be exactly"));
        assert!(unsupported_key.contains("unsupported key"));
        assert!(drifting_msrv.contains("does not match pinned toolchain"));
        assert!(legacy_file.contains("legacy rust-toolchain is forbidden"));
        Ok(())
    }

    #[test]
    fn workflow_archive_inventory_accepts_exact_tools_and_actions() -> Result<(), Box<dyn Error>> {
        let provenance = test_supply_chain_provenance();
        let toolchain = parse_pinned_toolchain(test_toolchain_file())?;

        let archives =
            resolve_remote_archive_records(test_governance_workflow(), &toolchain, &provenance)?;

        assert_eq!(archives.len(), 3);
        assert_eq!(archives[0].kind, RemoteArchiveKind::GitHubAction);
        assert_eq!(archives[1].kind, RemoteArchiveKind::GovernanceTool);
        assert_eq!(archives[2].kind, RemoteArchiveKind::ToolchainManifest);
        Ok(())
    }

    #[test]
    fn workflow_archive_inventory_rejects_mutable_missing_and_unknown_entries()
    -> Result<(), Box<dyn Error>> {
        let toolchain = parse_pinned_toolchain(test_toolchain_file())?;
        let mutable_action_error = result_error(resolve_remote_archive_records(
            "steps:\n  - uses: actions/checkout@v4\n  - run: cargo install cargo-deny --version 0.20.2 --locked\n",
            &toolchain,
            &test_supply_chain_provenance(),
        ))?;
        let mutable_tool_error = result_error(resolve_remote_archive_records(
            "steps:\n  - uses: actions/checkout@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n  - run: cargo install cargo-deny --locked\n",
            &toolchain,
            &test_supply_chain_provenance(),
        ))?;

        let mut missing = test_supply_chain_provenance();
        missing.records.remove(0);
        let missing_error = result_error(resolve_remote_archive_records(
            test_governance_workflow(),
            &toolchain,
            &missing,
        ))?;

        let mut unknown = test_supply_chain_provenance();
        unknown.records.push(test_dependency_record(
            "prov-crates-unknown-1.0.0",
            "https://static.crates.io/crates/unknown/unknown-1.0.0.crate",
            "sha256:0000000000000000000000000000000000000000000000000000000000000000",
            ProvenanceUse::SupplyChainVerification,
        ));
        let unknown_error = result_error(resolve_remote_archive_records(
            test_governance_workflow(),
            &toolchain,
            &unknown,
        ))?;

        assert!(mutable_action_error.contains("40-character commit SHA"));
        assert!(mutable_tool_error.contains("--version <exact> --locked"));
        assert!(missing_error.contains("has no provenance record"));
        assert!(unknown_error.contains("unknown supply-chain provenance record"));
        Ok(())
    }

    #[test]
    fn remote_archive_downloads_fail_closed() -> Result<(), Box<dyn Error>> {
        let test_root = temporary_test_root("remote-archive-downloads")?;
        let reference_path = test_root.join("reference");
        fs::write(&reference_path, b"archive bytes")?;
        let digest = sha256_digest(&reference_path)?;
        let archives = vec![
            test_remote_archive(
                RemoteArchiveKind::GitHubAction,
                "prov-action",
                "https://codeload.github.com/actions/example/tar.gz/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                &digest,
            ),
            test_remote_archive(
                RemoteArchiveKind::GovernanceTool,
                "prov-tool",
                "https://static.crates.io/crates/example/example-1.0.0.crate",
                &digest,
            ),
        ];
        let successful_curl = write_fake_curl(
            &test_root,
            "curl-success",
            "printf '%s' 'archive bytes' > \"$output\"\nprintf '200\\n%s' \"$url\"",
        )?;
        verify_remote_archives(successful_curl.as_os_str(), &archives)?;

        let redirecting_curl = write_fake_curl(
            &test_root,
            "curl-redirect",
            "printf '%s' 'redirect' > \"$output\"\nprintf '302\\n%s' 'https://evil.example/archive'",
        )?;
        let redirect_error = result_error(verify_remote_archives(
            redirecting_curl.as_os_str(),
            &archives[..1],
        ))?;

        let failing_curl = write_fake_curl(
            &test_root,
            "curl-network-failure",
            "printf '%s' 'network failed' >&2\nexit 7",
        )?;
        let network_error = result_error(verify_remote_archives(
            failing_curl.as_os_str(),
            &archives[..1],
        ))?;

        let mismatching_curl = write_fake_curl(
            &test_root,
            "curl-mismatch",
            "printf '%s' 'different bytes' > \"$output\"\nprintf '200\\n%s' \"$url\"",
        )?;
        let mismatch_error = result_error(verify_remote_archives(
            mismatching_curl.as_os_str(),
            &archives[..1],
        ))?;

        let missing_program = test_root.join("curl-missing");
        let missing_tool_error = result_error(verify_remote_archives(
            missing_program.as_os_str(),
            &archives[..1],
        ))?;

        fs::remove_dir_all(&test_root)?;
        assert!(redirect_error.contains("redirects are not permitted"));
        assert!(network_error.contains("download failed"));
        assert!(mismatch_error.contains("digest mismatch"));
        assert!(missing_tool_error.contains("required download tool"));
        Ok(())
    }

    fn verification_error(
        workspace_root: &Path,
        artifact_path: &str,
        expected_digest: &str,
    ) -> Result<String, Box<dyn Error>> {
        verify_repository_artifact(workspace_root, artifact_path, expected_digest)
            .err()
            .map(|error| error.to_string())
            .ok_or_else(|| {
                invalid_data(format!("repository artifact {artifact_path} was accepted"))
            })
    }

    fn result_error<T>(result: Result<T, Box<dyn Error>>) -> Result<String, Box<dyn Error>> {
        result
            .err()
            .map(|error| error.to_string())
            .ok_or_else(|| invalid_data("invalid input was accepted"))
    }

    fn temporary_test_root(name: &str) -> Result<PathBuf, Box<dyn Error>> {
        create_temporary_directory(&format!("ntsql-{name}"))
    }

    fn test_cargo_metadata() -> Result<Value, Box<dyn Error>> {
        Ok(serde_json::from_str(
            r#"{
  "workspace_members": [
    "path+file:///workspace/crates/example#0.1.0"
  ],
  "packages": [
    {
      "id": "path+file:///workspace/crates/example#0.1.0",
      "dependencies": [
        {
          "name": "local",
          "source": null
        },
        {
          "name": "serde",
          "source": "registry+https://github.com/rust-lang/crates.io-index"
        }
      ]
    }
  ]
}"#,
        )?)
    }

    fn test_lockfile() -> &'static str {
        r#"version = 4

[[package]]
name = "serde"
version = "1.0.229"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "4148590afebada386688f18773da617792bf2ef03ffc1e4cbd2b1d45b023e0ba"
"#
    }

    fn test_governance_workflow() -> &'static str {
        "steps:\n  - uses: actions/checkout@aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n  - run: cargo install cargo-deny --version 0.20.2 --locked\n"
    }

    fn test_toolchain_file() -> &'static str {
        "[toolchain]\nchannel = \"1.97.1\"\nprofile = \"minimal\"\ncomponents = [\"clippy\", \"rustfmt\"]\n"
    }

    fn test_supply_chain_provenance() -> ProvenanceLedger {
        test_provenance_ledger(vec![
            test_dependency_record(
                "prov-action",
                "https://codeload.github.com/actions/checkout/tar.gz/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                ProvenanceUse::SupplyChainVerification,
            ),
            test_dependency_record(
                "prov-tool",
                "https://static.crates.io/crates/cargo-deny/cargo-deny-0.20.2.crate",
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                ProvenanceUse::SupplyChainVerification,
            ),
            test_dependency_record(
                "prov-toolchain",
                "https://static.rust-lang.org/dist/channel-rust-1.97.1.toml",
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                ProvenanceUse::SupplyChainVerification,
            ),
        ])
    }

    fn test_provenance_ledger(records: Vec<ProvenanceRecord>) -> ProvenanceLedger {
        ProvenanceLedger {
            schema_version: "1.0.0".to_owned(),
            records,
        }
    }

    fn test_dependency_record(
        id: &str,
        source_url: &str,
        content_digest: &str,
        intended_use: ProvenanceUse,
    ) -> ProvenanceRecord {
        ProvenanceRecord {
            id: id.to_owned(),
            source_kind: ProvenanceSourceKind::Dependency,
            title: id.to_owned(),
            source_url: Some(source_url.to_owned()),
            artifact_path: None,
            revision: "test revision".to_owned(),
            retrieved_on: "2026-08-05".to_owned(),
            author: "test".to_owned(),
            generation_method: "test".to_owned(),
            environment: None,
            license: "MIT".to_owned(),
            content_digest: content_digest.to_owned(),
            intended_uses: vec![intended_use],
            parent_provenance_ids: Vec::new(),
            legal_review_id: "legal-review-test".to_owned(),
        }
    }

    fn test_remote_archive(
        kind: RemoteArchiveKind,
        record_id: &str,
        source_url: &str,
        content_digest: &str,
    ) -> RemoteArchive {
        RemoteArchive {
            kind,
            record_id: record_id.to_owned(),
            source_url: source_url.to_owned(),
            content_digest: content_digest.to_owned(),
        }
    }

    fn write_fake_curl(
        test_root: &Path,
        name: &str,
        behavior: &str,
    ) -> Result<PathBuf, Box<dyn Error>> {
        let path = test_root.join(name);
        let script = format!(
            "#!/bin/sh\nset -eu\noutput=''\nurl=''\nwhile [ \"$#\" -gt 0 ]; do\n  case \"$1\" in\n    --output) output=\"$2\"; shift 2 ;;\n    --url) url=\"$2\"; shift 2 ;;\n    *) shift ;;\n  esac\ndone\n{behavior}\n"
        );
        fs::write(&path, script)?;
        let mut permissions = fs::metadata(&path)?.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&path, permissions)?;
        Ok(path)
    }

    #[test]
    fn authority_path_rejects_symlinks_across_checkout_boundary() -> Result<(), Box<dyn Error>> {
        let test_root = temporary_test_root("authority-path")?;
        let workspace_root = test_root.join("checkout");
        fs::create_dir_all(&workspace_root)?;
        let workspace_root = workspace_root.canonicalize()?;

        let external_authority = test_root.join("external-authority.json");
        fs::write(&external_authority, b"{}")?;
        let inside_link = workspace_root.join("inside-link.json");
        symlink(&external_authority, &inside_link)?;
        let inside_error = read_external_authority(&workspace_root, &inside_link)
            .err()
            .ok_or_else(|| invalid_data("inside authority symlink was accepted"))?
            .to_string();

        let inside_authority = workspace_root.join("inside-authority.json");
        fs::write(&inside_authority, b"{}")?;
        let outside_link = test_root.join("outside-link.json");
        symlink(&inside_authority, &outside_link)?;
        let outside_error = read_external_authority(&workspace_root, &outside_link)
            .err()
            .ok_or_else(|| invalid_data("outside authority symlink target was accepted"))?
            .to_string();

        fs::remove_dir_all(&test_root)?;
        assert!(inside_error.contains("path must originate outside"));
        assert!(outside_error.contains("target must be outside"));
        Ok(())
    }
}
