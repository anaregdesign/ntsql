//! Types and invariants for ntsql compatibility evidence.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current version of the conformance record contract.
pub const CONFORMANCE_SCHEMA_VERSION: &str = "1.0.0";

/// Current version of the legal-review ledger contract.
pub const LEGAL_REVIEW_SCHEMA_VERSION: &str = "2.0.0";

/// Current version of authenticated legal-decision authority input.
pub const LEGAL_DECISION_AUTHORITY_SCHEMA_VERSION: &str = "1.0.0";

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

/// Stable GitHub identity of the human who made a legal-review decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LegalReviewerIdentity {
    /// Stable numeric GitHub account identifier.
    pub github_account_id: u64,
    /// GitHub login recorded with the decision for audit readability.
    pub github_login: String,
}

/// Immutable GitHub pull-request review referenced by a legal decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LegalDecisionEvidenceReference {
    /// Repository in `owner/name` form.
    pub repository: String,
    /// Pull request containing the reviewed legal-ledger decision.
    pub pull_request_number: u64,
    /// Identifier repeated in exactly one immutable authenticated review.
    pub attestation_id: String,
}

/// One legal decision attested by an authenticated pull-request review.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LegalDecisionAttestation {
    /// Identifier chosen before review and recorded in the ledger decision.
    pub attestation_id: String,
    /// Complete legal-review decision parsed from the authenticated review body.
    pub decision: LegalReviewRecord,
    /// Complete provenance closure reviewed with the decision.
    pub provenance_records: Vec<ProvenanceRecord>,
}

/// State returned by GitHub for an authenticated pull-request review.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthenticatedReviewState {
    /// The reviewer approved the exact commit.
    Approved,
    /// The review was dismissed after submission.
    Dismissed,
    /// The reviewer requested changes.
    ChangesRequested,
    /// The review contains a non-approving comment.
    Commented,
}

/// Pull-request review data obtained from an authenticated GitHub API response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedPullRequestReview {
    /// Repository in `owner/name` form.
    pub repository: String,
    /// Pull request containing the reviewed legal-ledger decision.
    pub pull_request_number: u64,
    /// Immutable GitHub pull-request review identifier.
    pub review_id: u64,
    /// Stable identity returned for the review author.
    pub reviewer: LegalReviewerIdentity,
    /// Exact commit associated with the review.
    pub reviewed_commit_sha: String,
    /// Current authenticated review state.
    pub state: AuthenticatedReviewState,
    /// UTC timestamp at which GitHub recorded the review.
    pub submitted_at: String,
    /// Legal decisions explicitly attested in the review body.
    pub attestations: Vec<LegalDecisionAttestation>,
}

/// Authenticated context and reviews for one pull request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthenticatedPullRequest {
    /// Repository in `owner/name` form.
    pub repository: String,
    /// Pull request containing reviewed legal-ledger decisions.
    pub pull_request_number: u64,
    /// Stable identity of the pull-request author.
    pub pull_request_author_account_id: u64,
    /// Current head commit of the pull request when evidence was collected.
    pub candidate_commit_sha: String,
    /// Reviews obtained from authenticated GitHub API responses.
    pub authenticated_reviews: Vec<AuthenticatedPullRequestReview>,
}

/// Out-of-branch authority used to authenticate legal-ledger decisions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LegalDecisionAuthority {
    /// Contract version used to interpret this authority input.
    pub schema_version: String,
    /// Candidate repository for which this authority was generated.
    pub candidate_repository: String,
    /// Candidate commit for which this authority was generated.
    pub candidate_commit_sha: String,
    /// Stable account identifiers obtained from the protected trust anchor.
    pub trusted_reviewer_account_ids: Vec<u64>,
    /// Authenticated pull requests referenced by non-pending decisions.
    pub pull_requests: Vec<AuthenticatedPullRequest>,
}

/// Trusted event context against which an authority document is verified.
#[derive(Clone, Copy, Debug)]
pub struct LegalDecisionVerificationContext<'a> {
    /// Authority supplied outside the candidate checkout.
    pub authority: &'a LegalDecisionAuthority,
    /// Repository obtained from the trusted event context.
    pub candidate_repository: &'a str,
    /// Commit obtained from the trusted event context.
    pub candidate_commit_sha: &'a str,
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
    /// Stable identity of the qualified human reviewer, present only after a decision.
    pub reviewed_by: Option<LegalReviewerIdentity>,
    /// ISO 8601 decision date, present only after a decision.
    pub decided_on: Option<String>,
    /// Immutable authenticated review evidence, present only after a decision.
    pub decision_evidence: Option<LegalDecisionEvidenceReference>,
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

impl LegalReviewerIdentity {
    fn is_well_formed(&self) -> bool {
        self.github_account_id > 0 && is_github_login(&self.github_login)
    }
}

impl LegalDecisionEvidenceReference {
    fn is_well_formed(&self) -> bool {
        is_github_repository(&self.repository)
            && self.pull_request_number > 0
            && is_contract_identifier(&self.attestation_id)
    }
}

impl LegalDecisionAuthority {
    fn validate(
        &self,
        candidate_repository: &str,
        candidate_commit_sha: &str,
    ) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if self.schema_version != LEGAL_DECISION_AUTHORITY_SCHEMA_VERSION {
            violations.push(ContractViolation {
                code: "legal-review.authority.malformed",
                message: "legal-review authority uses an unsupported schema version".to_owned(),
            });
        }
        if !is_github_repository(&self.candidate_repository)
            || !is_git_commit_sha(&self.candidate_commit_sha)
        {
            violations.push(ContractViolation {
                code: "legal-review.authority.candidate.malformed",
                message: "legal-review authority contains a malformed candidate target".to_owned(),
            });
        }
        if self.candidate_repository != candidate_repository
            || self.candidate_commit_sha != candidate_commit_sha
        {
            violations.push(ContractViolation {
                code: "legal-review.authority.candidate.mismatch",
                message: "legal-review authority does not match the trusted candidate target"
                    .to_owned(),
            });
        }

        let mut trusted_reviewers = BTreeSet::new();
        if self.trusted_reviewer_account_ids.is_empty() {
            violations.push(ContractViolation {
                code: "legal-review.authority.trusted-reviewer.missing",
                message: "legal-review authority requires a trusted reviewer".to_owned(),
            });
        }
        for reviewer_id in &self.trusted_reviewer_account_ids {
            if *reviewer_id == 0 || !trusted_reviewers.insert(*reviewer_id) {
                violations.push(ContractViolation {
                    code: "legal-review.authority.trusted-reviewer.invalid",
                    message: format!(
                        "trusted reviewer account identifiers must be nonzero and unique: {reviewer_id}"
                    ),
                });
            }
        }

        let mut pull_requests = BTreeSet::new();
        let mut review_ids = BTreeSet::new();
        let mut attestation_keys = BTreeSet::new();
        if self.pull_requests.is_empty() {
            violations.push(ContractViolation {
                code: "legal-review.authority.pull-request.missing",
                message: "legal-review authority requires a pull-request context".to_owned(),
            });
        }
        for pull_request in &self.pull_requests {
            if pull_request.repository != self.candidate_repository {
                violations.push(ContractViolation {
                    code: "legal-review.authority.pull-request.repository-mismatch",
                    message: format!(
                        "pull request {}/{} is outside candidate repository {}",
                        pull_request.repository,
                        pull_request.pull_request_number,
                        self.candidate_repository
                    ),
                });
            }
            if !is_github_repository(&pull_request.repository)
                || pull_request.pull_request_number == 0
                || pull_request.pull_request_author_account_id == 0
                || !is_git_commit_sha(&pull_request.candidate_commit_sha)
            {
                violations.push(ContractViolation {
                    code: "legal-review.authority.pull-request.malformed",
                    message: "legal-review authority contains a malformed pull request".to_owned(),
                });
            }

            if !pull_requests.insert((
                pull_request.repository.as_str(),
                pull_request.pull_request_number,
            )) {
                violations.push(ContractViolation {
                    code: "legal-review.authority.pull-request.duplicate",
                    message: format!(
                        "legal-review authority repeats pull request {}/{}",
                        pull_request.repository, pull_request.pull_request_number
                    ),
                });
            }

            for review in &pull_request.authenticated_reviews {
                if !is_github_repository(&review.repository)
                    || review.pull_request_number == 0
                    || review.review_id == 0
                    || !review.reviewer.is_well_formed()
                    || !is_git_commit_sha(&review.reviewed_commit_sha)
                    || !is_iso_utc_timestamp(&review.submitted_at)
                {
                    violations.push(ContractViolation {
                        code: "legal-review.evidence.malformed",
                        message: format!(
                            "authenticated review {} contains malformed evidence",
                            review.review_id
                        ),
                    });
                }

                if review.repository != pull_request.repository
                    || review.pull_request_number != pull_request.pull_request_number
                {
                    violations.push(ContractViolation {
                        code: "legal-review.evidence.pull-request-mismatch",
                        message: format!(
                            "authenticated review {} is not from its pull-request context",
                            review.review_id
                        ),
                    });
                }

                if !review_ids.insert(review.review_id) {
                    violations.push(ContractViolation {
                        code: "legal-review.evidence.duplicate",
                        message: format!(
                            "authenticated review {} appears more than once",
                            review.review_id
                        ),
                    });
                }

                for attestation in &review.attestations {
                    if !is_contract_identifier(&attestation.attestation_id)
                        || attestation.decision.status == LegalReviewStatus::Pending
                        || !attestation.decision.validate().is_empty()
                        || attestation
                            .provenance_records
                            .iter()
                            .any(|record| !record.validate().is_empty())
                        || !is_complete_provenance_snapshot(
                            &attestation.decision,
                            &attestation.provenance_records,
                        )
                    {
                        violations.push(ContractViolation {
                            code: "legal-review.evidence.attestation.malformed",
                            message: format!(
                                "authenticated review {} contains a malformed attestation",
                                review.review_id
                            ),
                        });
                    }
                    if !attestation_keys.insert((
                        review.repository.as_str(),
                        review.pull_request_number,
                        review.reviewed_commit_sha.as_str(),
                        attestation.attestation_id.as_str(),
                    )) {
                        violations.push(ContractViolation {
                            code: "legal-review.evidence.attestation.duplicate",
                            message: format!(
                                "pull request {}/{} repeats attestation {} for commit {}",
                                review.repository,
                                review.pull_request_number,
                                attestation.attestation_id,
                                review.reviewed_commit_sha
                            ),
                        });
                    }
                }
            }
        }

        violations
    }
}

impl LegalReviewRecord {
    fn validate(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if self.id.trim().is_empty() {
            violations.push(ContractViolation {
                code: "legal-review.id.empty",
                message: "legal review id must not be empty".to_owned(),
            });
        } else if !is_contract_identifier(&self.id) {
            violations.push(ContractViolation {
                code: "legal-review.id.invalid",
                message: format!("legal review id is malformed: {}", self.id),
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
                if !is_contract_identifier(source_id) {
                    violations.push(ContractViolation {
                        code: "legal-review.source.invalid",
                        message: format!(
                            "legal review {} contains malformed source {}",
                            self.id, source_id
                        ),
                    });
                }
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
            .as_ref()
            .is_some_and(LegalReviewerIdentity::is_well_formed)
            && self.decided_on.as_deref().is_some_and(is_iso_date)
            && self
                .decision_evidence
                .as_ref()
                .is_some_and(LegalDecisionEvidenceReference::is_well_formed);

        match self.status {
            LegalReviewStatus::Pending => {
                if !decided_uses.is_empty()
                    || self.reviewed_by.is_some()
                    || self.decided_on.is_some()
                    || self.decision_evidence.is_some()
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
                            "approved legal review {} requires a reviewer, decision date, and evidence",
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
                            "rejected legal review {} requires a reviewer, decision date, and evidence",
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

        if self.schema_version != LEGAL_REVIEW_SCHEMA_VERSION {
            violations.push(ContractViolation {
                code: "legal-review.schema-version.unsupported",
                message: format!(
                    "unsupported legal review schema version: {}",
                    self.schema_version
                ),
            });
        }
        if self.reviews.is_empty() {
            violations.push(ContractViolation {
                code: "legal-review.record.missing",
                message: "legal-review ledger requires at least one review".to_owned(),
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

    /// Validates decisions at a governed-use boundary.
    #[must_use]
    pub fn validate_for_governed_use(
        &self,
        provenance: &ProvenanceLedger,
        verification: Option<LegalDecisionVerificationContext<'_>>,
    ) -> Vec<ContractViolation> {
        if let Some(verification) = verification {
            return self.validate_authenticated_decisions(provenance, verification);
        }

        let mut violations = self.validate();
        if self
            .reviews
            .iter()
            .any(|review| review.status != LegalReviewStatus::Pending)
        {
            violations.push(ContractViolation {
                code: "legal-review.authority.required",
                message: "non-pending legal decisions require out-of-branch authority".to_owned(),
            });
        }
        violations
    }

    /// Validates every non-pending decision against out-of-branch GitHub evidence.
    #[must_use]
    pub fn validate_authenticated_decisions(
        &self,
        provenance: &ProvenanceLedger,
        verification: LegalDecisionVerificationContext<'_>,
    ) -> Vec<ContractViolation> {
        let authority = verification.authority;
        let mut violations = self.validate();
        violations.extend(provenance.validate(self));
        violations.extend(authority.validate(
            verification.candidate_repository,
            verification.candidate_commit_sha,
        ));

        for review in self
            .reviews
            .iter()
            .filter(|review| review.status != LegalReviewStatus::Pending)
        {
            let (Some(reviewer), Some(reference), Some(decided_on)) = (
                review.reviewed_by.as_ref(),
                review.decision_evidence.as_ref(),
                review.decided_on.as_deref(),
            ) else {
                continue;
            };

            let pull_requests = authority
                .pull_requests
                .iter()
                .filter(|pull_request| {
                    pull_request.repository == reference.repository
                        && pull_request.pull_request_number == reference.pull_request_number
                })
                .collect::<Vec<_>>();
            let [pull_request] = pull_requests.as_slice() else {
                violations.push(ContractViolation {
                    code: "legal-review.evidence.pull-request-mismatch",
                    message: format!(
                        "legal review {} does not reference one authenticated pull request",
                        review.id
                    ),
                });
                continue;
            };

            let evidence_matches = pull_request
                .authenticated_reviews
                .iter()
                .filter(|evidence| {
                    evidence.repository == pull_request.repository
                        && evidence.pull_request_number == pull_request.pull_request_number
                        && evidence.reviewed_commit_sha == pull_request.candidate_commit_sha
                        && evidence.attestations.iter().any(|attestation| {
                            attestation.attestation_id == reference.attestation_id
                                && attestation.decision == *review
                                && provenance_snapshot_matches(
                                    provenance,
                                    review,
                                    &attestation.provenance_records,
                                )
                        })
                })
                .collect::<Vec<_>>();
            let [evidence] = evidence_matches.as_slice() else {
                let matching_pull_request = |evidence: &AuthenticatedPullRequestReview| {
                    evidence.repository == pull_request.repository
                        && evidence.pull_request_number == pull_request.pull_request_number
                };
                let code = if pull_request.authenticated_reviews.iter().any(|evidence| {
                    matching_pull_request(evidence)
                        && evidence.reviewed_commit_sha != pull_request.candidate_commit_sha
                        && evidence.attestations.iter().any(|attestation| {
                            attestation.attestation_id == reference.attestation_id
                                && attestation.decision == *review
                        })
                }) {
                    "legal-review.evidence.stale"
                } else if pull_request.authenticated_reviews.iter().any(|evidence| {
                    matching_pull_request(evidence)
                        && evidence.reviewed_commit_sha == pull_request.candidate_commit_sha
                        && evidence.attestations.iter().any(|attestation| {
                            attestation.attestation_id == reference.attestation_id
                                && attestation.decision == *review
                                && !provenance_snapshot_matches(
                                    provenance,
                                    review,
                                    &attestation.provenance_records,
                                )
                        })
                }) {
                    "legal-review.evidence.provenance-mismatch"
                } else if pull_request.authenticated_reviews.iter().any(|evidence| {
                    matching_pull_request(evidence)
                        && evidence.attestations.iter().any(|attestation| {
                            attestation.attestation_id == reference.attestation_id
                                || attestation.decision.id == review.id
                        })
                }) {
                    "legal-review.evidence.attestation-mismatch"
                } else {
                    "legal-review.evidence.untrusted"
                };
                violations.push(ContractViolation {
                    code,
                    message: format!(
                        "legal review {} does not reference one authenticated review",
                        review.id
                    ),
                });
                continue;
            };

            if evidence.state != AuthenticatedReviewState::Approved {
                violations.push(ContractViolation {
                    code: "legal-review.evidence.not-approved",
                    message: format!(
                        "authenticated review {} is not approved",
                        evidence.review_id
                    ),
                });
            }

            let latest_decisive_review = pull_request
                .authenticated_reviews
                .iter()
                .filter(|candidate| {
                    candidate.reviewer.github_account_id == evidence.reviewer.github_account_id
                        && candidate.reviewed_commit_sha == pull_request.candidate_commit_sha
                        && matches!(
                            candidate.state,
                            AuthenticatedReviewState::Approved
                                | AuthenticatedReviewState::ChangesRequested
                        )
                        && (candidate.submitted_at.as_str(), candidate.review_id)
                            > (evidence.submitted_at.as_str(), evidence.review_id)
                })
                .max_by_key(|candidate| (candidate.submitted_at.as_str(), candidate.review_id));
            if latest_decisive_review.is_some() {
                violations.push(ContractViolation {
                    code: "legal-review.evidence.superseded",
                    message: format!(
                        "authenticated review {} is not the reviewer's latest decisive review for the candidate commit",
                        evidence.review_id
                    ),
                });
            }

            if evidence.reviewer.github_account_id != reviewer.github_account_id {
                violations.push(ContractViolation {
                    code: "legal-review.reviewer.mismatch",
                    message: format!(
                        "legal review {} does not identify the authenticated reviewer",
                        review.id
                    ),
                });
            }

            if !authority
                .trusted_reviewer_account_ids
                .contains(&reviewer.github_account_id)
            {
                violations.push(ContractViolation {
                    code: "legal-review.reviewer.untrusted",
                    message: format!("legal review {} was made by an unknown reviewer", review.id),
                });
            }

            if pull_request.pull_request_author_account_id == reviewer.github_account_id {
                violations.push(ContractViolation {
                    code: "legal-review.reviewer.self-approval",
                    message: format!(
                        "legal review {} was self-approved by the pull-request author",
                        review.id
                    ),
                });
            }

            if evidence.submitted_at.get(..10) != Some(decided_on) {
                violations.push(ContractViolation {
                    code: "legal-review.evidence.date-mismatch",
                    message: format!(
                        "legal review {} decision date does not match authenticated evidence",
                        review.id
                    ),
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
        legal_verification: Option<LegalDecisionVerificationContext<'_>>,
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

        let mut ledger_violations =
            legal_reviews.validate_for_governed_use(self, legal_verification);
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
        legal_verification: Option<LegalDecisionVerificationContext<'_>>,
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

            violations.extend(self.validate_use(
                legal_reviews,
                legal_verification,
                &record.id,
                ProvenanceUse::Fixture,
            ));
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
        legal_verification: Option<LegalDecisionVerificationContext<'_>>,
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
            legal_verification,
            &target.provenance_id,
            ProvenanceUse::OracleOperation,
        );
        violations.extend(provenance.validate_use(
            legal_reviews,
            legal_verification,
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
        legal_verification: Option<LegalDecisionVerificationContext<'_>>,
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
                    legal_verification,
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

fn is_iso_utc_timestamp(value: &str) -> bool {
    value.len() == 20
        && value.as_bytes()[10] == b'T'
        && value.as_bytes()[13] == b':'
        && value.as_bytes()[16] == b':'
        && value.as_bytes()[19] == b'Z'
        && is_iso_date(&value[..10])
        && value.bytes().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
        })
}

fn is_git_commit_sha(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn is_github_repository(value: &str) -> bool {
    let mut components = value.split('/');
    matches!(
        (components.next(), components.next(), components.next()),
        (Some(owner), Some(repository), None)
            if is_github_name(owner) && is_github_name(repository)
    )
}

fn is_contract_identifier(value: &str) -> bool {
    value.len() <= 128
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn is_github_login(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 39
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .next_back()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !value.contains("--")
}

fn is_github_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
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

fn provenance_closure_ids<'a>(
    roots: &[String],
    records: &'a [ProvenanceRecord],
) -> Option<BTreeSet<&'a str>> {
    let mut all_ids = BTreeSet::new();
    if records
        .iter()
        .any(|record| !all_ids.insert(record.id.as_str()))
    {
        return None;
    }

    let mut closure = BTreeSet::new();
    let mut pending = roots.iter().map(String::as_str).collect::<Vec<_>>();
    while let Some(id) = pending.pop() {
        let record = records.iter().find(|record| record.id == id)?;
        if closure.insert(record.id.as_str()) {
            pending.extend(record.parent_provenance_ids.iter().map(String::as_str));
        }
    }

    for root in roots {
        let mut lineage = BTreeSet::new();
        if provenance_lineage_has_cycle(root, records, &mut lineage) {
            return None;
        }
    }

    Some(closure)
}

fn is_complete_provenance_snapshot(
    decision: &LegalReviewRecord,
    records: &[ProvenanceRecord],
) -> bool {
    provenance_closure_ids(&decision.source_provenance_ids, records)
        .is_some_and(|closure| closure.len() == records.len())
}

fn provenance_snapshot_matches(
    provenance: &ProvenanceLedger,
    decision: &LegalReviewRecord,
    snapshot: &[ProvenanceRecord],
) -> bool {
    let Some(current_ids) =
        provenance_closure_ids(&decision.source_provenance_ids, &provenance.records)
    else {
        return false;
    };
    let Some(snapshot_ids) = provenance_closure_ids(&decision.source_provenance_ids, snapshot)
    else {
        return false;
    };
    if snapshot_ids.len() != snapshot.len() || current_ids != snapshot_ids {
        return false;
    }

    current_ids.iter().all(|id| {
        let current = provenance.records.iter().find(|record| record.id == *id);
        let authenticated = snapshot.iter().find(|record| record.id == *id);
        current == authenticated
    })
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
        AuthenticatedPullRequest, AuthenticatedPullRequestReview, AuthenticatedReviewState,
        CompatibilityStatus, ConformanceRecord, FeatureCategory, FeatureMatrix, FeatureRecord,
        FixtureArtifact, LegalDecisionAttestation, LegalDecisionAuthority,
        LegalDecisionEvidenceReference, LegalDecisionVerificationContext, LegalReviewLedger,
        LegalReviewRecord, LegalReviewStatus, LegalReviewerIdentity, OracleTarget,
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
            None,
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
            None,
            "prov-public-specification",
            ProvenanceUse::ImplementationInput,
        );

        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].code, "provenance.legal-review.pending");
    }

    #[test]
    fn governed_use_rejects_in_branch_approval_without_authority() {
        let violations = provenance_ledger().validate_use(
            &approved_legal_reviews(),
            None,
            "prov-public-specification",
            ProvenanceUse::ImplementationInput,
        );

        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "legal-review.authority.required")
        );
    }

    #[test]
    fn governed_use_authenticates_the_provenance_being_used() {
        let legal_reviews = approved_legal_reviews();
        let authority = legal_decision_authority();
        let mut changed_provenance = provenance_ledger();
        changed_provenance.records[0].title = "Unattested replacement".to_owned();

        let violations = changed_provenance.validate_use(
            &legal_reviews,
            Some(legal_decision_verification(&authority)),
            "prov-public-specification",
            ProvenanceUse::ImplementationInput,
        );

        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "legal-review.evidence.provenance-mismatch")
        );
    }

    #[test]
    fn governed_use_rejects_malformed_provenance_despite_approved_status() {
        let mut provenance = provenance_ledger();
        provenance.records[0].content_digest = "sha256:not-a-digest".to_owned();
        let mut legal_reviews = pending_legal_reviews();
        let review = &mut legal_reviews.reviews[0];
        review.status = LegalReviewStatus::Approved;
        review.approved_uses = vec![ProvenanceUse::ImplementationInput];
        review.reviewed_by = Some(reviewer_identity());
        review.decided_on = Some("2026-08-02".to_owned());
        review.decision_evidence = Some(decision_evidence_reference());

        let authority = legal_decision_authority();
        let violations = provenance.validate_use(
            &legal_reviews,
            Some(legal_decision_verification(&authority)),
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
            None,
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
        let prohibited_authority = legal_decision_authority_for(&prohibited_reviews.reviews[0]);

        let prohibited = provenance.validate_use(
            &prohibited_reviews,
            Some(legal_decision_verification(&prohibited_authority)),
            "prov-public-specification",
            ProvenanceUse::ImplementationInput,
        );

        assert_eq!(prohibited.len(), 1);
        assert_eq!(prohibited[0].code, "provenance.use.prohibited");

        let mut individual_reviews = approved_legal_reviews();
        individual_reviews.reviews[0].approved_uses = vec![ProvenanceUse::DocumentationReference];
        individual_reviews.reviews[0].individual_review_uses =
            vec![ProvenanceUse::ImplementationInput];
        let individual_authority = legal_decision_authority_for(&individual_reviews.reviews[0]);

        let individual = provenance.validate_use(
            &individual_reviews,
            Some(legal_decision_verification(&individual_authority)),
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
        pending.reviews[0].reviewed_by = Some(reviewer_identity());
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
    fn legal_review_rejects_empty_ledger_and_invalid_github_login() {
        let empty = LegalReviewLedger {
            schema_version: "2.0.0".to_owned(),
            reviews: Vec::new(),
        };
        assert!(
            empty
                .validate()
                .iter()
                .any(|violation| violation.code == "legal-review.record.missing")
        );

        let mut invalid_login = approved_legal_reviews();
        let mut invalid_reviewer = reviewer_identity();
        invalid_reviewer.github_login = "invalid_login".to_owned();
        invalid_login.reviews[0].reviewed_by = Some(invalid_reviewer);
        assert!(
            invalid_login
                .validate()
                .iter()
                .any(|violation| violation.code == "legal-review.decision-metadata.missing")
        );

        let mut invalid_identifiers = pending_legal_reviews();
        invalid_identifiers.reviews[0].id = "x".repeat(129);
        invalid_identifiers.reviews[0].source_provenance_ids = vec!["invalid source".to_owned()];
        let violations = invalid_identifiers.validate();
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "legal-review.id.invalid")
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "legal-review.source.invalid")
        );
    }

    #[test]
    fn authenticated_legal_decision_requires_exact_trusted_review() {
        let legal_reviews = approved_legal_reviews();
        let provenance = provenance_ledger();
        let authority = legal_decision_authority();

        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&authority),
                )
                .is_empty()
        );

        let mut renamed_reviewer = authority.clone();
        renamed_reviewer.pull_requests[0].authenticated_reviews[0]
            .reviewer
            .github_login = "renamed-reviewer".to_owned();
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&renamed_reviewer),
                )
                .is_empty()
        );

        let mut wrong_candidate = authority.clone();
        wrong_candidate.candidate_commit_sha =
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned();
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&wrong_candidate),
                )
                .iter()
                .any(|violation| violation.code == "legal-review.authority.candidate.mismatch")
        );

        let mut stale = authority.clone();
        stale.pull_requests[0].candidate_commit_sha =
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned();
        assert!(
            legal_reviews
                .validate_authenticated_decisions(&provenance, legal_decision_verification(&stale),)
                .iter()
                .any(|violation| violation.code == "legal-review.evidence.stale")
        );

        let mut unknown_reviewer = authority.clone();
        unknown_reviewer.trusted_reviewer_account_ids.clear();
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&unknown_reviewer),
                )
                .iter()
                .any(|violation| violation.code == "legal-review.reviewer.untrusted")
        );

        let mut altered_evidence = legal_reviews.clone();
        altered_evidence.reviews[0]
            .rationale
            .push_str(" Altered after attestation.");
        assert!(
            altered_evidence
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&authority),
                )
                .iter()
                .any(|violation| {
                    violation.code == "legal-review.evidence.attestation-mismatch"
                })
        );

        let mut self_approval = authority;
        self_approval.pull_requests[0].pull_request_author_account_id = 4242;
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&self_approval),
                )
                .iter()
                .any(|violation| violation.code == "legal-review.reviewer.self-approval")
        );
    }

    #[test]
    fn authenticated_legal_decision_rejects_missing_or_malformed_evidence() {
        let legal_reviews = approved_legal_reviews();
        let provenance = provenance_ledger();

        let mut missing_review = legal_decision_authority();
        missing_review.pull_requests[0]
            .authenticated_reviews
            .clear();
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&missing_review),
                )
                .iter()
                .any(|violation| violation.code == "legal-review.evidence.untrusted")
        );

        let mut missing_reference = legal_reviews.clone();
        missing_reference.reviews[0].decision_evidence = None;
        let authority = legal_decision_authority();
        assert!(
            missing_reference
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&authority),
                )
                .iter()
                .any(|violation| violation.code == "legal-review.decision-metadata.missing")
        );

        let mut malformed = legal_decision_authority();
        malformed.pull_requests[0].authenticated_reviews[0].review_id = 0;
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&malformed),
                )
                .iter()
                .any(|violation| violation.code == "legal-review.evidence.malformed")
        );
    }

    #[test]
    fn authenticated_legal_decision_requires_exact_provenance_closure() {
        let legal_reviews = approved_legal_reviews();
        let current_provenance = provenance_ledger();

        let mut changed_snapshot = legal_decision_authority();
        changed_snapshot.pull_requests[0].authenticated_reviews[0].attestations[0]
            .provenance_records[0]
            .revision = "altered-after-review".to_owned();
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &current_provenance,
                    legal_decision_verification(&changed_snapshot),
                )
                .iter()
                .any(|violation| { violation.code == "legal-review.evidence.provenance-mismatch" })
        );

        let mut changed_current = current_provenance.clone();
        changed_current.records[0].source_url =
            Some("https://example.com/changed-specification".to_owned());
        let authority = legal_decision_authority();
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &changed_current,
                    legal_decision_verification(&authority),
                )
                .iter()
                .any(|violation| { violation.code == "legal-review.evidence.provenance-mismatch" })
        );

        let mut missing = legal_decision_authority();
        missing.pull_requests[0].authenticated_reviews[0].attestations[0]
            .provenance_records
            .clear();
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &current_provenance,
                    legal_decision_verification(&missing),
                )
                .iter()
                .any(|violation| {
                    violation.code == "legal-review.evidence.attestation.malformed"
                })
        );

        let mut extra = legal_decision_authority();
        let mut unrelated = current_provenance.records[0].clone();
        unrelated.id = "prov-unrelated".to_owned();
        extra.pull_requests[0].authenticated_reviews[0].attestations[0]
            .provenance_records
            .push(unrelated);
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &current_provenance,
                    legal_decision_verification(&extra),
                )
                .iter()
                .any(|violation| {
                    violation.code == "legal-review.evidence.attestation.malformed"
                })
        );

        let mut duplicate = legal_decision_authority();
        let repeated = duplicate.pull_requests[0].authenticated_reviews[0].attestations[0]
            .provenance_records[0]
            .clone();
        duplicate.pull_requests[0].authenticated_reviews[0].attestations[0]
            .provenance_records
            .push(repeated);
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &current_provenance,
                    legal_decision_verification(&duplicate),
                )
                .iter()
                .any(|violation| {
                    violation.code == "legal-review.evidence.attestation.malformed"
                })
        );

        let mut lineage_provenance = current_provenance.clone();
        let mut parent = lineage_provenance.records[0].clone();
        parent.id = "prov-parent".to_owned();
        parent.legal_review_id = "legal-review-public-specification".to_owned();
        lineage_provenance.records[0].parent_provenance_ids = vec![parent.id.clone()];
        lineage_provenance.records.push(parent);
        let mut lineage_reviews = legal_reviews.clone();
        lineage_reviews.reviews[0]
            .source_provenance_ids
            .push("prov-parent".to_owned());
        let mut incomplete_lineage = legal_decision_authority_for(&lineage_reviews.reviews[0]);
        incomplete_lineage.pull_requests[0].authenticated_reviews[0].attestations[0]
            .provenance_records[0]
            .parent_provenance_ids = vec!["prov-parent".to_owned()];
        assert!(
            lineage_reviews
                .validate_authenticated_decisions(
                    &lineage_provenance,
                    legal_decision_verification(&incomplete_lineage),
                )
                .iter()
                .any(|violation| {
                    violation.code == "legal-review.evidence.attestation.malformed"
                })
        );

        let mut complete_lineage = incomplete_lineage;
        complete_lineage.pull_requests[0].authenticated_reviews[0].attestations[0]
            .provenance_records
            .push(lineage_provenance.records[1].clone());
        assert!(
            lineage_reviews
                .validate_authenticated_decisions(
                    &lineage_provenance,
                    legal_decision_verification(&complete_lineage),
                )
                .is_empty()
        );
    }

    #[test]
    fn authenticated_legal_decision_rejects_review_context_mismatches() {
        let legal_reviews = approved_legal_reviews();
        let provenance = provenance_ledger();

        let mut cross_repository = legal_decision_authority();
        cross_repository.pull_requests[0].repository = "anaregdesign/review-staging".to_owned();
        cross_repository.pull_requests[0].authenticated_reviews[0].repository =
            "anaregdesign/review-staging".to_owned();
        if let Some(evidence) = cross_repository.pull_requests[0].authenticated_reviews[0]
            .attestations[0]
            .decision
            .decision_evidence
            .as_mut()
        {
            evidence.repository = "anaregdesign/review-staging".to_owned();
        }
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&cross_repository),
                )
                .iter()
                .any(|violation| {
                    violation.code == "legal-review.authority.pull-request.repository-mismatch"
                })
        );

        let mut dismissed = legal_decision_authority();
        dismissed.pull_requests[0].authenticated_reviews[0].state =
            AuthenticatedReviewState::Dismissed;
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&dismissed),
                )
                .iter()
                .any(|violation| violation.code == "legal-review.evidence.not-approved")
        );

        let mut superseded = legal_decision_authority();
        let mut changes_requested = superseded.pull_requests[0].authenticated_reviews[0].clone();
        changes_requested.review_id = 9002;
        changes_requested.state = AuthenticatedReviewState::ChangesRequested;
        changes_requested.submitted_at = "2026-08-02T12:35:56Z".to_owned();
        changes_requested.attestations.clear();
        superseded.pull_requests[0]
            .authenticated_reviews
            .push(changes_requested);
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&superseded),
                )
                .iter()
                .any(|violation| violation.code == "legal-review.evidence.superseded")
        );

        let mut superseded_by_approval = legal_decision_authority();
        let mut later_approval =
            superseded_by_approval.pull_requests[0].authenticated_reviews[0].clone();
        later_approval.review_id = 9002;
        later_approval.submitted_at = "2026-08-02T12:35:56Z".to_owned();
        later_approval.attestations.clear();
        superseded_by_approval.pull_requests[0]
            .authenticated_reviews
            .push(later_approval);
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&superseded_by_approval),
                )
                .iter()
                .any(|violation| violation.code == "legal-review.evidence.superseded")
        );

        let mut reviewer_mismatch = legal_decision_authority();
        reviewer_mismatch.pull_requests[0].authenticated_reviews[0].reviewer =
            LegalReviewerIdentity {
                github_account_id: 8484,
                github_login: "different-reviewer".to_owned(),
            };
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&reviewer_mismatch),
                )
                .iter()
                .any(|violation| violation.code == "legal-review.reviewer.mismatch")
        );

        let mut date_mismatch = legal_decision_authority();
        date_mismatch.pull_requests[0].authenticated_reviews[0].submitted_at =
            "2026-08-03T12:34:56Z".to_owned();
        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&date_mismatch),
                )
                .iter()
                .any(|violation| violation.code == "legal-review.evidence.date-mismatch")
        );

        let mut pull_request_mismatch = legal_reviews.clone();
        if let Some(reference) = pull_request_mismatch.reviews[0].decision_evidence.as_mut() {
            reference.pull_request_number = 31;
        }
        let authority = legal_decision_authority();
        assert!(
            pull_request_mismatch
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&authority),
                )
                .iter()
                .any(|violation| {
                    violation.code == "legal-review.evidence.pull-request-mismatch"
                })
        );
    }

    #[test]
    fn authenticated_legal_decision_selects_current_review_before_uniqueness() {
        let legal_reviews = approved_legal_reviews();
        let provenance = provenance_ledger();
        let mut authority = legal_decision_authority();
        let mut stale_review = authority.pull_requests[0].authenticated_reviews[0].clone();
        stale_review.review_id = 9002;
        stale_review.reviewed_commit_sha = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned();
        authority.pull_requests[0]
            .authenticated_reviews
            .push(stale_review);

        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&authority),
                )
                .is_empty()
        );
    }

    #[test]
    fn authenticated_legal_decisions_can_reference_distinct_pull_requests() {
        let mut legal_reviews = approved_legal_reviews();
        let provenance = provenance_ledger();
        let mut second_decision = legal_reviews.reviews[0].clone();
        second_decision.id = "legal-review-second-source".to_owned();
        second_decision.decision_evidence = Some(LegalDecisionEvidenceReference {
            repository: "anaregdesign/ntsql".to_owned(),
            pull_request_number: 31,
            attestation_id: "legal-review-second-source:v1".to_owned(),
        });
        legal_reviews.reviews.push(second_decision.clone());

        let mut authority = legal_decision_authority();
        let mut second_pull_request = authority.pull_requests[0].clone();
        second_pull_request.pull_request_number = 31;
        second_pull_request.pull_request_author_account_id = 8;
        second_pull_request.candidate_commit_sha =
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned();
        let second_review = &mut second_pull_request.authenticated_reviews[0];
        second_review.pull_request_number = 31;
        second_review.review_id = 9002;
        second_review.reviewed_commit_sha = second_pull_request.candidate_commit_sha.clone();
        second_review.attestations = vec![LegalDecisionAttestation {
            attestation_id: "legal-review-second-source:v1".to_owned(),
            decision: second_decision,
            provenance_records: provenance_ledger().records,
        }];
        authority.pull_requests.push(second_pull_request);

        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&authority),
                )
                .is_empty()
        );
    }

    #[test]
    fn authenticated_authority_rejects_cross_pull_request_review_id_reuse() {
        let legal_reviews = approved_legal_reviews();
        let provenance = provenance_ledger();
        let mut authority = legal_decision_authority();
        let mut duplicate_review_context = authority.pull_requests[0].clone();
        duplicate_review_context.pull_request_number = 31;
        duplicate_review_context.authenticated_reviews[0].pull_request_number = 31;
        authority.pull_requests.push(duplicate_review_context);

        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&authority),
                )
                .iter()
                .any(|violation| violation.code == "legal-review.evidence.duplicate")
        );
    }

    #[test]
    fn authenticated_authority_rejects_attestation_reuse_on_one_head() {
        let legal_reviews = approved_legal_reviews();
        let provenance = provenance_ledger();
        let mut authority = legal_decision_authority();
        let mut duplicate_attestation = authority.pull_requests[0].authenticated_reviews[0].clone();
        duplicate_attestation.review_id = 9002;
        authority.pull_requests[0]
            .authenticated_reviews
            .push(duplicate_attestation);

        assert!(
            legal_reviews
                .validate_authenticated_decisions(
                    &provenance,
                    legal_decision_verification(&authority),
                )
                .iter()
                .any(|violation| {
                    violation.code == "legal-review.evidence.attestation.duplicate"
                })
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

        let violations = provenance_ledger().validate_fixture_inventory(
            &pending_legal_reviews(),
            None,
            &fixtures,
        );

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

        let authority =
            legal_decision_authority_for_provenance(&legal_reviews.reviews[0], &provenance);
        let mismatch_violations = provenance.validate_fixture_inventory(
            &legal_reviews,
            Some(legal_decision_verification(&authority)),
            &mismatched,
        );
        let missing_violations = provenance.validate_fixture_inventory(
            &legal_reviews,
            Some(legal_decision_verification(&authority)),
            &[],
        );

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

        let authority =
            legal_decision_authority_for_provenance(&legal_reviews.reviews[0], &provenance);
        let violations = provenance.validate_fixture_inventory(
            &legal_reviews,
            Some(legal_decision_verification(&authority)),
            &fixtures,
        );

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
            None,
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
            schema_version: "2.0.0".to_owned(),
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
                decision_evidence: None,
                rationale: "Awaiting qualified human legal review".to_owned(),
            }],
        }
    }

    fn approved_legal_reviews() -> LegalReviewLedger {
        let mut legal_reviews = pending_legal_reviews();
        let review = &mut legal_reviews.reviews[0];
        review.status = LegalReviewStatus::Approved;
        review.approved_uses = vec![ProvenanceUse::ImplementationInput];
        review.reviewed_by = Some(reviewer_identity());
        review.decided_on = Some("2026-08-02".to_owned());
        review.decision_evidence = Some(decision_evidence_reference());
        legal_reviews
    }

    fn reviewer_identity() -> LegalReviewerIdentity {
        LegalReviewerIdentity {
            github_account_id: 4242,
            github_login: "qualified-reviewer".to_owned(),
        }
    }

    fn decision_evidence_reference() -> LegalDecisionEvidenceReference {
        LegalDecisionEvidenceReference {
            repository: "anaregdesign/ntsql".to_owned(),
            pull_request_number: 30,
            attestation_id: "legal-review-public-specification:v1".to_owned(),
        }
    }

    fn legal_decision_authority() -> LegalDecisionAuthority {
        let decision = approved_legal_reviews().reviews[0].clone();
        legal_decision_authority_for(&decision)
    }

    fn legal_decision_authority_for(decision: &LegalReviewRecord) -> LegalDecisionAuthority {
        legal_decision_authority_for_provenance(decision, &provenance_ledger())
    }

    fn legal_decision_authority_for_provenance(
        decision: &LegalReviewRecord,
        provenance: &ProvenanceLedger,
    ) -> LegalDecisionAuthority {
        LegalDecisionAuthority {
            schema_version: "1.0.0".to_owned(),
            candidate_repository: "anaregdesign/ntsql".to_owned(),
            candidate_commit_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            trusted_reviewer_account_ids: vec![4242],
            pull_requests: vec![AuthenticatedPullRequest {
                repository: "anaregdesign/ntsql".to_owned(),
                pull_request_number: 30,
                pull_request_author_account_id: 7,
                candidate_commit_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                authenticated_reviews: vec![AuthenticatedPullRequestReview {
                    repository: "anaregdesign/ntsql".to_owned(),
                    pull_request_number: 30,
                    review_id: 9001,
                    reviewer: reviewer_identity(),
                    reviewed_commit_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                    state: AuthenticatedReviewState::Approved,
                    submitted_at: "2026-08-02T12:34:56Z".to_owned(),
                    attestations: vec![LegalDecisionAttestation {
                        attestation_id: "legal-review-public-specification:v1".to_owned(),
                        decision: decision.clone(),
                        provenance_records: provenance.records.clone(),
                    }],
                }],
            }],
        }
    }

    fn legal_decision_verification(
        authority: &LegalDecisionAuthority,
    ) -> LegalDecisionVerificationContext<'_> {
        LegalDecisionVerificationContext {
            authority,
            candidate_repository: "anaregdesign/ntsql",
            candidate_commit_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        }
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
