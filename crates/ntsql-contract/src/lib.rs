//! Types and invariants for ntsql compatibility evidence.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current version of the conformance record contract.
pub const CONFORMANCE_SCHEMA_VERSION: &str = "1.0.0";

/// Compatibility dimensions that must be evaluated for every case.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompatibilityDimension {
    /// Whether the input is accepted and parsed equivalently.
    Syntax,
    /// Whether client-visible protocol behavior is equivalent.
    Wire,
    /// Whether returned values and row ordering are equivalent.
    Result,
    /// Whether column and result-set metadata are equivalent.
    Metadata,
    /// Whether errors, warnings, and session diagnostics are equivalent.
    Diagnostic,
    /// Whether transactions and persistent side effects are equivalent.
    TransactionalSideEffect,
    /// Whether startup, configuration, and administration behavior is equivalent.
    Operational,
}

/// The comparison outcome for a feature or conformance case.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompatibilityStatus {
    /// All required observations match every target in scope.
    Compatible,
    /// A documented subset is compatible.
    Partial,
    /// At least one required observation intentionally differs.
    Divergent,
    /// Work is isolated pending an explicit legal approval.
    BlockedLegal,
    /// No sufficient conformance evidence exists yet.
    NotTested,
}

/// The outcome of comparing one observed dimension.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ComparisonStatus {
    /// The normalized oracle and subject observations match.
    Compatible,
    /// The observations match only for a documented subset.
    Partial,
    /// At least one required value differs.
    Divergent,
}

/// Classification of the authority behind an expected behavior.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum BehaviorClass {
    /// Behavior stated in a public, authoritative specification.
    Documented,
    /// Behavior stated to vary by SQL Server or compatibility version.
    VersionDependent,
    /// Public documentation leaves the behavior unspecified.
    Unspecified,
    /// Behavior is an observation of a particular implementation.
    ImplementationDependent,
}

/// A mandatory dimension observation, including an explicit reason when absent.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DimensionObservation {
    /// The oracle and subject observations were captured.
    Observed {
        /// Normalized, dimension-specific observation payload.
        oracle: Value,
        /// Normalized observation produced by ntsql.
        subject: Value,
        /// Outcome of comparing the normalized payloads.
        status: ComparisonStatus,
    },
    /// The dimension could not be observed in this run.
    NotObserved {
        /// Actionable explanation for the missing observation.
        reason: String,
    },
}

/// Complete set of externally observable dimensions for one input.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConformanceObservations {
    /// Syntax acceptance and parser diagnostics.
    pub syntax: DimensionObservation,
    /// TDS and connection-level behavior.
    pub wire: DimensionObservation,
    /// Values, rows, result sets, and ordering.
    pub result: DimensionObservation,
    /// Type, nullability, collation, and column metadata.
    pub metadata: DimensionObservation,
    /// Errors, warnings, `@@ERROR`, severity, state, and connection state.
    pub diagnostic: DimensionObservation,
    /// `XACT_STATE()`, commit state, and persistent side effects.
    pub transactional_side_effect: DimensionObservation,
    /// Configuration, lifecycle, backup, restore, and administration behavior.
    pub operational: DimensionObservation,
}

/// Machine-readable conformance evidence for one input and target environment.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConformanceRecord {
    /// Contract version used to interpret this record.
    pub schema_version: String,
    /// Stable identifier of the test input.
    pub case_id: String,
    /// Identifier of the exact oracle target from the target matrix.
    pub target_id: String,
    /// ISO 8601 UTC capture timestamp.
    pub observed_at: String,
    /// Provenance record that authorizes the input and oracle observation.
    pub provenance_id: String,
    /// Behavior authority classification.
    pub behavior_class: BehaviorClass,
    /// All required observable dimensions.
    pub observations: ConformanceObservations,
}

/// Top-level Database Engine feature categories.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FeatureCategory {
    /// Client connectivity and TDS protocol behavior.
    ProtocolConnectivity,
    /// T-SQL lexical, grammar, and batch behavior.
    Language,
    /// SQL Server data types and conversions.
    DataTypes,
    /// Collations, Unicode, locale, and comparison behavior.
    Collation,
    /// Scalar expressions and built-in functions.
    ScalarExpressions,
    /// Query processing, relational operators, and optimizer-visible behavior.
    QueryProcessing,
    /// Data manipulation statements and bulk data movement.
    DataManipulation,
    /// Data definition and schema objects.
    DataDefinition,
    /// Stored procedures, functions, triggers, and dynamic SQL.
    Programmability,
    /// Transactions, locking, row versioning, and concurrency.
    TransactionsConcurrency,
    /// Authentication, authorization, encryption, and auditing.
    Security,
    /// Catalog views, information schema, and metadata APIs.
    CatalogMetadata,
    /// Configuration, maintenance, and lifecycle administration.
    Administration,
    /// Persistence, recovery, backup, restore, and integrity.
    StorageRecovery,
    /// Availability groups, failover, and resilience surfaces.
    HighAvailability,
    /// Replication, change tracking, and change data capture.
    DataDistribution,
    /// Extended Events, DMVs, tracing, and diagnostics.
    ObservabilityDiagnostics,
    /// Public integration surfaces owned by the Database Engine.
    ExternalIntegration,
}

/// Every category that a Database Engine feature can occupy.
pub const FEATURE_CATEGORIES: [FeatureCategory; 18] = [
    FeatureCategory::ProtocolConnectivity,
    FeatureCategory::Language,
    FeatureCategory::DataTypes,
    FeatureCategory::Collation,
    FeatureCategory::ScalarExpressions,
    FeatureCategory::QueryProcessing,
    FeatureCategory::DataManipulation,
    FeatureCategory::DataDefinition,
    FeatureCategory::Programmability,
    FeatureCategory::TransactionsConcurrency,
    FeatureCategory::Security,
    FeatureCategory::CatalogMetadata,
    FeatureCategory::Administration,
    FeatureCategory::StorageRecovery,
    FeatureCategory::HighAvailability,
    FeatureCategory::DataDistribution,
    FeatureCategory::ObservabilityDiagnostics,
    FeatureCategory::ExternalIntegration,
];

/// One reproducible SQL Server oracle configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OracleTarget {
    /// Stable target identifier referenced by evidence.
    pub id: String,
    /// Provenance record for the image, build, and configuration facts.
    pub provenance_id: String,
    /// SQL Server product release, for example `2022`.
    pub product_release: String,
    /// Exact servicing update, for example `CU26`.
    pub servicing_update: String,
    /// Exact product build returned by `SERVERPROPERTY('ProductVersion')`.
    pub product_version: String,
    /// SQL Server edition selected for the oracle.
    pub edition: String,
    /// Container or host operating system.
    pub operating_system: String,
    /// Required processor architecture.
    pub architecture: String,
    /// Container repository without tag or digest.
    pub container_repository: String,
    /// Immutable-in-policy human-readable container tag.
    pub container_tag: String,
    /// Registry manifest digest that actually makes the image immutable.
    pub container_digest: String,
    /// Database compatibility level under test.
    pub compatibility_level: u16,
    /// Server and database collation under test.
    pub collation: String,
    /// Session language under test.
    pub language: String,
    /// SQL Server language identifier.
    pub lcid: u32,
    /// Host and session timezone policy.
    pub timezone: String,
    /// Explicit session settings applied before every conformance case.
    pub session_settings: Vec<String>,
}

/// A future expansion checkpoint for the target matrix.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetExpansion {
    /// Stable, one-based execution order.
    pub sequence: u16,
    /// Product or configuration axis added by this checkpoint.
    pub scope: String,
    /// Evidence required before the checkpoint enters the active target set.
    pub admission_criteria: String,
}

/// Versioned set of exact oracle targets and its expansion order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetMatrix {
    /// Contract version used to interpret this matrix.
    pub schema_version: String,
    /// First vertical-slice target.
    pub baseline_target_id: String,
    /// Exact, currently active oracle configurations.
    pub targets: Vec<OracleTarget>,
    /// Ordered checkpoints for expanding the active target set.
    pub expansion_order: Vec<TargetExpansion>,
}

impl TargetMatrix {
    /// Validates target uniqueness, reproducibility, and baseline selection.
    #[must_use]
    pub fn validate(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let mut target_ids = BTreeSet::new();

        if self.schema_version != CONFORMANCE_SCHEMA_VERSION {
            violations.push(ContractViolation {
                code: "target.schema-version.unsupported",
                message: format!("unsupported target schema version: {}", self.schema_version),
            });
        }

        for target in &self.targets {
            if !target_ids.insert(target.id.as_str()) {
                violations.push(ContractViolation {
                    code: "target.id.duplicate",
                    message: format!("duplicate target id: {}", target.id),
                });
            }

            if !is_sha256_digest(&target.container_digest) {
                violations.push(ContractViolation {
                    code: "target.container-digest.invalid",
                    message: format!("target {} must use a sha256 container digest", target.id),
                });
            }

            if target.container_tag.contains("latest") {
                violations.push(ContractViolation {
                    code: "target.container-tag.mutable",
                    message: format!("target {} must not use a latest tag", target.id),
                });
            }

            if target.session_settings.is_empty() {
                violations.push(ContractViolation {
                    code: "target.session-settings.empty",
                    message: format!("target {} requires explicit session settings", target.id),
                });
            }
        }

        if !target_ids.contains(self.baseline_target_id.as_str()) {
            violations.push(ContractViolation {
                code: "target.baseline.unknown",
                message: format!(
                    "baseline target is not present: {}",
                    self.baseline_target_id
                ),
            });
        }

        for (index, expansion) in self.expansion_order.iter().enumerate() {
            if usize::from(expansion.sequence) != index + 1 {
                violations.push(ContractViolation {
                    code: "target.expansion.sequence",
                    message: "target expansion sequence must be contiguous and one-based"
                        .to_owned(),
                });
                break;
            }
        }

        violations
    }

    fn target_ids(&self) -> BTreeSet<&str> {
        self.targets
            .iter()
            .map(|target| target.id.as_str())
            .collect()
    }
}

fn is_sha256_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };

    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// One entry in the Database Engine feature matrix.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureRecord {
    /// Stable feature identifier.
    pub id: String,
    /// Human-readable feature name.
    pub title: String,
    /// Required category; there is deliberately no unclassified variant.
    pub category: FeatureCategory,
    /// Current compatibility outcome.
    pub status: CompatibilityStatus,
    /// Exact oracle target identifiers used by this feature.
    pub oracle_targets: Vec<String>,
    /// Conformance case or authoritative-source identifiers.
    pub evidence: Vec<String>,
    /// Known, externally observable differences.
    pub differences: Vec<String>,
    /// GitHub issue that owns the remaining work.
    pub owner_issue: u64,
    /// Legal review or gate identifier when the feature is legally blocked.
    pub legal_review_id: Option<String>,
}

/// Versioned Database Engine feature inventory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureMatrix {
    /// Contract version used to interpret this matrix.
    pub schema_version: String,
    /// Features and category roots in compatibility scope.
    pub features: Vec<FeatureRecord>,
}

/// A contract invariant violation with a stable code.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContractViolation {
    /// Stable code suitable for CI parsing.
    pub code: &'static str,
    /// Human-readable context.
    pub message: String,
}

impl FeatureRecord {
    /// Validates evidence and ownership rules implied by the feature status.
    #[must_use]
    pub fn validate(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if self.id.trim().is_empty() {
            violations.push(ContractViolation {
                code: "feature.id.empty",
                message: "feature id must not be empty".to_owned(),
            });
        }

        if self.owner_issue == 0 {
            violations.push(ContractViolation {
                code: "feature.owner.missing",
                message: "owner_issue must reference a GitHub issue".to_owned(),
            });
        }

        if self.evidence.is_empty() || self.oracle_targets.is_empty() {
            violations.push(ContractViolation {
                code: "feature.traceability.missing",
                message: "features require evidence and oracle targets".to_owned(),
            });
        }

        match self.status {
            CompatibilityStatus::Compatible => {
                if !self.differences.is_empty() {
                    violations.push(ContractViolation {
                        code: "feature.compatible.has-differences",
                        message: "compatible features cannot have known differences".to_owned(),
                    });
                }
            }
            CompatibilityStatus::Partial | CompatibilityStatus::Divergent => {
                if self.differences.is_empty() {
                    violations.push(ContractViolation {
                        code: "feature.difference.missing",
                        message: "partial or divergent features require a known difference"
                            .to_owned(),
                    });
                }
            }
            CompatibilityStatus::BlockedLegal => {
                if self.legal_review_id.is_none() {
                    violations.push(ContractViolation {
                        code: "feature.legal-review.missing",
                        message: "legally blocked features require a legal review record"
                            .to_owned(),
                    });
                }
            }
            CompatibilityStatus::NotTested => {}
        }

        violations
    }
}

impl FeatureMatrix {
    /// Validates every feature and all cross-record invariants.
    #[must_use]
    pub fn validate(&self, targets: &TargetMatrix) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let mut feature_ids = BTreeSet::new();
        let mut categories = BTreeSet::new();
        let target_ids = targets.target_ids();

        if self.schema_version != CONFORMANCE_SCHEMA_VERSION {
            violations.push(ContractViolation {
                code: "feature.schema-version.unsupported",
                message: format!(
                    "unsupported feature schema version: {}",
                    self.schema_version
                ),
            });
        }

        for feature in &self.features {
            violations.extend(feature.validate());
            categories.insert(feature.category);

            if !feature_ids.insert(feature.id.as_str()) {
                violations.push(ContractViolation {
                    code: "feature.id.duplicate",
                    message: format!("duplicate feature id: {}", feature.id),
                });
            }

            for target_id in &feature.oracle_targets {
                if !target_ids.contains(target_id.as_str()) {
                    violations.push(ContractViolation {
                        code: "feature.oracle-target.unknown",
                        message: format!(
                            "feature {} references unknown target {}",
                            feature.id, target_id
                        ),
                    });
                }
            }
        }

        for category in FEATURE_CATEGORIES {
            if !categories.contains(&category) {
                violations.push(ContractViolation {
                    code: "feature.category.missing",
                    message: format!("feature category has no inventory root: {category:?}"),
                });
            }
        }

        violations
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CompatibilityStatus, ConformanceRecord, FeatureCategory, FeatureMatrix, FeatureRecord,
        OracleTarget, TargetMatrix,
    };

    #[test]
    fn conformance_record_requires_every_dimension() {
        let missing_operational = r#"
        {
          "schema_version": "1.0.0",
          "case_id": "select.literal.integer",
          "target_id": "sqlserver-2022-cu18-linux",
          "observed_at": "2026-08-02T00:00:00Z",
          "provenance_id": "prov-select-literal-integer",
          "behavior_class": "documented",
          "observations": {
            "syntax": { "state": "not-observed", "reason": "pending" },
            "wire": { "state": "not-observed", "reason": "pending" },
            "result": { "state": "not-observed", "reason": "pending" },
            "metadata": { "state": "not-observed", "reason": "pending" },
            "diagnostic": { "state": "not-observed", "reason": "pending" },
            "transactional_side_effect": {
              "state": "not-observed",
              "reason": "pending"
            }
          }
        }
        "#;

        let result = serde_json::from_str::<ConformanceRecord>(missing_operational);

        assert!(result.is_err());
    }

    #[test]
    fn observed_dimension_rejects_feature_only_status() {
        let blocked_observation = r#"
                {
                    "schema_version": "1.0.0",
                    "case_id": "select.literal.integer",
                    "target_id": "sqlserver-2022-cu26-linux-x86_64-developer-compat160",
                    "observed_at": "2026-08-02T00:00:00Z",
                    "provenance_id": "prov-select-literal-integer",
                    "behavior_class": "documented",
                    "observations": {
                        "syntax": {
                            "state": "observed",
                            "oracle": "accepted",
                            "subject": "accepted",
                            "status": "blocked-legal"
                        },
                        "wire": { "state": "not-observed", "reason": "pending" },
                        "result": { "state": "not-observed", "reason": "pending" },
                        "metadata": { "state": "not-observed", "reason": "pending" },
                        "diagnostic": { "state": "not-observed", "reason": "pending" },
                        "transactional_side_effect": {
                            "state": "not-observed",
                            "reason": "pending"
                        },
                        "operational": { "state": "not-observed", "reason": "pending" }
                    }
                }
                "#;

        let result = serde_json::from_str::<ConformanceRecord>(blocked_observation);

        assert!(result.is_err());
    }

    #[test]
    fn compatible_feature_requires_evidence_and_oracle() {
        let feature = FeatureRecord {
            id: "language.select".to_owned(),
            title: "SELECT statement".to_owned(),
            category: FeatureCategory::Language,
            status: CompatibilityStatus::Compatible,
            oracle_targets: Vec::new(),
            evidence: Vec::new(),
            differences: Vec::new(),
            owner_issue: 8,
            legal_review_id: None,
        };

        let violations = feature.validate();

        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, "feature.traceability.missing");
    }

    #[test]
    fn blocked_feature_requires_legal_record() {
        let feature = FeatureRecord {
            id: "storage.mdf".to_owned(),
            title: "MDF file compatibility".to_owned(),
            category: FeatureCategory::StorageRecovery,
            status: CompatibilityStatus::BlockedLegal,
            oracle_targets: vec!["baseline".to_owned()],
            evidence: vec!["legal-review:native-file-formats".to_owned()],
            differences: Vec::new(),
            owner_issue: 3,
            legal_review_id: None,
        };

        let violations = feature.validate();

        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, "feature.legal-review.missing");
    }

    #[test]
    fn target_matrix_rejects_mutable_container_tag() {
        let matrix = TargetMatrix {
            schema_version: "1.0.0".to_owned(),
            baseline_target_id: "baseline".to_owned(),
            targets: vec![oracle_target("baseline", "2022-latest")],
            expansion_order: Vec::new(),
        };

        let violations = matrix.validate();

        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, "target.container-tag.mutable");
    }

    #[test]
    fn feature_matrix_requires_every_category() {
        let targets = TargetMatrix {
            schema_version: "1.0.0".to_owned(),
            baseline_target_id: "baseline".to_owned(),
            targets: vec![oracle_target("baseline", "2022-CU26-ubuntu-22.04")],
            expansion_order: Vec::new(),
        };
        let features = FeatureMatrix {
            schema_version: "1.0.0".to_owned(),
            features: vec![FeatureRecord {
                id: "language".to_owned(),
                title: "Language".to_owned(),
                category: FeatureCategory::Language,
                status: CompatibilityStatus::NotTested,
                oracle_targets: vec!["baseline".to_owned()],
                evidence: Vec::new(),
                differences: Vec::new(),
                owner_issue: 8,
                legal_review_id: None,
            }],
        };

        let violations = features.validate(&targets);

        assert_eq!(
            violations
                .iter()
                .filter(|violation| violation.code == "feature.category.missing")
                .count(),
            17
        );
    }

    fn oracle_target(id: &str, tag: &str) -> OracleTarget {
        OracleTarget {
            id: id.to_owned(),
            provenance_id: "prov-oracle-sqlserver-2022-cu26".to_owned(),
            product_release: "2022".to_owned(),
            servicing_update: "CU26".to_owned(),
            product_version: "16.0.4265.3".to_owned(),
            edition: "Developer".to_owned(),
            operating_system: "Ubuntu 22.04".to_owned(),
            architecture: "x86_64".to_owned(),
            container_repository: "mcr.microsoft.com/mssql/server".to_owned(),
            container_tag: tag.to_owned(),
            container_digest:
                "sha256:ba4c8329f48fb8f02e1416be6a930ebfd71268caee78aa985f3af4315e457c89".to_owned(),
            compatibility_level: 160,
            collation: "SQL_Latin1_General_CP1_CI_AS".to_owned(),
            language: "us_english".to_owned(),
            lcid: 1033,
            timezone: "UTC".to_owned(),
            session_settings: vec!["ANSI_NULLS ON".to_owned()],
        }
    }
}
