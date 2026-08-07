//! Enforces the reviewed direct-dependency graph for every workspace package.

use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    error::Error,
    fmt,
    path::{Path, PathBuf},
    process::Command,
};

const PACKAGE_POLICIES: &[PackagePolicy] = &[
    PackagePolicy {
        package: "ntsql-architecture-check",
        normal_dependencies: &[],
        build_dependencies: &[],
        development_dependencies: &[],
    },
    PackagePolicy {
        package: "ntsql-compatibility",
        normal_dependencies: &[],
        build_dependencies: &[],
        development_dependencies: &[],
    },
    PackagePolicy {
        package: "ntsql-diagnostics",
        normal_dependencies: &[],
        build_dependencies: &[],
        development_dependencies: &[],
    },
    PackagePolicy {
        package: "ntsql-contract",
        normal_dependencies: &["ntsql-compatibility", "serde", "serde_json"],
        build_dependencies: &[],
        development_dependencies: &[],
    },
    PackagePolicy {
        package: "ntsql-database",
        normal_dependencies: &["ntsql-wal"],
        build_dependencies: &[],
        development_dependencies: &[],
    },
    PackagePolicy {
        package: "ntsql-page",
        normal_dependencies: &["ntsql-wal"],
        build_dependencies: &[],
        development_dependencies: &[],
    },
    PackagePolicy {
        package: "ntsql-recovery-model",
        normal_dependencies: &[],
        build_dependencies: &[],
        development_dependencies: &[],
    },
    PackagePolicy {
        package: "ntsql-storage-file",
        normal_dependencies: &[
            "ntsql-database",
            "ntsql-page",
            "ntsql-transaction",
            "ntsql-wal",
        ],
        build_dependencies: &[],
        development_dependencies: &["ntsql-recovery-model"],
    },
    PackagePolicy {
        package: "ntsql-storage-memory",
        normal_dependencies: &[
            "ntsql-database",
            "ntsql-page",
            "ntsql-transaction",
            "ntsql-wal",
        ],
        build_dependencies: &[],
        development_dependencies: &["ntsql-recovery-model"],
    },
    PackagePolicy {
        package: "ntsql-testkit",
        normal_dependencies: &["ntsql-contract"],
        build_dependencies: &[],
        development_dependencies: &[],
    },
    PackagePolicy {
        package: "ntsql-transaction",
        normal_dependencies: &["ntsql-page", "ntsql-wal"],
        build_dependencies: &[],
        development_dependencies: &[],
    },
    PackagePolicy {
        package: "ntsql-wal",
        normal_dependencies: &[],
        build_dependencies: &[],
        development_dependencies: &[],
    },
];

const CARGO_TREE_BASE_ARGS: &[&str] = &[
    "tree",
    "--workspace",
    "--all-features",
    "--no-dedupe",
    "--depth",
    "1",
    "--target",
    "all",
    "--prefix",
    "depth",
    "--format",
    "{p}",
    "--locked",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PackagePolicy {
    package: &'static str,
    normal_dependencies: &'static [&'static str],
    build_dependencies: &'static [&'static str],
    development_dependencies: &'static [&'static str],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DependencyKind {
    Normal,
    Build,
    Development,
}

impl DependencyKind {
    const ALL: [Self; 3] = [Self::Normal, Self::Build, Self::Development];

    const fn cargo_argument(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Build => "build",
            Self::Development => "dev",
        }
    }

    const fn policy_dependencies(self, policy: &PackagePolicy) -> &'static [&'static str] {
        match self {
            Self::Normal => policy.normal_dependencies,
            Self::Build => policy.build_dependencies,
            Self::Development => policy.development_dependencies,
        }
    }

    const fn diagnostic_qualifier(self) -> &'static str {
        match self {
            Self::Normal => "",
            Self::Build => " build",
            Self::Development => " development",
        }
    }
}

fn main() {
    if let Err(error) = check_workspace() {
        eprintln!("architecture check failed: {error}");
        std::process::exit(1);
    }
    println!("architecture dependency graph is valid");
}

fn check_workspace() -> Result<(), ArchitectureCheckError> {
    let manifest = workspace_manifest()?;
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut violations = Vec::new();
    for kind in DependencyKind::ALL {
        let output = Command::new(&cargo)
            .args(CARGO_TREE_BASE_ARGS)
            .args(["--edges", kind.cargo_argument(), "--manifest-path"])
            .arg(&manifest)
            .output()
            .map_err(|source| ArchitectureCheckError::CargoInvocation { kind, source })?;

        if !output.status.success() {
            return Err(ArchitectureCheckError::CargoTreeFailed {
                kind,
                message: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }

        let tree = String::from_utf8(output.stdout)
            .map_err(|source| ArchitectureCheckError::CargoOutput { kind, source })?;
        let graph = parse_dependency_tree(&tree)?;
        violations.extend(validate_graph(&graph, PACKAGE_POLICIES, kind));
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(ArchitectureCheckError::PolicyViolations(violations))
    }
}

fn workspace_manifest() -> Result<PathBuf, ArchitectureCheckError> {
    let manifest_directory = Path::new(env!("CARGO_MANIFEST_DIR"));
    let Some(workspace_root) = manifest_directory.parent().and_then(Path::parent) else {
        return Err(ArchitectureCheckError::WorkspaceRootMissing);
    };
    Ok(workspace_root.join("Cargo.toml"))
}

fn parse_dependency_tree(input: &str) -> Result<DependencyGraph, ArchitectureCheckError> {
    let mut graph = BTreeMap::new();
    let mut current_package: Option<String> = None;

    for line in input.lines().filter(|line| !line.is_empty()) {
        let (depth, package_text) = if let Some(package) = line.strip_prefix('0') {
            (0_u8, package)
        } else if let Some(package) = line.strip_prefix('1') {
            (1_u8, package)
        } else {
            return Err(ArchitectureCheckError::InvalidTreeLine(line.to_owned()));
        };
        let Some(package) = package_text.split_whitespace().next() else {
            return Err(ArchitectureCheckError::InvalidTreeLine(line.to_owned()));
        };

        if depth == 0 {
            current_package = Some(package.to_owned());
            graph
                .entry(package.to_owned())
                .or_insert_with(BTreeSet::new);
        } else {
            let Some(root) = current_package.as_ref() else {
                return Err(ArchitectureCheckError::InvalidTreeLine(line.to_owned()));
            };
            graph
                .entry(root.clone())
                .or_insert_with(BTreeSet::new)
                .insert(package.to_owned());
        }
    }

    Ok(graph)
}

fn validate_graph(
    graph: &DependencyGraph,
    policies: &[PackagePolicy],
    kind: DependencyKind,
) -> Vec<String> {
    let policy_by_package = policies
        .iter()
        .map(|policy| (policy.package, policy))
        .collect::<BTreeMap<_, _>>();
    let mut violations = Vec::new();

    for package in graph.keys() {
        if !policy_by_package.contains_key(package.as_str()) {
            violations.push(format!(
                "workspace package {package} has no reviewed dependency policy"
            ));
        }
    }

    for policy in policies {
        let Some(actual) = graph.get(policy.package) else {
            violations.push(format!(
                "reviewed package {} is missing from the workspace graph",
                policy.package
            ));
            continue;
        };
        let expected = kind
            .policy_dependencies(policy)
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();

        for dependency in actual {
            if !expected.contains(dependency.as_str()) {
                violations.push(format!(
                    "package {} has forbidden direct{} dependency {dependency}",
                    policy.package,
                    kind.diagnostic_qualifier()
                ));
            }
        }
        for dependency in expected {
            if !actual.contains(dependency) {
                violations.push(format!(
                    "package {} is missing required direct{} dependency {dependency}",
                    policy.package,
                    kind.diagnostic_qualifier()
                ));
            }
        }
    }

    violations
}

type DependencyGraph = BTreeMap<String, BTreeSet<String>>;

#[derive(Debug)]
enum ArchitectureCheckError {
    WorkspaceRootMissing,
    CargoInvocation {
        kind: DependencyKind,
        source: std::io::Error,
    },
    CargoTreeFailed {
        kind: DependencyKind,
        message: String,
    },
    CargoOutput {
        kind: DependencyKind,
        source: std::string::FromUtf8Error,
    },
    InvalidTreeLine(String),
    PolicyViolations(Vec<String>),
}

impl fmt::Display for ArchitectureCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceRootMissing => formatter.write_str("workspace root is unavailable"),
            Self::CargoInvocation { kind, source } => write!(
                formatter,
                "could not run cargo tree for {} dependencies: {source}",
                kind.cargo_argument()
            ),
            Self::CargoTreeFailed { kind, message } => write!(
                formatter,
                "cargo tree failed for {} dependencies: {message}",
                kind.cargo_argument()
            ),
            Self::CargoOutput { kind, source } => {
                write!(
                    formatter,
                    "cargo tree output for {} dependencies is not UTF-8: {source}",
                    kind.cargo_argument()
                )
            }
            Self::InvalidTreeLine(line) => write!(formatter, "invalid cargo tree line: {line}"),
            Self::PolicyViolations(violations) => {
                write!(formatter, "{} policy violation(s)", violations.len())?;
                for violation in violations {
                    write!(formatter, "\n- {violation}")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for ArchitectureCheckError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::CargoInvocation { source, .. } => Some(source),
            Self::CargoOutput { source, .. } => Some(source),
            Self::WorkspaceRootMissing
            | Self::CargoTreeFailed { .. }
            | Self::InvalidTreeLine(_)
            | Self::PolicyViolations(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_tree_enables_every_feature() {
        assert!(CARGO_TREE_BASE_ARGS.contains(&"--all-features"));
        assert!(CARGO_TREE_BASE_ARGS.contains(&"--no-dedupe"));
    }

    #[test]
    fn reviewed_graph_is_accepted() -> Result<(), ArchitectureCheckError> {
        let graph = parse_dependency_tree(
            "0ntsql-architecture-check v0.1.0\n\
             0ntsql-compatibility v0.1.0\n\
             0ntsql-diagnostics v0.1.0\n\
             0ntsql-contract v0.1.0\n\
             1ntsql-compatibility v0.1.0\n\
             1serde v1.0.0\n\
             1serde_json v1.0.0\n\
             0ntsql-database v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-page v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-recovery-model v0.1.0\n\
             0ntsql-storage-file v0.1.0\n\
             1ntsql-database v0.1.0\n\
             1ntsql-page v0.1.0\n\
             1ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-storage-memory v0.1.0\n\
             1ntsql-database v0.1.0\n\
             1ntsql-page v0.1.0\n\
             1ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-testkit v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             0ntsql-storage-memory v0.1.0\n\
             1ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-transaction v0.1.0\n\
             1ntsql-page v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-wal v0.1.0\n",
        )?;

        assert!(validate_graph(&graph, PACKAGE_POLICIES, DependencyKind::Normal).is_empty());
        Ok(())
    }

    #[test]
    fn reviewed_build_and_development_graphs_are_accepted() -> Result<(), ArchitectureCheckError> {
        let build_graph = parse_dependency_tree(
            "0ntsql-architecture-check v0.1.0\n\
             0ntsql-compatibility v0.1.0\n\
             0ntsql-contract v0.1.0\n\
             0ntsql-database v0.1.0\n\
             0ntsql-diagnostics v0.1.0\n\
             0ntsql-page v0.1.0\n\
             0ntsql-recovery-model v0.1.0\n\
             0ntsql-storage-file v0.1.0\n\
             0ntsql-storage-memory v0.1.0\n\
             0ntsql-testkit v0.1.0\n\
             0ntsql-transaction v0.1.0\n\
             0ntsql-wal v0.1.0\n",
        )?;
        assert!(validate_graph(&build_graph, PACKAGE_POLICIES, DependencyKind::Build).is_empty());

        let development_graph = parse_dependency_tree(
            "0ntsql-architecture-check v0.1.0\n\
             0ntsql-compatibility v0.1.0\n\
             0ntsql-contract v0.1.0\n\
             0ntsql-database v0.1.0\n\
             0ntsql-diagnostics v0.1.0\n\
             0ntsql-page v0.1.0\n\
             0ntsql-recovery-model v0.1.0\n\
             0ntsql-storage-file v0.1.0\n\
             1ntsql-recovery-model v0.1.0\n\
             0ntsql-storage-memory v0.1.0\n\
             1ntsql-recovery-model v0.1.0\n\
             0ntsql-testkit v0.1.0\n\
             0ntsql-transaction v0.1.0\n\
             0ntsql-wal v0.1.0\n",
        )?;
        assert!(
            validate_graph(
                &development_graph,
                PACKAGE_POLICIES,
                DependencyKind::Development,
            )
            .is_empty()
        );
        Ok(())
    }

    #[test]
    fn recovery_model_is_dev_only_for_storage_adapters() -> Result<(), ArchitectureCheckError> {
        let adapter_development_graph = parse_dependency_tree(
            "0ntsql-storage-file v0.1.0\n\
             1ntsql-recovery-model v0.1.0\n\
             0ntsql-storage-memory v0.1.0\n\
             1ntsql-recovery-model v0.1.0\n",
        )?;
        let development_violations = validate_graph(
            &adapter_development_graph,
            PACKAGE_POLICIES,
            DependencyKind::Development,
        );
        assert!(!development_violations.iter().any(|violation| violation
            == "package ntsql-storage-file has forbidden direct development dependency ntsql-recovery-model"));
        assert!(!development_violations.iter().any(|violation| violation
            == "package ntsql-storage-memory has forbidden direct development dependency ntsql-recovery-model"));

        let adapter_production_graph = parse_dependency_tree(
            "0ntsql-storage-file v0.1.0\n\
             1ntsql-recovery-model v0.1.0\n\
             0ntsql-storage-memory v0.1.0\n\
             1ntsql-recovery-model v0.1.0\n",
        )?;
        for kind in [DependencyKind::Normal, DependencyKind::Build] {
            let violations = validate_graph(&adapter_production_graph, PACKAGE_POLICIES, kind);
            assert!(violations.iter().any(|violation| violation
                == &format!(
                    "package ntsql-storage-file has forbidden direct{} dependency ntsql-recovery-model",
                    kind.diagnostic_qualifier()
                )));
            assert!(violations.iter().any(|violation| violation
                == &format!(
                    "package ntsql-storage-memory has forbidden direct{} dependency ntsql-recovery-model",
                    kind.diagnostic_qualifier()
                )));
        }
        Ok(())
    }

    #[test]
    fn recovery_model_rejects_adapter_dependencies_in_every_kind()
    -> Result<(), ArchitectureCheckError> {
        let graph = parse_dependency_tree(
            "0ntsql-recovery-model v0.1.0\n\
             1ntsql-storage-file v0.1.0\n\
             1ntsql-storage-memory v0.1.0\n\
             1ntsql-transaction v0.1.0\n",
        )?;
        for kind in DependencyKind::ALL {
            let violations = validate_graph(&graph, PACKAGE_POLICIES, kind);
            for dependency in [
                "ntsql-storage-file",
                "ntsql-storage-memory",
                "ntsql-transaction",
            ] {
                assert!(violations.iter().any(|violation| violation
                    == &format!(
                        "package ntsql-recovery-model has forbidden direct{} dependency {dependency}",
                        kind.diagnostic_qualifier()
                    )));
            }
        }
        Ok(())
    }

    #[test]
    fn forbidden_domain_dependencies_are_rejected() -> Result<(), ArchitectureCheckError> {
        let graph = parse_dependency_tree(
            "0ntsql-architecture-check v0.1.0\n\
             0ntsql-compatibility v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             1ntsql-filesystem-adapter v0.1.0\n\
             1ntsql-network-adapter v0.1.0\n\
             1serde v1.0.0\n\
             0ntsql-diagnostics v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             1ntsql-filesystem-adapter v0.1.0\n\
             1ntsql-network-adapter v0.1.0\n\
             1ntsql-protocol-host v0.1.0\n\
             1serde v1.0.0\n\
             0ntsql-contract v0.1.0\n\
             1ntsql-compatibility v0.1.0\n\
             1serde v1.0.0\n\
             1serde_json v1.0.0\n\
             0ntsql-testkit v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             0ntsql-storage-memory v0.1.0\n\
             1ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-wal v0.1.0\n",
        )?;

        let violations = validate_graph(&graph, PACKAGE_POLICIES, DependencyKind::Normal);

        assert!(violations.iter().any(|violation| violation
            == "package ntsql-compatibility has forbidden direct dependency ntsql-contract"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-compatibility has forbidden direct dependency ntsql-filesystem-adapter"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-compatibility has forbidden direct dependency ntsql-network-adapter"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-compatibility has forbidden direct dependency serde"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-diagnostics has forbidden direct dependency ntsql-contract"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-diagnostics has forbidden direct dependency ntsql-filesystem-adapter"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-diagnostics has forbidden direct dependency ntsql-network-adapter"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-diagnostics has forbidden direct dependency ntsql-protocol-host"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-diagnostics has forbidden direct dependency serde"));
        Ok(())
    }

    #[test]
    fn database_domain_dependencies_are_enforced() -> Result<(), ArchitectureCheckError> {
        let graph = parse_dependency_tree(
            "0ntsql-database v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             1ntsql-compatibility v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             1ntsql-diagnostics v0.1.0\n\
             1ntsql-storage-file v0.1.0\n\
             1ntsql-storage-memory v0.1.0\n\
             1ntsql-transaction v0.1.0\n\
             1serde v1.0.0\n",
        )?;

        let forbidden = [
            "ntsql-compatibility",
            "ntsql-contract",
            "ntsql-diagnostics",
            "ntsql-storage-file",
            "ntsql-storage-memory",
            "ntsql-transaction",
            "serde",
        ];
        let normal_violations = validate_graph(&graph, PACKAGE_POLICIES, DependencyKind::Normal);
        assert!(!normal_violations.iter().any(|violation| violation
            == "package ntsql-database has forbidden direct dependency ntsql-wal"));
        for dependency in forbidden {
            assert!(normal_violations.iter().any(|violation| violation
                == &format!(
                    "package ntsql-database has forbidden direct dependency {dependency}"
                )));
        }

        for kind in [DependencyKind::Build, DependencyKind::Development] {
            let violations = validate_graph(&graph, PACKAGE_POLICIES, kind);
            for dependency in forbidden.into_iter().chain(["ntsql-wal"]) {
                assert!(violations.iter().any(|violation| violation
                    == &format!(
                        "package ntsql-database has forbidden direct{} dependency {dependency}",
                        kind.diagnostic_qualifier()
                    )));
            }
        }
        Ok(())
    }

    #[test]
    fn unregistered_workspace_package_is_rejected() -> Result<(), ArchitectureCheckError> {
        let graph = parse_dependency_tree(
            "0ntsql-architecture-check v0.1.0\n\
             0ntsql-compatibility v0.1.0\n\
             0ntsql-diagnostics v0.1.0\n\
             0ntsql-contract v0.1.0\n\
             1ntsql-compatibility v0.1.0\n\
             1serde v1.0.0\n\
             1serde_json v1.0.0\n\
             0ntsql-testkit v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             0ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-wal v0.1.0\n\
             0ntsql-unreviewed v0.1.0\n",
        )?;

        let violations = validate_graph(&graph, PACKAGE_POLICIES, DependencyKind::Normal);

        assert!(violations.iter().any(|violation| violation
            == "workspace package ntsql-unreviewed has no reviewed dependency policy"));
        Ok(())
    }

    #[test]
    fn testkit_adapter_dependencies_are_enforced() -> Result<(), ArchitectureCheckError> {
        let graph = parse_dependency_tree(
            "0ntsql-architecture-check v0.1.0\n\
             0ntsql-compatibility v0.1.0\n\
             0ntsql-diagnostics v0.1.0\n\
             0ntsql-contract v0.1.0\n\
             1ntsql-compatibility v0.1.0\n\
             1serde v1.0.0\n\
             1serde_json v1.0.0\n\
             0ntsql-testkit v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             1ntsql-server v0.1.0\n\
             1serde_json v1.0.0\n\
             0ntsql-storage-memory v0.1.0\n\
             1ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-wal v0.1.0\n",
        )?;

        let violations = validate_graph(&graph, PACKAGE_POLICIES, DependencyKind::Normal);

        assert!(violations.iter().any(|violation| violation
            == "package ntsql-testkit has forbidden direct dependency ntsql-server"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-testkit has forbidden direct dependency serde_json"));
        Ok(())
    }

    #[test]
    fn wal_domain_dependencies_are_enforced() -> Result<(), ArchitectureCheckError> {
        let graph = parse_dependency_tree(
            "0ntsql-architecture-check v0.1.0\n\
             0ntsql-compatibility v0.1.0\n\
             0ntsql-diagnostics v0.1.0\n\
             0ntsql-contract v0.1.0\n\
             1ntsql-compatibility v0.1.0\n\
             1serde v1.0.0\n\
             1serde_json v1.0.0\n\
             0ntsql-testkit v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             0ntsql-storage-memory v0.1.0\n\
             1ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-wal v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             1ntsql-filesystem-adapter v0.1.0\n\
             1ntsql-page v0.1.0\n\
             1ntsql-protocol-host v0.1.0\n\
             1ntsql-transaction v0.1.0\n\
             1serde v1.0.0\n\
             0ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n",
        )?;

        let violations = validate_graph(&graph, PACKAGE_POLICIES, DependencyKind::Normal);

        for dependency in [
            "ntsql-contract",
            "ntsql-filesystem-adapter",
            "ntsql-page",
            "ntsql-protocol-host",
            "ntsql-transaction",
            "serde",
        ] {
            assert!(violations.iter().any(|violation| violation
                == &format!("package ntsql-wal has forbidden direct dependency {dependency}")));
        }
        Ok(())
    }

    #[test]
    fn page_domain_dependencies_are_enforced() -> Result<(), ArchitectureCheckError> {
        let graph = parse_dependency_tree(
            "0ntsql-page v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             1ntsql-storage-file v0.1.0\n\
            1ntsql-storage-memory v0.1.0\n\
            1ntsql-transaction v0.1.0\n\
            1ntsql-wal v0.1.0\n\
             1serde v1.0.0\n\
             0ntsql-wal v0.1.0\n",
        )?;

        let violations = validate_graph(&graph, PACKAGE_POLICIES, DependencyKind::Normal);

        for dependency in [
            "ntsql-contract",
            "ntsql-storage-file",
            "ntsql-storage-memory",
            "ntsql-transaction",
            "serde",
        ] {
            assert!(violations.iter().any(|violation| violation
                == &format!("package ntsql-page has forbidden direct dependency {dependency}")));
        }
        Ok(())
    }

    #[test]
    fn transaction_domain_dependencies_are_enforced() -> Result<(), ArchitectureCheckError> {
        let graph = parse_dependency_tree(
            "0ntsql-architecture-check v0.1.0\n\
             0ntsql-compatibility v0.1.0\n\
             0ntsql-diagnostics v0.1.0\n\
             0ntsql-contract v0.1.0\n\
             1ntsql-compatibility v0.1.0\n\
             1serde v1.0.0\n\
             1serde_json v1.0.0\n\
             0ntsql-testkit v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             0ntsql-storage-memory v0.1.0\n\
             1ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             1ntsql-filesystem-adapter v0.1.0\n\
             1ntsql-protocol-host v0.1.0\n\
             1serde v1.0.0\n\
             0ntsql-wal v0.1.0\n",
        )?;

        let violations = validate_graph(&graph, PACKAGE_POLICIES, DependencyKind::Normal);

        for dependency in [
            "ntsql-contract",
            "ntsql-filesystem-adapter",
            "ntsql-protocol-host",
            "serde",
        ] {
            assert!(violations.iter().any(|violation| violation
                == &format!(
                    "package ntsql-transaction has forbidden direct dependency {dependency}"
                )));
        }
        Ok(())
    }

    #[test]
    fn memory_storage_adapter_dependencies_are_enforced() -> Result<(), ArchitectureCheckError> {
        let graph = parse_dependency_tree(
            "0ntsql-architecture-check v0.1.0\n\
             0ntsql-compatibility v0.1.0\n\
             0ntsql-contract v0.1.0\n\
             1ntsql-compatibility v0.1.0\n\
             1serde v1.0.0\n\
             1serde_json v1.0.0\n\
             0ntsql-diagnostics v0.1.0\n\
             0ntsql-storage-memory v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             1ntsql-database v0.1.0\n\
             1ntsql-page v0.1.0\n\
             1ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             1serde v1.0.0\n\
             0ntsql-testkit v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             0ntsql-transaction v0.1.0\n\
             1ntsql-storage-memory v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-page v0.1.0\n\
             1ntsql-storage-memory v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-wal v0.1.0\n\
             1ntsql-storage-memory v0.1.0\n",
        )?;

        let violations = validate_graph(&graph, PACKAGE_POLICIES, DependencyKind::Normal);

        for dependency in ["ntsql-contract", "serde"] {
            assert!(violations.iter().any(|violation| violation
                == &format!(
                    "package ntsql-storage-memory has forbidden direct dependency {dependency}"
                )));
        }
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-transaction has forbidden direct dependency ntsql-storage-memory"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-page has forbidden direct dependency ntsql-storage-memory"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-wal has forbidden direct dependency ntsql-storage-memory"));
        assert!(!violations.iter().any(|violation| violation
            == "package ntsql-storage-memory has forbidden direct dependency ntsql-database"));
        Ok(())
    }

    #[test]
    fn file_storage_adapter_dependencies_are_enforced() -> Result<(), ArchitectureCheckError> {
        let graph = parse_dependency_tree(
            "0ntsql-architecture-check v0.1.0\n\
             0ntsql-compatibility v0.1.0\n\
             0ntsql-contract v0.1.0\n\
             1ntsql-compatibility v0.1.0\n\
             1serde v1.0.0\n\
             1serde_json v1.0.0\n\
             0ntsql-diagnostics v0.1.0\n\
             0ntsql-storage-file v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             1ntsql-database v0.1.0\n\
             1ntsql-page v0.1.0\n\
             1ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             1serde v1.0.0\n\
             0ntsql-storage-memory v0.1.0\n\
             1ntsql-transaction v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-testkit v0.1.0\n\
             1ntsql-contract v0.1.0\n\
             0ntsql-transaction v0.1.0\n\
             1ntsql-storage-file v0.1.0\n\
             1ntsql-wal v0.1.0\n\
             0ntsql-wal v0.1.0\n\
             1ntsql-storage-file v0.1.0\n",
        )?;

        let violations = validate_graph(&graph, PACKAGE_POLICIES, DependencyKind::Normal);

        for dependency in ["ntsql-contract", "serde"] {
            assert!(violations.iter().any(|violation| violation
                == &format!(
                    "package ntsql-storage-file has forbidden direct dependency {dependency}"
                )));
        }
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-transaction has forbidden direct dependency ntsql-storage-file"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-wal has forbidden direct dependency ntsql-storage-file"));
        assert!(!violations.iter().any(|violation| violation
            == "package ntsql-storage-file has forbidden direct dependency ntsql-database"));
        Ok(())
    }
}
