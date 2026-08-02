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

/// A provenance-backed activity that must be explicitly authorized.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProvenanceUse {
    /// Cite or inventory a source without using it as an implementation input.
    DocumentationReference,
    /// Use source material to guide implementation decisions.
    ImplementationInput,
    /// Include third-party code in a build or distributed artifact.
    DependencyInclusion,
    /// Execute a third-party tool or action for supply-chain verification.
    SupplyChainVerification,
    /// Apply license terms to the repository or a distributed artifact.
    ProjectLicensing,
    /// Apply contribution terms to repository submissions.
    ContributionPolicy,
    /// Install, configure, or observe a proprietary oracle.
    OracleOperation,
    /// Use a source or observation as conformance evidence.
    ConformanceEvidence,
    /// Import or derive data used as a test or conformance fixture.
    Fixture,
    /// Use a source or observation to support a public compatibility claim.
    ReleaseClaim,
}

/// Human legal-review decision for a provenance record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LegalReviewStatus {
    /// No qualified human reviewer has made a decision.
    Pending,
    /// A qualified human reviewer approved the recorded scope.
    Approved,
    /// A qualified human reviewer rejected the recorded scope.
    Rejected,
}

/// Classification of the source captured by a provenance record.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProvenanceSourceKind {
    /// Public product or API documentation.
    PublicDocumentation,
    /// A publicly available interoperability specification.
    OpenSpecification,
    /// A standard published by a recognized standards body.
    Standard,
    /// A public API or protocol surface.
    PublicApi,
    /// Product terms, license terms, or another legal instrument.
    LegalTerms,
    /// Facts observed from an independently operated oracle.
    OracleObservation,
    /// Clean-room behavior specification derived from approved inputs.
    BehaviorSpecification,
    /// Repository source code.
    SourceCode,
    /// Repository test code or a conformance case.
    Test,
    /// Test or conformance fixture data.
    Fixture,
    /// Third-party package or other dependency.
    Dependency,
    /// Repository-owned generated artifact.
    GeneratedArtifact,
}

/// Traceable source material that may be considered for governed use.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceRecord {
    /// Stable provenance identifier.
    pub id: String,
    /// Source classification.
    pub source_kind: ProvenanceSourceKind,
    /// Human-readable source title.
    pub title: String,
    /// Canonical public URL, when the source is external.
    pub source_url: Option<String>,
    /// Repository-relative path, when the record describes an owned artifact.
    pub artifact_path: Option<String>,
    /// Published revision, version, commit, or retrieval snapshot identifier.
    pub revision: String,
    /// ISO 8601 date on which the source was retrieved or generated.
    pub retrieved_on: String,
    /// Author, publisher, or artifact owner.
    pub author: String,
    /// Reproducible description of how this record was produced.
    pub generation_method: String,
    /// Relevant capture environment, or `None` for environment-neutral sources.
    pub environment: Option<String>,
    /// License or terms identifier governing the source.
    pub license: String,
    /// SHA-256 digest of the retained source snapshot or artifact.
    pub content_digest: String,
    /// Governed uses requested for this source.
    pub intended_uses: Vec<ProvenanceUse>,
    /// Provenance records from which this artifact was derived.
    pub parent_provenance_ids: Vec<String>,
    /// Legal review that decides the requested uses.
    pub legal_review_id: String,
}

/// Versioned provenance inventory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProvenanceLedger {
    /// Contract version used to interpret this ledger.
    pub schema_version: String,
    /// Recorded source and artifact provenance.
    pub records: Vec<ProvenanceRecord>,
}

/// A repository fixture discovered by the governance scanner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FixtureArtifact {
    /// Repository-relative fixture path.
    pub artifact_path: String,
    /// SHA-256 digest prefixed with `sha256:`.
    pub content_digest: String,
}

/// One human-owned legal decision and its exact authorization scope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LegalReviewRecord {
    /// Stable legal-review identifier.
    pub id: String,
    /// Question or material being reviewed.
    pub subject: String,
    /// Current human legal-review decision.
    pub status: LegalReviewStatus,
    /// Uses authorized by an approved decision.
    pub approved_uses: Vec<ProvenanceUse>,
    /// Uses explicitly prohibited by the decision.
    pub prohibited_uses: Vec<ProvenanceUse>,
    /// Uses that require a separate, narrower legal review.
    pub individual_review_uses: Vec<ProvenanceUse>,
    /// Provenance records for terms and facts considered by the reviewer.
    pub source_provenance_ids: Vec<String>,
    /// Qualified human reviewer, present only after a decision.
    pub reviewed_by: Option<String>,
    /// ISO 8601 decision date, present only after a decision.
    pub decided_on: Option<String>,
    /// Scope, conditions, and rationale for the decision.
    pub rationale: String,
}

/// Versioned human legal-review inventory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LegalReviewLedger {
    /// Contract version used to interpret this ledger.
    pub schema_version: String,
    /// Human legal-review records.
    pub reviews: Vec<LegalReviewRecord>,
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
    /// Provenance records supporting this inventory entry or compatibility status.
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

impl LegalReviewRecord {
    fn validate(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if self.id.trim().is_empty() {
            violations.push(ContractViolation {
                code: "legal-review.id.empty",
                message: "legal review id must not be empty".to_owned(),
            });
        }

        if self.subject.trim().is_empty() || self.rationale.trim().is_empty() {
            violations.push(ContractViolation {
                code: "legal-review.description.empty",
                message: format!("legal review {} requires a subject and rationale", self.id),
            });
        }

        if self.source_provenance_ids.is_empty() {
            violations.push(ContractViolation {
                code: "legal-review.source.empty",
                message: format!("legal review {} requires a source", self.id),
            });
        } else {
            let mut source_ids = BTreeSet::new();
            for source_id in &self.source_provenance_ids {
                if !source_ids.insert(source_id.as_str()) {
                    violations.push(ContractViolation {
                        code: "legal-review.source.duplicate",
                        message: format!("legal review {} repeats source {}", self.id, source_id),
                    });
                }
            }
        }

        let mut decided_uses = BTreeSet::new();
        for use_kind in self
            .approved_uses
            .iter()
            .chain(&self.prohibited_uses)
            .chain(&self.individual_review_uses)
        {
            if !decided_uses.insert(*use_kind) {
                violations.push(ContractViolation {
                    code: "legal-review.use.duplicate",
                    message: format!(
                        "legal review {} contains a duplicate or conflicting use {use_kind:?}",
                        self.id
                    ),
                });
            }
        }

        let has_decision_metadata = self
            .reviewed_by
            .as_deref()
            .is_some_and(|reviewer| !reviewer.trim().is_empty())
            && self.decided_on.as_deref().is_some_and(is_iso_date);

        match self.status {
            LegalReviewStatus::Pending => {
                if !decided_uses.is_empty()
                    || self.reviewed_by.is_some()
                    || self.decided_on.is_some()
                {
                    violations.push(ContractViolation {
                        code: "legal-review.pending.has-decision",
                        message: format!(
                            "pending legal review {} cannot contain a decision",
                            self.id
                        ),
                    });
                }
            }
            LegalReviewStatus::Approved => {
                if self.approved_uses.is_empty() {
                    violations.push(ContractViolation {
                        code: "legal-review.approved.scope-empty",
                        message: format!(
                            "approved legal review {} requires an approved use",
                            self.id
                        ),
                    });
                }
                if !has_decision_metadata {
                    violations.push(ContractViolation {
                        code: "legal-review.decision-metadata.missing",
                        message: format!(
                            "approved legal review {} requires a human reviewer and decision date",
                            self.id
                        ),
                    });
                }
            }
            LegalReviewStatus::Rejected => {
                if !self.approved_uses.is_empty() {
                    violations.push(ContractViolation {
                        code: "legal-review.rejected.has-approved-use",
                        message: format!("rejected legal review {} cannot approve a use", self.id),
                    });
                }
                if !has_decision_metadata {
                    violations.push(ContractViolation {
                        code: "legal-review.decision-metadata.missing",
                        message: format!(
                            "rejected legal review {} requires a human reviewer and decision date",
                            self.id
                        ),
                    });
                }
            }
        }

        violations
    }
}

impl LegalReviewLedger {
    /// Validates legal-review structure without treating pending work as approval.
    #[must_use]
    pub fn validate(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let mut review_ids = BTreeSet::new();

        if self.schema_version != CONFORMANCE_SCHEMA_VERSION {
            violations.push(ContractViolation {
                code: "legal-review.schema-version.unsupported",
                message: format!(
                    "unsupported legal review schema version: {}",
                    self.schema_version
                ),
            });
        }

        for review in &self.reviews {
            violations.extend(review.validate());
            if !review_ids.insert(review.id.as_str()) {
                violations.push(ContractViolation {
                    code: "legal-review.id.duplicate",
                    message: format!("duplicate legal review id: {}", review.id),
                });
            }
        }

        violations
    }
}

impl ProvenanceRecord {
    fn validate(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if self.id.trim().is_empty() {
            violations.push(ContractViolation {
                code: "provenance.id.empty",
                message: "provenance id must not be empty".to_owned(),
            });
        }

        if self.title.trim().is_empty()
            || self.revision.trim().is_empty()
            || self.author.trim().is_empty()
            || self.generation_method.trim().is_empty()
            || self.license.trim().is_empty()
        {
            violations.push(ContractViolation {
                code: "provenance.metadata.empty",
                message: format!("provenance {} has incomplete source metadata", self.id),
            });
        }

        if !is_iso_date(&self.retrieved_on) {
            violations.push(ContractViolation {
                code: "provenance.retrieved-on.invalid",
                message: format!("provenance {} requires an ISO 8601 date", self.id),
            });
        }

        if !is_sha256_digest(&self.content_digest) {
            violations.push(ContractViolation {
                code: "provenance.content-digest.invalid",
                message: format!("provenance {} requires a SHA-256 digest", self.id),
            });
        }

        if self.source_kind.is_external() {
            if !self
                .source_url
                .as_deref()
                .is_some_and(|url| url.starts_with("https://"))
            {
                violations.push(ContractViolation {
                    code: "provenance.source-url.missing",
                    message: format!("external provenance {} requires an HTTPS URL", self.id),
                });
            }
            if self.artifact_path.is_some() {
                violations.push(ContractViolation {
                    code: "provenance.artifact-path.unexpected",
                    message: format!(
                        "external provenance {} cannot name a repository artifact",
                        self.id
                    ),
                });
            }
        } else {
            if self.source_url.is_some() {
                violations.push(ContractViolation {
                    code: "provenance.source-url.unexpected",
                    message: format!(
                        "repository provenance {} cannot name an external source URL",
                        self.id
                    ),
                });
            }
            if !self
                .artifact_path
                .as_deref()
                .is_some_and(is_repository_relative_path)
            {
                violations.push(ContractViolation {
                    code: "provenance.artifact-path.missing",
                    message: format!(
                        "repository provenance {} requires a safe relative path",
                        self.id
                    ),
                });
            }
        }

        if self.intended_uses.is_empty() {
            violations.push(ContractViolation {
                code: "provenance.use.empty",
                message: format!("provenance {} requires an intended use", self.id),
            });
        } else if has_duplicates(&self.intended_uses) {
            violations.push(ContractViolation {
                code: "provenance.use.duplicate",
                message: format!("provenance {} contains duplicate intended uses", self.id),
            });
        }

        if self.legal_review_id.trim().is_empty() {
            violations.push(ContractViolation {
                code: "provenance.legal-review.missing",
                message: format!("provenance {} requires a legal review reference", self.id),
            });
        }

        violations
    }
}

impl ProvenanceSourceKind {
    fn is_external(self) -> bool {
        matches!(
            self,
            Self::PublicDocumentation
                | Self::OpenSpecification
                | Self::Standard
                | Self::PublicApi
                | Self::LegalTerms
                | Self::Dependency
        )
    }
}

impl ProvenanceLedger {
    /// Validates provenance structure and every cross-ledger reference.
    #[must_use]
    pub fn validate(&self, legal_reviews: &LegalReviewLedger) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let mut provenance_ids = BTreeSet::new();
        let legal_review_ids: BTreeSet<&str> = legal_reviews
            .reviews
            .iter()
            .map(|review| review.id.as_str())
            .collect();

        if self.schema_version != CONFORMANCE_SCHEMA_VERSION {
            violations.push(ContractViolation {
                code: "provenance.schema-version.unsupported",
                message: format!(
                    "unsupported provenance schema version: {}",
                    self.schema_version
                ),
            });
        }

        for record in &self.records {
            violations.extend(record.validate());
            if !provenance_ids.insert(record.id.as_str()) {
                violations.push(ContractViolation {
                    code: "provenance.id.duplicate",
                    message: format!("duplicate provenance id: {}", record.id),
                });
            }

            if !legal_review_ids.contains(record.legal_review_id.as_str()) {
                violations.push(ContractViolation {
                    code: "provenance.legal-review.unknown",
                    message: format!(
                        "provenance {} references unknown legal review {}",
                        record.id, record.legal_review_id
                    ),
                });
            } else if let Some(review) = legal_reviews
                .reviews
                .iter()
                .find(|review| review.id == record.legal_review_id)
                && !review.source_provenance_ids.contains(&record.id)
            {
                violations.push(ContractViolation {
                    code: "provenance.legal-review.source-unlisted",
                    message: format!(
                        "legal review {} does not list provenance {}",
                        review.id, record.id
                    ),
                });
            }
        }

        for record in &self.records {
            let mut parent_ids = BTreeSet::new();
            for parent_id in &record.parent_provenance_ids {
                if !parent_ids.insert(parent_id.as_str()) {
                    violations.push(ContractViolation {
                        code: "provenance.parent.duplicate",
                        message: format!("provenance {} repeats parent {}", record.id, parent_id),
                    });
                } else if parent_id == &record.id {
                    violations.push(ContractViolation {
                        code: "provenance.parent.self-reference",
                        message: format!("provenance {} references itself", record.id),
                    });
                } else if !provenance_ids.contains(parent_id.as_str()) {
                    violations.push(ContractViolation {
                        code: "provenance.parent.unknown",
                        message: format!(
                            "provenance {} references unknown parent {}",
                            record.id, parent_id
                        ),
                    });
                }
            }
        }

        for record in &self.records {
            let mut lineage = BTreeSet::new();
            if provenance_lineage_has_cycle(&record.id, &self.records, &mut lineage) {
                violations.push(ContractViolation {
                    code: "provenance.parent.cycle",
                    message: format!(
                        "provenance lineage contains a cycle reachable from {}",
                        record.id
                    ),
                });
                break;
            }
        }

        for review in &legal_reviews.reviews {
            for source_id in &review.source_provenance_ids {
                if !provenance_ids.contains(source_id.as_str()) {
                    violations.push(ContractViolation {
                        code: "legal-review.source.unknown",
                        message: format!(
                            "legal review {} references unknown provenance {}",
                            review.id, source_id
                        ),
                    });
                }
            }
        }

        violations
    }

    /// Validates that a provenance-backed activity has explicit human approval.
    #[must_use]
    pub fn validate_use(
        &self,
        legal_reviews: &LegalReviewLedger,
        provenance_id: &str,
        requested_use: ProvenanceUse,
    ) -> Vec<ContractViolation> {
        let provenance_matches: Vec<&ProvenanceRecord> = self
            .records
            .iter()
            .filter(|record| record.id == provenance_id)
            .collect();
        let [provenance] = provenance_matches.as_slice() else {
            let code = if provenance_matches.is_empty() {
                "provenance.id.unknown"
            } else {
                "provenance.id.duplicate"
            };
            return vec![ContractViolation {
                code,
                message: format!("provenance use requires one record: {provenance_id}"),
            }];
        };

        let review_matches: Vec<&LegalReviewRecord> = legal_reviews
            .reviews
            .iter()
            .filter(|review| review.id == provenance.legal_review_id)
            .collect();
        let [review] = review_matches.as_slice() else {
            let code = if review_matches.is_empty() {
                "provenance.legal-review.unknown"
            } else {
                "legal-review.id.duplicate"
            };
            return vec![ContractViolation {
                code,
                message: format!(
                    "provenance {} requires one legal review {}",
                    provenance.id, provenance.legal_review_id
                ),
            }];
        };

        let mut ledger_violations = legal_reviews.validate();
        ledger_violations.extend(self.validate(legal_reviews));
        if !ledger_violations.is_empty() {
            return ledger_violations;
        }

        if !provenance.intended_uses.contains(&requested_use) {
            return vec![ContractViolation {
                code: "provenance.use.undeclared",
                message: format!(
                    "provenance {} does not declare use {requested_use:?}",
                    provenance.id
                ),
            }];
        }

        match review.status {
            LegalReviewStatus::Pending => vec![ContractViolation {
                code: "provenance.legal-review.pending",
                message: format!(
                    "legal review {} is pending for provenance {}",
                    review.id, provenance.id
                ),
            }],
            LegalReviewStatus::Rejected => vec![ContractViolation {
                code: "provenance.legal-review.rejected",
                message: format!(
                    "legal review {} rejected use of provenance {}",
                    review.id, provenance.id
                ),
            }],
            LegalReviewStatus::Approved
                if review.reviewed_by.is_none()
                    || !review.decided_on.as_deref().is_some_and(is_iso_date) =>
            {
                vec![ContractViolation {
                    code: "legal-review.decision-metadata.missing",
                    message: format!(
                        "legal review {} lacks a human reviewer or decision date",
                        review.id
                    ),
                }]
            }
            LegalReviewStatus::Approved if review.approved_uses.contains(&requested_use) => {
                Vec::new()
            }
            LegalReviewStatus::Approved if review.prohibited_uses.contains(&requested_use) => {
                vec![ContractViolation {
                    code: "provenance.use.prohibited",
                    message: format!("legal review {} prohibits use {requested_use:?}", review.id),
                }]
            }
            LegalReviewStatus::Approved
                if review.individual_review_uses.contains(&requested_use) =>
            {
                vec![ContractViolation {
                    code: "provenance.use.individual-review-required",
                    message: format!(
                        "legal review {} requires a separate review for use {requested_use:?}",
                        review.id
                    ),
                }]
            }
            LegalReviewStatus::Approved => {
                vec![ContractViolation {
                    code: "provenance.use.not-approved",
                    message: format!(
                        "legal review {} does not approve use {requested_use:?}",
                        review.id
                    ),
                }]
            }
        }
    }

    /// Validates discovered fixture files against provenance and legal approval.
    #[must_use]
    pub fn validate_fixture_inventory(
        &self,
        legal_reviews: &LegalReviewLedger,
        fixtures: &[FixtureArtifact],
    ) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let mut fixture_paths = BTreeSet::new();

        for fixture in fixtures {
            if !fixture_paths.insert(fixture.artifact_path.as_str()) {
                violations.push(ContractViolation {
                    code: "fixture.path.duplicate",
                    message: format!(
                        "fixture scanner reported {} more than once",
                        fixture.artifact_path
                    ),
                });
                continue;
            }

            let matching_records: Vec<&ProvenanceRecord> = self
                .records
                .iter()
                .filter(|record| {
                    record.artifact_path.as_deref() == Some(fixture.artifact_path.as_str())
                })
                .collect();

            if matching_records.is_empty() {
                violations.push(ContractViolation {
                    code: "fixture.provenance.unregistered",
                    message: format!("fixture {} has no provenance record", fixture.artifact_path),
                });
                continue;
            }

            if matching_records.len() > 1 {
                violations.push(ContractViolation {
                    code: "fixture.provenance.duplicate",
                    message: format!(
                        "fixture {} has multiple provenance records",
                        fixture.artifact_path
                    ),
                });
                continue;
            }

            let record = matching_records[0];
            if record.source_kind != ProvenanceSourceKind::Fixture {
                violations.push(ContractViolation {
                    code: "fixture.provenance.kind",
                    message: format!(
                        "fixture {} must use the fixture source kind",
                        fixture.artifact_path
                    ),
                });
            }
            if !record.intended_uses.contains(&ProvenanceUse::Fixture) {
                violations.push(ContractViolation {
                    code: "fixture.provenance.use-missing",
                    message: format!(
                        "fixture {} must declare the fixture use",
                        fixture.artifact_path
                    ),
                });
            }
            if !record
                .content_digest
                .eq_ignore_ascii_case(&fixture.content_digest)
            {
                violations.push(ContractViolation {
                    code: "fixture.content-digest.mismatch",
                    message: format!(
                        "fixture {} does not match its recorded SHA-256 digest",
                        fixture.artifact_path
                    ),
                });
            }

            violations.extend(self.validate_use(legal_reviews, &record.id, ProvenanceUse::Fixture));
        }

        for record in &self.records {
            if (record.source_kind == ProvenanceSourceKind::Fixture
                || record.intended_uses.contains(&ProvenanceUse::Fixture))
                && record
                    .artifact_path
                    .as_deref()
                    .is_some_and(|path| !fixture_paths.contains(path))
            {
                violations.push(ContractViolation {
                    code: "fixture.artifact.missing",
                    message: format!("fixture provenance {} points to a missing file", record.id),
                });
            }
        }

        violations
    }
}

/// Validates references shared by the compatibility and governance ledgers.
#[must_use]
pub fn validate_governance_references(
    targets: &TargetMatrix,
    features: &FeatureMatrix,
    provenance: &ProvenanceLedger,
    legal_reviews: &LegalReviewLedger,
) -> Vec<ContractViolation> {
    let mut violations = legal_reviews.validate();
    violations.extend(provenance.validate(legal_reviews));
    let provenance_ids: BTreeSet<&str> = provenance
        .records
        .iter()
        .map(|record| record.id.as_str())
        .collect();
    let legal_review_ids: BTreeSet<&str> = legal_reviews
        .reviews
        .iter()
        .map(|review| review.id.as_str())
        .collect();

    for target in &targets.targets {
        if !provenance_ids.contains(target.provenance_id.as_str()) {
            violations.push(ContractViolation {
                code: "target.provenance.unknown",
                message: format!(
                    "target {} references unknown provenance {}",
                    target.id, target.provenance_id
                ),
            });
        }
    }

    for feature in &features.features {
        for provenance_id in &feature.evidence {
            if !provenance_ids.contains(provenance_id.as_str()) {
                violations.push(ContractViolation {
                    code: "feature.provenance.unknown",
                    message: format!(
                        "feature {} references unknown provenance {}",
                        feature.id, provenance_id
                    ),
                });
            }
        }

        if let Some(review_id) = &feature.legal_review_id
            && !legal_review_ids.contains(review_id.as_str())
        {
            violations.push(ContractViolation {
                code: "feature.legal-review.unknown",
                message: format!(
                    "feature {} references unknown legal review {}",
                    feature.id, review_id
                ),
            });
        }
    }

    violations
}

impl ConformanceRecord {
    /// Validates that a conformance run used an approved oracle and evidence source.
    #[must_use]
    pub fn validate_governance(
        &self,
        targets: &TargetMatrix,
        provenance: &ProvenanceLedger,
        legal_reviews: &LegalReviewLedger,
    ) -> Vec<ContractViolation> {
        let Some(target) = targets
            .targets
            .iter()
            .find(|target| target.id == self.target_id)
        else {
            return vec![ContractViolation {
                code: "conformance.target.unknown",
                message: format!("unknown conformance target: {}", self.target_id),
            }];
        };

        let mut violations = provenance.validate_use(
            legal_reviews,
            &target.provenance_id,
            ProvenanceUse::OracleOperation,
        );
        violations.extend(provenance.validate_use(
            legal_reviews,
            &self.provenance_id,
            ProvenanceUse::ConformanceEvidence,
        ));
        violations
    }
}

impl FeatureMatrix {
    /// Validates the approved clean-room inputs for implementation of one feature.
    #[must_use]
    pub fn validate_implementation_inputs(
        &self,
        feature_id: &str,
        provenance: &ProvenanceLedger,
        legal_reviews: &LegalReviewLedger,
    ) -> Vec<ContractViolation> {
        let Some(feature) = self
            .features
            .iter()
            .find(|feature| feature.id == feature_id)
        else {
            return vec![ContractViolation {
                code: "feature.id.unknown",
                message: format!("unknown feature: {feature_id}"),
            }];
        };

        if feature.status == CompatibilityStatus::BlockedLegal {
            return vec![ContractViolation {
                code: "feature.implementation.blocked-legal",
                message: format!("feature {} is blocked by legal review", feature.id),
            }];
        }

        feature
            .evidence
            .iter()
            .flat_map(|provenance_id| {
                provenance.validate_use(
                    legal_reviews,
                    provenance_id,
                    ProvenanceUse::ImplementationInput,
                )
            })
            .collect()
    }
}

fn has_duplicates<T: Copy + Ord>(values: &[T]) -> bool {
    let mut unique = BTreeSet::new();
    values.iter().any(|value| !unique.insert(*value))
}

fn is_iso_date(value: &str) -> bool {
    value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

fn is_repository_relative_path(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains('\\')
        && !value.split('/').any(|component| component == "..")
}

fn provenance_lineage_has_cycle<'a>(
    provenance_id: &'a str,
    records: &'a [ProvenanceRecord],
    lineage: &mut BTreeSet<&'a str>,
) -> bool {
    if !lineage.insert(provenance_id) {
        return true;
    }

    let has_cycle = records
        .iter()
        .find(|record| record.id == provenance_id)
        .is_some_and(|record| {
            record
                .parent_provenance_ids
                .iter()
                .any(|parent_id| provenance_lineage_has_cycle(parent_id, records, lineage))
        });
    lineage.remove(provenance_id);
    has_cycle
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
        FixtureArtifact, LegalReviewLedger, LegalReviewRecord, LegalReviewStatus, OracleTarget,
        ProvenanceLedger, ProvenanceRecord, ProvenanceSourceKind, ProvenanceUse, TargetMatrix,
        validate_governance_references,
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
    fn governed_use_rejects_unknown_provenance() {
        let violations = provenance_ledger().validate_use(
            &pending_legal_reviews(),
            "prov-unknown",
            ProvenanceUse::ImplementationInput,
        );

        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, "provenance.id.unknown");
    }

    #[test]
    fn governed_use_rejects_pending_legal_review() {
        let violations = provenance_ledger().validate_use(
            &pending_legal_reviews(),
            "prov-public-specification",
            ProvenanceUse::ImplementationInput,
        );

        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, "provenance.legal-review.pending");
    }

    #[test]
    fn governed_use_rejects_malformed_provenance_despite_approved_status() {
        let mut provenance = provenance_ledger();
        provenance.records[0].content_digest = "sha256:not-a-digest".to_owned();
        let mut legal_reviews = pending_legal_reviews();
        let review = &mut legal_reviews.reviews[0];
        review.status = LegalReviewStatus::Approved;
        review.approved_uses = vec![ProvenanceUse::ImplementationInput];
        review.reviewed_by = Some("Qualified legal reviewer".to_owned());
        review.decided_on = Some("2026-08-02".to_owned());

        let violations = provenance.validate_use(
            &legal_reviews,
            "prov-public-specification",
            ProvenanceUse::ImplementationInput,
        );

        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "provenance.content-digest.invalid")
        );
    }

    #[test]
    fn governed_use_rejects_review_that_does_not_cover_source() {
        let provenance = provenance_ledger();
        let mut legal_reviews = pending_legal_reviews();
        legal_reviews.reviews[0].source_provenance_ids = vec!["prov-other".to_owned()];

        let violations = provenance.validate_use(
            &legal_reviews,
            "prov-public-specification",
            ProvenanceUse::ImplementationInput,
        );

        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "provenance.legal-review.source-unlisted")
        );
    }

    #[test]
    fn governed_use_rejects_prohibited_and_individually_reviewed_scopes() {
        let provenance = provenance_ledger();
        let mut prohibited_reviews = approved_legal_reviews();
        prohibited_reviews.reviews[0].approved_uses = vec![ProvenanceUse::DocumentationReference];
        prohibited_reviews.reviews[0].prohibited_uses = vec![ProvenanceUse::ImplementationInput];

        let prohibited = provenance.validate_use(
            &prohibited_reviews,
            "prov-public-specification",
            ProvenanceUse::ImplementationInput,
        );

        assert_eq!(prohibited.len(), 1);
        assert_eq!(prohibited[0].code, "provenance.use.prohibited");

        let mut individual_reviews = approved_legal_reviews();
        individual_reviews.reviews[0].approved_uses = vec![ProvenanceUse::DocumentationReference];
        individual_reviews.reviews[0].individual_review_uses =
            vec![ProvenanceUse::ImplementationInput];

        let individual = provenance.validate_use(
            &individual_reviews,
            "prov-public-specification",
            ProvenanceUse::ImplementationInput,
        );

        assert_eq!(individual.len(), 1);
        assert_eq!(
            individual[0].code,
            "provenance.use.individual-review-required"
        );
    }

    #[test]
    fn legal_review_states_require_consistent_human_decisions() {
        let mut pending = pending_legal_reviews();
        pending.reviews[0].reviewed_by = Some("Reviewer".to_owned());
        assert!(
            pending
                .validate()
                .iter()
                .any(|violation| violation.code == "legal-review.pending.has-decision")
        );

        let mut approved = pending_legal_reviews();
        approved.reviews[0].status = LegalReviewStatus::Approved;
        approved.reviews[0].approved_uses = vec![ProvenanceUse::ImplementationInput];
        assert!(
            approved
                .validate()
                .iter()
                .any(|violation| violation.code == "legal-review.decision-metadata.missing")
        );

        let mut rejected = pending_legal_reviews();
        rejected.reviews[0].status = LegalReviewStatus::Rejected;
        assert!(
            rejected
                .validate()
                .iter()
                .any(|violation| violation.code == "legal-review.decision-metadata.missing")
        );
    }

    #[test]
    fn legal_review_requires_unique_sources() {
        let mut legal_reviews = pending_legal_reviews();
        legal_reviews.reviews[0].source_provenance_ids.clear();
        assert!(
            legal_reviews
                .validate()
                .iter()
                .any(|violation| violation.code == "legal-review.source.empty")
        );

        legal_reviews.reviews[0].source_provenance_ids = vec![
            "prov-public-specification".to_owned(),
            "prov-public-specification".to_owned(),
        ];
        assert!(
            legal_reviews
                .validate()
                .iter()
                .any(|violation| violation.code == "legal-review.source.duplicate")
        );
    }

    #[test]
    fn provenance_rejects_conflicting_source_locations() {
        let mut external = provenance_ledger();
        external.records[0].artifact_path = Some("contracts/spec.json".to_owned());
        assert!(
            external
                .validate(&pending_legal_reviews())
                .iter()
                .any(|violation| violation.code == "provenance.artifact-path.unexpected")
        );

        let mut repository = provenance_ledger();
        repository.records[0].source_kind = ProvenanceSourceKind::BehaviorSpecification;
        repository.records[0].artifact_path = Some("contracts/spec.json".to_owned());
        assert!(
            repository
                .validate(&pending_legal_reviews())
                .iter()
                .any(|violation| violation.code == "provenance.source-url.unexpected")
        );
    }

    #[test]
    fn provenance_rejects_indirect_lineage_cycles() {
        let mut provenance = provenance_ledger();
        provenance.records[0].parent_provenance_ids = vec!["prov-derived".to_owned()];
        let mut derived = provenance.records[0].clone();
        derived.id = "prov-derived".to_owned();
        derived.parent_provenance_ids = vec!["prov-public-specification".to_owned()];
        provenance.records.push(derived);
        let mut legal_reviews = pending_legal_reviews();
        legal_reviews.reviews[0]
            .source_provenance_ids
            .push("prov-derived".to_owned());

        let violations = provenance.validate(&legal_reviews);

        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "provenance.parent.cycle")
        );
    }

    #[test]
    fn fixture_inventory_rejects_unregistered_files() {
        let fixtures = vec![FixtureArtifact {
            artifact_path: "tests/fixtures/unregistered.bin".to_owned(),
            content_digest:
                "sha256:ba4c8329f48fb8f02e1416be6a930ebfd71268caee78aa985f3af4315e457c89".to_owned(),
        }];

        let violations =
            provenance_ledger().validate_fixture_inventory(&pending_legal_reviews(), &fixtures);

        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "fixture.provenance.unregistered")
        );
    }

    #[test]
    fn fixture_inventory_rejects_digest_mismatch_and_missing_files() {
        let (provenance, legal_reviews) = approved_fixture_governance();
        let mismatched = vec![FixtureArtifact {
            artifact_path: "tests/fixtures/case.bin".to_owned(),
            content_digest:
                "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
        }];

        let mismatch_violations =
            provenance.validate_fixture_inventory(&legal_reviews, &mismatched);
        let missing_violations = provenance.validate_fixture_inventory(&legal_reviews, &[]);

        assert!(
            mismatch_violations
                .iter()
                .any(|violation| violation.code == "fixture.content-digest.mismatch")
        );
        assert!(
            missing_violations
                .iter()
                .any(|violation| violation.code == "fixture.artifact.missing")
        );
    }

    #[test]
    fn fixture_inventory_accepts_approved_matching_files() {
        let (provenance, legal_reviews) = approved_fixture_governance();
        let fixtures = vec![FixtureArtifact {
            artifact_path: "tests/fixtures/case.bin".to_owned(),
            content_digest:
                "sha256:ba4c8329f48fb8f02e1416be6a930ebfd71268caee78aa985f3af4315e457c89".to_owned(),
        }];

        let violations = provenance.validate_fixture_inventory(&legal_reviews, &fixtures);

        assert!(violations.is_empty(), "{violations:#?}");
    }

    #[test]
    fn governance_references_reject_unknown_targets_and_features() {
        let targets = TargetMatrix {
            schema_version: "1.0.0".to_owned(),
            baseline_target_id: "baseline".to_owned(),
            targets: vec![oracle_target("baseline", "2022-CU26-ubuntu-22.04")],
            expansion_order: Vec::new(),
        };
        let features = FeatureMatrix {
            schema_version: "1.0.0".to_owned(),
            features: vec![FeatureRecord {
                id: "language.select".to_owned(),
                title: "SELECT".to_owned(),
                category: FeatureCategory::Language,
                status: CompatibilityStatus::NotTested,
                oracle_targets: vec!["baseline".to_owned()],
                evidence: vec!["prov-unknown".to_owned()],
                differences: Vec::new(),
                owner_issue: 8,
                legal_review_id: None,
            }],
        };
        let mut unknown_target = targets.clone();
        unknown_target.targets[0].provenance_id = "prov-unknown".to_owned();

        let violations = validate_governance_references(
            &unknown_target,
            &features,
            &provenance_ledger(),
            &pending_legal_reviews(),
        );

        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "target.provenance.unknown")
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "feature.provenance.unknown")
        );
    }

    #[test]
    fn blocked_feature_rejects_implementation() {
        let features = FeatureMatrix {
            schema_version: "1.0.0".to_owned(),
            features: vec![FeatureRecord {
                id: "storage.native-file-formats".to_owned(),
                title: "Native file formats".to_owned(),
                category: FeatureCategory::StorageRecovery,
                status: CompatibilityStatus::BlockedLegal,
                oracle_targets: vec!["baseline".to_owned()],
                evidence: Vec::new(),
                differences: Vec::new(),
                owner_issue: 3,
                legal_review_id: Some("legal-review-native-file-formats".to_owned()),
            }],
        };

        let violations = features.validate_implementation_inputs(
            "storage.native-file-formats",
            &provenance_ledger(),
            &pending_legal_reviews(),
        );

        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, "feature.implementation.blocked-legal");
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

    fn provenance_ledger() -> ProvenanceLedger {
        ProvenanceLedger {
            schema_version: "1.0.0".to_owned(),
            records: vec![ProvenanceRecord {
                id: "prov-public-specification".to_owned(),
                source_kind: ProvenanceSourceKind::OpenSpecification,
                title: "Public specification".to_owned(),
                source_url: Some("https://example.com/specification".to_owned()),
                artifact_path: None,
                revision: "1.0".to_owned(),
                retrieved_on: "2026-08-02".to_owned(),
                author: "Example publisher".to_owned(),
                generation_method: "Downloaded without modification".to_owned(),
                environment: None,
                license: "Terms under review".to_owned(),
                content_digest:
                    "sha256:ba4c8329f48fb8f02e1416be6a930ebfd71268caee78aa985f3af4315e457c89"
                        .to_owned(),
                intended_uses: vec![ProvenanceUse::ImplementationInput],
                parent_provenance_ids: Vec::new(),
                legal_review_id: "legal-review-public-specification".to_owned(),
            }],
        }
    }

    fn pending_legal_reviews() -> LegalReviewLedger {
        LegalReviewLedger {
            schema_version: "1.0.0".to_owned(),
            reviews: vec![LegalReviewRecord {
                id: "legal-review-public-specification".to_owned(),
                subject: "Use of the public specification".to_owned(),
                status: LegalReviewStatus::Pending,
                approved_uses: Vec::new(),
                prohibited_uses: Vec::new(),
                individual_review_uses: Vec::new(),
                source_provenance_ids: vec!["prov-public-specification".to_owned()],
                reviewed_by: None,
                decided_on: None,
                rationale: "Awaiting qualified human legal review".to_owned(),
            }],
        }
    }

    fn approved_legal_reviews() -> LegalReviewLedger {
        let mut legal_reviews = pending_legal_reviews();
        let review = &mut legal_reviews.reviews[0];
        review.status = LegalReviewStatus::Approved;
        review.approved_uses = vec![ProvenanceUse::ImplementationInput];
        review.reviewed_by = Some("Qualified legal reviewer".to_owned());
        review.decided_on = Some("2026-08-02".to_owned());
        legal_reviews
    }

    fn approved_fixture_governance() -> (ProvenanceLedger, LegalReviewLedger) {
        let mut provenance = provenance_ledger();
        let record = &mut provenance.records[0];
        record.source_kind = ProvenanceSourceKind::Fixture;
        record.source_url = None;
        record.artifact_path = Some("tests/fixtures/case.bin".to_owned());
        record.intended_uses = vec![ProvenanceUse::Fixture];

        let mut legal_reviews = approved_legal_reviews();
        legal_reviews.reviews[0].approved_uses = vec![ProvenanceUse::Fixture];

        (provenance, legal_reviews)
    }
}
