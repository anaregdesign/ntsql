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
        allowed_dependencies: &[],
    },
    PackagePolicy {
        package: "ntsql-compatibility",
        allowed_dependencies: &[],
    },
    PackagePolicy {
        package: "ntsql-diagnostics",
        allowed_dependencies: &[],
    },
    PackagePolicy {
        package: "ntsql-contract",
        allowed_dependencies: &["ntsql-compatibility", "serde", "serde_json"],
    },
    PackagePolicy {
        package: "ntsql-testkit",
        allowed_dependencies: &["ntsql-contract"],
    },
];

const CARGO_TREE_ARGS: &[&str] = &[
    "tree",
    "--workspace",
    "--all-features",
    "--depth",
    "1",
    "--edges",
    "normal,build,dev",
    "--target",
    "all",
    "--prefix",
    "depth",
    "--format",
    "{p}",
    "--locked",
    "--manifest-path",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PackagePolicy {
    package: &'static str,
    allowed_dependencies: &'static [&'static str],
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
    let output = Command::new(cargo)
        .args(CARGO_TREE_ARGS)
        .arg(&manifest)
        .output()
        .map_err(ArchitectureCheckError::CargoInvocation)?;

    if !output.status.success() {
        return Err(ArchitectureCheckError::CargoTreeFailed(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }

    let tree = String::from_utf8(output.stdout).map_err(ArchitectureCheckError::CargoOutput)?;
    let graph = parse_dependency_tree(&tree)?;
    let violations = validate_graph(&graph, PACKAGE_POLICIES);
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

fn validate_graph(graph: &DependencyGraph, policies: &[PackagePolicy]) -> Vec<String> {
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
        let expected = policy
            .allowed_dependencies
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();

        for dependency in actual {
            if !expected.contains(dependency.as_str()) {
                violations.push(format!(
                    "package {} has forbidden direct dependency {dependency}",
                    policy.package
                ));
            }
        }
        for dependency in expected {
            if !actual.contains(dependency) {
                violations.push(format!(
                    "package {} is missing required direct dependency {dependency}",
                    policy.package
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
    CargoInvocation(std::io::Error),
    CargoTreeFailed(String),
    CargoOutput(std::string::FromUtf8Error),
    InvalidTreeLine(String),
    PolicyViolations(Vec<String>),
}

impl fmt::Display for ArchitectureCheckError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkspaceRootMissing => formatter.write_str("workspace root is unavailable"),
            Self::CargoInvocation(error) => write!(formatter, "could not run cargo tree: {error}"),
            Self::CargoTreeFailed(error) => write!(formatter, "cargo tree failed: {error}"),
            Self::CargoOutput(error) => {
                write!(formatter, "cargo tree output is not UTF-8: {error}")
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
            Self::CargoInvocation(error) => Some(error),
            Self::CargoOutput(error) => Some(error),
            Self::WorkspaceRootMissing
            | Self::CargoTreeFailed(_)
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
        assert!(CARGO_TREE_ARGS.contains(&"--all-features"));
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
             0ntsql-testkit v0.1.0\n\
             1ntsql-contract v0.1.0\n",
        )?;

        assert!(validate_graph(&graph, PACKAGE_POLICIES).is_empty());
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
             1ntsql-contract v0.1.0\n",
        )?;

        let violations = validate_graph(&graph, PACKAGE_POLICIES);

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
             0ntsql-unreviewed v0.1.0\n",
        )?;

        let violations = validate_graph(&graph, PACKAGE_POLICIES);

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
             1serde_json v1.0.0\n",
        )?;

        let violations = validate_graph(&graph, PACKAGE_POLICIES);

        assert!(violations.iter().any(|violation| violation
            == "package ntsql-testkit has forbidden direct dependency ntsql-server"));
        assert!(violations.iter().any(|violation| violation
            == "package ntsql-testkit has forbidden direct dependency serde_json"));
        Ok(())
    }
}
