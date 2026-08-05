//! Types and invariants for ntsql compatibility evidence.

use std::{collections::BTreeSet, error::Error, fmt};

use ntsql_compatibility::{CompatibilityContext, CompatibilityProfile, ObservationDimension};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// Lossless JSON value used by conformance evidence.
pub use serde_json::Value as JsonValue;

fn deserialize_required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

/// Current version shared by the target, feature, and provenance ledgers.
pub const COMPATIBILITY_SCHEMA_VERSION: &str = "1.0.0";

/// Current version of the conformance record contract.
pub const CONFORMANCE_SCHEMA_VERSION: &str = "2.0.0";

/// Current version of the legal-review ledger contract.
pub const LEGAL_REVIEW_SCHEMA_VERSION: &str = "2.0.0";

/// Current version of authenticated legal-decision authority input.
pub const LEGAL_DECISION_AUTHORITY_SCHEMA_VERSION: &str = "2.0.0";

/// Current version of the clean-room behavior-specification admission ledger.
pub const BEHAVIOR_SPECIFICATION_ADMISSION_SCHEMA_VERSION: &str = "1.0.0";

const SHA256_EMPTY_CONTENT: &str =
    "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

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

impl From<ObservationDimension> for CompatibilityDimension {
    fn from(value: ObservationDimension) -> Self {
        match value {
            ObservationDimension::Syntax => Self::Syntax,
            ObservationDimension::Wire => Self::Wire,
            ObservationDimension::Result => Self::Result,
            ObservationDimension::Metadata => Self::Metadata,
            ObservationDimension::Diagnostic => Self::Diagnostic,
            ObservationDimension::TransactionalSideEffect => Self::TransactionalSideEffect,
            ObservationDimension::Operational => Self::Operational,
        }
    }
}

impl From<CompatibilityDimension> for ObservationDimension {
    fn from(value: CompatibilityDimension) -> Self {
        match value {
            CompatibilityDimension::Syntax => Self::Syntax,
            CompatibilityDimension::Wire => Self::Wire,
            CompatibilityDimension::Result => Self::Result,
            CompatibilityDimension::Metadata => Self::Metadata,
            CompatibilityDimension::Diagnostic => Self::Diagnostic,
            CompatibilityDimension::TransactionalSideEffect => Self::TransactionalSideEffect,
            CompatibilityDimension::Operational => Self::Operational,
        }
    }
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
    /// UTC timestamp of the latest review-body edit, or null when never edited.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub last_edited_at: Option<String>,
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub source_url: Option<String>,
    /// Repository-relative path, when the record describes an owned artifact.
    #[serde(deserialize_with = "deserialize_required_nullable")]
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub reviewed_by: Option<LegalReviewerIdentity>,
    /// ISO 8601 decision date, present only after a decision.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub decided_on: Option<String>,
    /// Immutable authenticated review evidence, present only after a decision.
    #[serde(deserialize_with = "deserialize_required_nullable")]
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

/// Stable GitHub identity assigned to one clean-room role.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GovernanceActorIdentity {
    /// Stable numeric GitHub account identifier.
    pub github_account_id: u64,
    /// GitHub login recorded for audit readability.
    pub github_login: String,
}

/// One human role assigned before observation begins.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CleanRoomRoleAssignment {
    /// Human assigned to the role.
    pub actor: GovernanceActorIdentity,
    /// ISO 8601 UTC assignment timestamp.
    pub assigned_at: String,
}

/// Human separation required for one behavior case.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CleanRoomRoles {
    /// Source custodian who observes approved material or an approved oracle.
    pub observer: CleanRoomRoleAssignment,
    /// Reviewer who sanitizes the factual behavior specification.
    pub specification_reviewer: CleanRoomRoleAssignment,
    /// Engineer who receives only the approved specification.
    pub implementer: CleanRoomRoleAssignment,
    /// Reviewer who independently evaluates conformance evidence.
    pub conformance_reviewer: CleanRoomRoleAssignment,
}

/// One exact process invocation recorded without a shell.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedCommand {
    /// Executable or stable runner identifier.
    pub program: String,
    /// Exact argument vector, excluding the executable.
    pub arguments: Vec<String>,
}

/// Reproducible facts captured during the approved observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviorObservationAudit {
    /// ISO 8601 UTC time at which observation began.
    pub started_at: String,
    /// ISO 8601 UTC time at which observation ended.
    pub completed_at: String,
    /// Exact argv-form commands used for the observation.
    pub commands: Vec<RecordedCommand>,
    /// Exact session settings applied before the case.
    pub session_settings: Vec<ConformanceEnvironmentFact>,
    /// Bounded environment facts required to reproduce the observation.
    pub environment: Vec<ConformanceEnvironmentFact>,
    /// SHA-256 digest of the exact input bytes.
    pub input_digest: String,
    /// Exact input byte length.
    pub input_byte_length: u64,
}

/// Final disposition of raw evidence outside the repository.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RawEvidenceDisposition {
    /// Evidence remains in an access-controlled external store.
    Protected {
        /// Stable evidence-store boundary.
        store_id: String,
        /// Stable artifact identifier within the store.
        artifact_id: String,
        /// ISO 8601 UTC time at which protected retention was confirmed.
        confirmed_at: String,
    },
    /// All retained raw-evidence copies were deleted.
    Deleted {
        /// ISO 8601 UTC time at which deletion was confirmed.
        deleted_at: String,
    },
}

/// Auditable cleanup action applied to raw evidence or observer access.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CleanupEventKind {
    /// A working copy of raw evidence was deleted.
    WorkingCopyDeleted,
    /// Observer access to retained evidence was revoked.
    AccessRevoked,
    /// Protected retention was confirmed at the named store boundary.
    ProtectedRetentionConfirmed,
}

/// One timestamped cleanup action.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CleanupEvent {
    /// Cleanup action that occurred.
    pub kind: CleanupEventKind,
    /// ISO 8601 UTC event timestamp.
    pub occurred_at: String,
    /// Human who performed or verified the action.
    pub actor: GovernanceActorIdentity,
}

/// Digest and cleanup metadata for raw evidence kept outside the repository.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawEvidenceAudit {
    /// SHA-256 digest of the exact raw-evidence bytes.
    pub content_digest: String,
    /// Exact raw-evidence byte length.
    pub byte_length: u64,
    /// Final retained-or-deleted state.
    pub disposition: RawEvidenceDisposition,
    /// Timestamped cleanup actions, without sensitive paths or bytes.
    pub cleanup_events: Vec<CleanupEvent>,
}

/// Technical disposition of a sanitized behavior specification.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpecificationReviewStatus {
    /// No specification reviewer has made a decision.
    Pending,
    /// The named reviewer accepted the sanitized factual specification.
    Approved,
    /// The named reviewer rejected the specification.
    Rejected,
}

/// Candidate-recorded review reference requiring out-of-branch authentication.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpecificationReviewEvidenceReference {
    /// Repository in `owner/name` form.
    pub repository: String,
    /// Pull request containing the reviewed specification and admission.
    pub pull_request_number: u64,
    /// Identifier repeated in authenticated review evidence.
    pub attestation_id: String,
}

/// Human technical review of one sanitized behavior specification.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SpecificationTechnicalReview {
    /// Current technical decision.
    pub status: SpecificationReviewStatus,
    /// Reviewer identity, present only after a decision.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub reviewed_by: Option<GovernanceActorIdentity>,
    /// ISO 8601 UTC decision timestamp, present only after a decision.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub decided_at: Option<String>,
    /// Immutable review reference, present only after a decision.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub decision_evidence: Option<SpecificationReviewEvidenceReference>,
    /// Non-sensitive explanation of the current review state.
    pub rationale: String,
}

/// Sanitized behavior specification and its exact provenance identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviorSpecificationReference {
    /// Behavior-specification provenance record.
    pub provenance_id: String,
    /// Repository-relative specification path.
    pub artifact_path: String,
    /// SHA-256 digest of the specification bytes.
    pub content_digest: String,
    /// Exact direct provenance parents used to derive the specification.
    pub parent_provenance_ids: Vec<String>,
    /// Technical review of the sanitized specification.
    pub technical_review: SpecificationTechnicalReview,
}

/// Controlled handoff of one approved specification to its implementer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImplementationHandoff {
    /// Specification supplied to the implementer.
    pub specification_provenance_id: String,
    /// Digest supplied to the implementer.
    pub specification_digest: String,
    /// Implementer who received the specification.
    pub implementer: GovernanceActorIdentity,
    /// ISO 8601 UTC handoff timestamp.
    pub handed_off_at: String,
}

/// Repository test derived from one sanitized behavior specification.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DerivedTestReference {
    /// Test provenance record.
    pub provenance_id: String,
    /// Repository-relative test path.
    pub artifact_path: String,
    /// SHA-256 digest of the test bytes.
    pub content_digest: String,
    /// Exact direct provenance parents used to derive the test.
    pub parent_provenance_ids: Vec<String>,
}

/// Audit record required before a behavior specification may guide implementation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviorSpecificationAdmissionRecord {
    /// Stable admission identifier.
    pub id: String,
    /// Stable behavior-case identifier.
    pub case_id: String,
    /// GitHub issue that owns the case.
    pub owner_issue: u64,
    /// Exact feature inventory entries covered by the specification.
    pub feature_ids: Vec<String>,
    /// Exact oracle target observed for this case.
    pub target_id: String,
    /// Human clean-room role assignments.
    pub roles: CleanRoomRoles,
    /// Direct source records used by the observer and specification reviewer.
    pub source_provenance_ids: Vec<String>,
    /// Exact legal-review records reached by every admission artifact.
    pub legal_review_ids: Vec<String>,
    /// Reproducible observation audit.
    pub observation: BehaviorObservationAudit,
    /// Raw-evidence digest and cleanup audit.
    pub raw_evidence: RawEvidenceAudit,
    /// Sanitized behavior specification.
    pub specification: BehaviorSpecificationReference,
    /// Approved handoff, or `None` before technical approval.
    #[serde(deserialize_with = "deserialize_required_nullable")]
    pub implementation_handoff: Option<ImplementationHandoff>,
    /// Repository tests derived from the sanitized specification.
    pub derived_tests: Vec<DerivedTestReference>,
}

/// Published inventory of clean-room behavior-specification admissions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BehaviorSpecificationAdmissionLedger {
    /// Contract version used to interpret this ledger.
    pub schema_version: String,
    /// Behavior cases admitted or awaiting admission.
    pub admissions: Vec<BehaviorSpecificationAdmissionRecord>,
}

/// Exact ledgers and authority used to decide one implementation admission.
#[derive(Clone, Copy)]
pub struct ImplementationAdmissionContext<'a> {
    /// Exact target inventory.
    pub targets: &'a TargetMatrix,
    /// Clean-room behavior-specification admissions.
    pub admissions: &'a BehaviorSpecificationAdmissionLedger,
    /// Complete provenance inventory.
    pub provenance: &'a ProvenanceLedger,
    /// Human legal-review ledger.
    pub legal_reviews: &'a LegalReviewLedger,
    /// Authenticated legal authority, when non-pending decisions exist.
    pub legal_verification: Option<LegalDecisionVerificationContext<'a>>,
}

/// Whether externally stored raw evidence may be redistributed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceAccess {
    /// The referenced artifact may be included in public evidence.
    Public,
    /// The referenced artifact remains in an access-controlled evidence store.
    Protected,
}

/// Raw evidence retained for one side of a comparison.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum RawEvidence {
    /// A synthetic or redistributable structured value stored directly.
    Inline {
        /// Unnormalized observation value.
        value: Value,
    },
    /// Immutable metadata for bytes retained outside the conformance record.
    Artifact {
        /// Stable identifier of the evidence store boundary.
        store_id: String,
        /// Stable identifier of the artifact within that store.
        artifact_id: String,
        /// SHA-256 digest of the retained bytes.
        content_digest: String,
        /// Exact number of retained bytes.
        byte_length: u64,
        /// Media type used to interpret the retained bytes.
        media_type: String,
        /// Redistribution boundary for the retained bytes.
        access: EvidenceAccess,
    },
}

/// Raw oracle and subject observations before normalization.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RawObservationPair {
    /// Raw oracle evidence.
    pub oracle: RawEvidence,
    /// Raw ntsql evidence.
    pub subject: RawEvidence,
}

/// Typed values compared after applying named normalization rules.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedObservationPair {
    /// Normalized oracle value.
    pub oracle: Value,
    /// Normalized ntsql value.
    pub subject: Value,
}

/// One immutable normalization rule definition captured with a record.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizationRule {
    /// Stable rule identifier.
    pub id: String,
    /// Positive rule revision.
    pub revision: u32,
    /// Provenance record authorizing the rule.
    pub provenance_id: String,
    /// Nonempty description of the value transformation.
    pub description: String,
}

/// Exact normalization rule revision applied to one dimension.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizationRuleReference {
    /// Stable rule identifier.
    pub id: String,
    /// Exact rule revision.
    pub revision: u32,
}

/// One exact fact needed to reproduce the subject environment.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConformanceEnvironmentFact {
    /// Stable environment field name.
    pub name: String,
    /// Exact observed value.
    pub value: String,
}

/// Machine-actionable identity of the runner, input, and subject build.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConformanceReproduction {
    /// Stable runner implementation identifier.
    pub runner_id: String,
    /// Exact Git revision of the runner implementation.
    pub runner_revision: String,
    /// SHA-256 digest of the runner artifact that executed the case.
    pub runner_digest: String,
    /// Exact Git revision of the ntsql subject.
    pub subject_revision: String,
    /// SHA-256 digest of the ntsql subject artifact under test.
    pub subject_digest: String,
    /// Deterministic seed used to construct the case input.
    pub case_seed: String,
    /// SHA-256 digest of the exact input bytes.
    pub input_digest: String,
    /// Complete bounded environment facts required by the runner.
    pub environment: Vec<ConformanceEnvironmentFact>,
    /// Argument vector passed to the identified runner, without a shell.
    pub arguments: Vec<String>,
}

/// A mandatory dimension observation, including an explicit reason when absent.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum DimensionObservation {
    /// The oracle and subject observations were captured.
    Observed {
        /// Unmodified evidence or immutable references to retained bytes.
        raw: Box<RawObservationPair>,
        /// Typed values after applying the named rules.
        normalized: NormalizedObservationPair,
        /// Exact rule revisions applied in order; empty means no normalization.
        normalization_rules: Vec<NormalizationRuleReference>,
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
    /// Feature inventory entry exercised by this case.
    pub feature_id: String,
    /// Issue that owns the referenced feature at capture time.
    pub owner_issue: u64,
    /// Identifier of the exact oracle target from the target matrix.
    pub target_id: String,
    /// ISO 8601 UTC capture timestamp.
    pub observed_at: String,
    /// Provenance record that authorizes the input and oracle observation.
    pub provenance_id: String,
    /// Behavior authority classification.
    pub behavior_class: BehaviorClass,
    /// Deterministic runner, subject, input, and environment identity.
    pub reproduction: ConformanceReproduction,
    /// Complete rule definitions referenced by observed dimensions.
    pub normalization_rules: Vec<NormalizationRule>,
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
    /// Validates constraints expressed by the published target-matrix schema.
    #[must_use]
    pub fn validate_schema_semantics(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if self.schema_version != COMPATIBILITY_SCHEMA_VERSION {
            violations.push(ContractViolation {
                code: "target.schema-version.unsupported",
                message: format!("unsupported target schema version: {}", self.schema_version),
            });
        }
        if !is_contract_identifier(&self.baseline_target_id) {
            violations.push(ContractViolation {
                code: "target.baseline.invalid",
                message: "baseline target id is malformed".to_owned(),
            });
        }
        if self.targets.is_empty() {
            violations.push(ContractViolation {
                code: "target.missing",
                message: "target matrix requires at least one target".to_owned(),
            });
        }

        for target in &self.targets {
            if !is_contract_identifier(&target.id) || !is_contract_identifier(&target.provenance_id)
            {
                violations.push(ContractViolation {
                    code: "target.id.invalid",
                    message: format!("target {} contains a malformed identifier", target.id),
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
            if !has_four_numeric_components(&target.product_version) {
                violations.push(ContractViolation {
                    code: "target.product-version.invalid",
                    message: format!("target {} has a malformed product version", target.id),
                });
            }
            if [
                &target.product_release,
                &target.servicing_update,
                &target.edition,
                &target.operating_system,
                &target.architecture,
                &target.container_repository,
                &target.container_tag,
                &target.collation,
                &target.language,
                &target.timezone,
            ]
            .into_iter()
            .any(|value| value.is_empty())
            {
                violations.push(ContractViolation {
                    code: "target.metadata.empty",
                    message: format!("target {} contains empty metadata", target.id),
                });
            }
            if target.session_settings.is_empty()
                || target.session_settings.iter().any(String::is_empty)
            {
                violations.push(ContractViolation {
                    code: "target.session-settings.empty",
                    message: format!("target {} requires explicit session settings", target.id),
                });
            }
            if has_duplicates(&target.session_settings) {
                violations.push(ContractViolation {
                    code: "target.session-settings.duplicate",
                    message: format!("target {} repeats a session setting", target.id),
                });
            }
        }

        for expansion in &self.expansion_order {
            if expansion.sequence == 0 {
                violations.push(ContractViolation {
                    code: "target.expansion.sequence.invalid",
                    message: "target expansion sequence must be positive".to_owned(),
                });
            }
            if expansion.scope.is_empty() || expansion.admission_criteria.is_empty() {
                violations.push(ContractViolation {
                    code: "target.expansion.metadata.empty",
                    message: "target expansion requires scope and admission criteria".to_owned(),
                });
            }
        }

        violations
    }

    /// Validates target uniqueness, reproducibility, and baseline selection.
    #[must_use]
    pub fn validate(&self) -> Vec<ContractViolation> {
        let mut violations = self.validate_schema_semantics();
        let mut target_ids = BTreeSet::new();

        for target in &self.targets {
            if !target_ids.insert(target.id.as_str()) {
                violations.push(ContractViolation {
                    code: "target.id.duplicate",
                    message: format!("duplicate target id: {}", target.id),
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

/// An owned target matrix that has passed every first-party target invariant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedTargetMatrix {
    matrix: TargetMatrix,
    contexts: Vec<CompatibilityContext>,
    baseline_index: usize,
}

impl ValidatedTargetMatrix {
    /// Returns the validated public target matrix without permitting mutation.
    #[must_use]
    pub fn matrix(&self) -> &TargetMatrix {
        &self.matrix
    }

    /// Returns the immutable context for the validated baseline target.
    #[must_use]
    pub fn baseline_context(&self) -> &CompatibilityContext {
        &self.contexts[self.baseline_index]
    }

    /// Selects an immutable context by exact target identifier.
    pub fn select_context(
        &self,
        target_id: &str,
    ) -> Result<&CompatibilityContext, TargetSelectionError> {
        self.contexts
            .iter()
            .find(|context| context.target_id().as_str() == target_id)
            .ok_or_else(|| TargetSelectionError {
                target_id: target_id.to_owned(),
            })
    }
}

impl TryFrom<TargetMatrix> for ValidatedTargetMatrix {
    type Error = TargetMatrixValidationError;

    fn try_from(matrix: TargetMatrix) -> Result<Self, Self::Error> {
        let violations = matrix.validate();
        if !violations.is_empty() {
            return Err(TargetMatrixValidationError { violations });
        }

        let Some(baseline_index) = matrix
            .targets
            .iter()
            .position(|target| target.id == matrix.baseline_target_id)
        else {
            return Err(TargetMatrixValidationError {
                violations: vec![ContractViolation {
                    code: "target.baseline.unknown",
                    message: format!(
                        "baseline target is not present: {}",
                        matrix.baseline_target_id
                    ),
                }],
            });
        };

        let mut contexts = Vec::with_capacity(matrix.targets.len());
        for target in &matrix.targets {
            let profile = CompatibilityProfile {
                target_id: target.id.clone(),
                product_release: target.product_release.clone(),
                servicing_update: target.servicing_update.clone(),
                product_version: target.product_version.clone(),
                edition: target.edition.clone(),
                operating_system: target.operating_system.clone(),
                architecture: target.architecture.clone(),
                compatibility_level: target.compatibility_level,
                collation: target.collation.clone(),
                language: target.language.clone(),
                lcid: target.lcid,
                timezone: target.timezone.clone(),
                session_defaults: target.session_settings.clone(),
            };
            match CompatibilityContext::try_new(profile) {
                Ok(context) => contexts.push(context),
                Err(error) => {
                    return Err(TargetMatrixValidationError {
                        violations: vec![ContractViolation {
                            code: "target.context.invalid",
                            message: format!(
                                "target {} cannot form a compatibility context: {error}",
                                target.id
                            ),
                        }],
                    });
                }
            }
        }

        Ok(Self {
            matrix,
            contexts,
            baseline_index,
        })
    }
}

/// Failure to promote a raw target matrix into a validated typestate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetMatrixValidationError {
    violations: Vec<ContractViolation>,
}

impl TargetMatrixValidationError {
    /// Returns every target invariant that prevented promotion.
    #[must_use]
    pub fn violations(&self) -> &[ContractViolation] {
        &self.violations
    }
}

impl fmt::Display for TargetMatrixValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "target matrix validation failed with {} violation(s)",
            self.violations.len()
        )
    }
}

impl Error for TargetMatrixValidationError {}

/// Failure to select an exact target from a validated matrix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetSelectionError {
    target_id: String,
}

impl TargetSelectionError {
    /// Returns the unavailable target identifier.
    #[must_use]
    pub fn target_id(&self) -> &str {
        &self.target_id
    }
}

impl fmt::Display for TargetSelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "target is not present in the validated matrix: {}",
            self.target_id
        )
    }
}

impl Error for TargetSelectionError {}

fn is_sha256_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };

    hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn is_canonical_sha256_digest(value: &str) -> bool {
    let Some(hex) = value.strip_prefix("sha256:") else {
        return false;
    };

    hex.len() == 64
        && hex
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn has_four_numeric_components(value: &str) -> bool {
    let components = value.split('.').collect::<Vec<_>>();
    components.len() == 4
        && components.iter().all(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        })
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
    #[serde(deserialize_with = "deserialize_required_nullable")]
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

impl GovernanceActorIdentity {
    fn is_well_formed(&self) -> bool {
        self.github_account_id > 0 && is_github_login(&self.github_login)
    }
}

impl SpecificationReviewEvidenceReference {
    fn is_well_formed(&self) -> bool {
        is_github_repository(&self.repository)
            && self.pull_request_number > 0
            && is_contract_identifier(&self.attestation_id)
    }
}

impl LegalDecisionAuthority {
    /// Validates constraints expressed by the published legal-decision authority schema.
    #[must_use]
    pub fn validate_schema_semantics(&self) -> Vec<ContractViolation> {
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

        if self.pull_requests.is_empty() {
            violations.push(ContractViolation {
                code: "legal-review.authority.pull-request.missing",
                message: "legal-review authority requires a pull-request context".to_owned(),
            });
        }
        for pull_request in &self.pull_requests {
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

            for review in &pull_request.authenticated_reviews {
                if !is_github_repository(&review.repository)
                    || review.pull_request_number == 0
                    || review.review_id == 0
                    || !review.reviewer.is_well_formed()
                    || !is_git_commit_sha(&review.reviewed_commit_sha)
                    || !is_iso_utc_timestamp(&review.submitted_at)
                    || review
                        .last_edited_at
                        .as_deref()
                        .is_some_and(|timestamp| !is_iso_utc_timestamp(timestamp))
                {
                    violations.push(ContractViolation {
                        code: "legal-review.evidence.malformed",
                        message: format!(
                            "authenticated review {} contains malformed evidence",
                            review.review_id
                        ),
                    });
                }
                if has_equal_duplicates(&review.attestations) {
                    violations.push(ContractViolation {
                        code: "legal-review.evidence.attestation.malformed",
                        message: format!(
                            "authenticated review {} repeats an attestation item",
                            review.review_id
                        ),
                    });
                }

                for attestation in &review.attestations {
                    if !is_contract_identifier(&attestation.attestation_id)
                        || !attestation.decision.validate_schema_semantics().is_empty()
                        || attestation.provenance_records.is_empty()
                        || has_equal_duplicates(&attestation.provenance_records)
                        || attestation
                            .provenance_records
                            .iter()
                            .any(|record| !record.validate_schema_semantics().is_empty())
                    {
                        violations.push(ContractViolation {
                            code: "legal-review.evidence.attestation.malformed",
                            message: format!(
                                "authenticated review {} contains a malformed attestation",
                                review.review_id
                            ),
                        });
                    }
                }
            }
        }

        violations
    }

    /// Validates cross-record constraints within one authority document.
    #[must_use]
    pub fn validate_document_semantics(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let mut pull_requests = BTreeSet::new();
        let mut review_ids = BTreeSet::new();
        let mut attestation_keys = BTreeSet::new();

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
                    if attestation.decision.status == LegalReviewStatus::Pending
                        || !attestation.decision.validate_cross_scope().is_empty()
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

    /// Validates the authority audience against a trusted workflow event.
    #[must_use]
    pub fn validate_trusted_candidate(
        &self,
        candidate_repository: &str,
        candidate_commit_sha: &str,
    ) -> Vec<ContractViolation> {
        if self.candidate_repository == candidate_repository
            && self.candidate_commit_sha == candidate_commit_sha
        {
            Vec::new()
        } else {
            vec![ContractViolation {
                code: "legal-review.authority.candidate.mismatch",
                message: "legal-review authority does not match the trusted candidate target"
                    .to_owned(),
            }]
        }
    }

    fn validate(
        &self,
        candidate_repository: &str,
        candidate_commit_sha: &str,
    ) -> Vec<ContractViolation> {
        let mut violations = self.validate_schema_semantics();
        violations.extend(self.validate_document_semantics());
        violations
            .extend(self.validate_trusted_candidate(candidate_repository, candidate_commit_sha));
        violations
    }
}

fn authenticated_review_effective_at(review: &AuthenticatedPullRequestReview) -> &str {
    review
        .last_edited_at
        .as_deref()
        .filter(|last_edited_at| *last_edited_at > review.submitted_at.as_str())
        .unwrap_or(&review.submitted_at)
}

impl LegalReviewRecord {
    /// Validates constraints expressed by the published legal-review schema.
    #[must_use]
    pub fn validate_schema_semantics(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if !is_contract_identifier(&self.id) {
            violations.push(ContractViolation {
                code: "legal-review.id.invalid",
                message: format!("legal review id is malformed: {}", self.id),
            });
        }

        if !has_schema_non_whitespace(&self.subject) || !has_schema_non_whitespace(&self.rationale)
        {
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

        for (field, uses) in [
            ("approved_uses", &self.approved_uses),
            ("prohibited_uses", &self.prohibited_uses),
            ("individual_review_uses", &self.individual_review_uses),
        ] {
            if has_duplicates(uses) {
                violations.push(ContractViolation {
                    code: "legal-review.use.duplicate",
                    message: format!(
                        "legal review {} contains a duplicate use in {field}",
                        self.id,
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
                if !self.approved_uses.is_empty()
                    || !self.prohibited_uses.is_empty()
                    || !self.individual_review_uses.is_empty()
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

    fn validate_cross_scope(&self) -> Vec<ContractViolation> {
        let has_conflict = self.approved_uses.iter().any(|use_kind| {
            self.prohibited_uses.contains(use_kind)
                || self.individual_review_uses.contains(use_kind)
        }) || self
            .prohibited_uses
            .iter()
            .any(|use_kind| self.individual_review_uses.contains(use_kind));

        if has_conflict {
            vec![ContractViolation {
                code: "legal-review.use.duplicate",
                message: format!(
                    "legal review {} assigns one use to conflicting scopes",
                    self.id
                ),
            }]
        } else {
            Vec::new()
        }
    }
}

impl LegalReviewLedger {
    /// Validates constraints expressed by the published legal-review ledger schema.
    #[must_use]
    pub fn validate_schema_semantics(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

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
            violations.extend(review.validate_schema_semantics());
        }

        violations
    }

    /// Validates legal-review structure without treating pending work as approval.
    #[must_use]
    pub fn validate(&self) -> Vec<ContractViolation> {
        let mut violations = self.validate_schema_semantics();
        let mut review_ids = BTreeSet::new();

        for review in &self.reviews {
            violations.extend(review.validate_cross_scope());
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

            if authenticated_review_effective_at(evidence).get(..10) != Some(decided_on) {
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

impl BehaviorSpecificationAdmissionRecord {
    /// Validates constraints expressed by the published admission schema.
    #[must_use]
    pub fn validate_schema_semantics(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if !is_contract_identifier(&self.id)
            || !is_contract_identifier(&self.case_id)
            || !is_contract_identifier(&self.target_id)
        {
            violations.push(ContractViolation {
                code: "behavior-admission.identifier.invalid",
                message: format!("admission {} contains a malformed identifier", self.id),
            });
        }
        if self.owner_issue == 0 {
            violations.push(ContractViolation {
                code: "behavior-admission.owner-issue.invalid",
                message: format!("admission {} requires a positive owner issue", self.id),
            });
        }

        for (field, identifiers) in [
            ("feature_ids", &self.feature_ids),
            ("source_provenance_ids", &self.source_provenance_ids),
            ("legal_review_ids", &self.legal_review_ids),
        ] {
            if identifiers.is_empty()
                || identifiers
                    .iter()
                    .any(|identifier| !is_contract_identifier(identifier))
            {
                violations.push(ContractViolation {
                    code: "behavior-admission.reference.invalid",
                    message: format!("admission {} has invalid {field}", self.id),
                });
            }
            if has_duplicates(identifiers) {
                violations.push(ContractViolation {
                    code: "behavior-admission.reference.duplicate",
                    message: format!("admission {} repeats a {field} entry", self.id),
                });
            }
        }

        for assignment in admission_role_assignments(&self.roles) {
            if !assignment.actor.is_well_formed() || !is_iso_utc_timestamp(&assignment.assigned_at)
            {
                violations.push(ContractViolation {
                    code: "behavior-admission.role.invalid",
                    message: format!("admission {} has malformed role metadata", self.id),
                });
            }
        }

        if !is_iso_utc_timestamp(&self.observation.started_at)
            || !is_iso_utc_timestamp(&self.observation.completed_at)
            || !is_canonical_sha256_digest(&self.observation.input_digest)
            || !digest_length_is_consistent(
                &self.observation.input_digest,
                self.observation.input_byte_length,
            )
        {
            violations.push(ContractViolation {
                code: "behavior-admission.observation.invalid",
                message: format!("admission {} has malformed observation metadata", self.id),
            });
        }
        if self.observation.commands.is_empty()
            || has_equal_duplicates(&self.observation.commands)
            || self.observation.commands.iter().any(|command| {
                !has_schema_non_whitespace(&command.program)
                    || command.program.contains('\0')
                    || command.arguments.iter().any(|argument| {
                        !has_schema_non_whitespace(argument) || argument.contains('\0')
                    })
            })
        {
            violations.push(ContractViolation {
                code: "behavior-admission.command.invalid",
                message: format!("admission {} requires unique argv-form commands", self.id),
            });
        }
        for (field, facts) in [
            ("session_settings", &self.observation.session_settings),
            ("environment", &self.observation.environment),
        ] {
            if facts.is_empty()
                || has_duplicates(facts)
                || facts.iter().any(|fact| {
                    !is_contract_identifier(&fact.name) || !has_schema_non_whitespace(&fact.value)
                })
            {
                violations.push(ContractViolation {
                    code: "behavior-admission.environment.invalid",
                    message: format!("admission {} has invalid {field}", self.id),
                });
            }
        }

        if !is_canonical_sha256_digest(&self.raw_evidence.content_digest)
            || !digest_length_is_consistent(
                &self.raw_evidence.content_digest,
                self.raw_evidence.byte_length,
            )
            || self.raw_evidence.cleanup_events.is_empty()
            || has_equal_duplicates(&self.raw_evidence.cleanup_events)
        {
            violations.push(ContractViolation {
                code: "behavior-admission.raw-evidence.invalid",
                message: format!("admission {} has malformed raw-evidence metadata", self.id),
            });
        }
        match &self.raw_evidence.disposition {
            RawEvidenceDisposition::Protected {
                store_id,
                artifact_id,
                confirmed_at,
            } => {
                if !is_contract_identifier(store_id)
                    || !is_contract_identifier(artifact_id)
                    || !is_iso_utc_timestamp(confirmed_at)
                {
                    violations.push(ContractViolation {
                        code: "behavior-admission.disposition.invalid",
                        message: format!(
                            "admission {} has invalid protected evidence disposition",
                            self.id
                        ),
                    });
                }
            }
            RawEvidenceDisposition::Deleted { deleted_at } => {
                if !is_iso_utc_timestamp(deleted_at) {
                    violations.push(ContractViolation {
                        code: "behavior-admission.disposition.invalid",
                        message: format!(
                            "admission {} has invalid deleted evidence disposition",
                            self.id
                        ),
                    });
                }
            }
        }
        for event in &self.raw_evidence.cleanup_events {
            if !is_iso_utc_timestamp(&event.occurred_at) || !event.actor.is_well_formed() {
                violations.push(ContractViolation {
                    code: "behavior-admission.cleanup-event.invalid",
                    message: format!("admission {} has malformed cleanup metadata", self.id),
                });
            }
        }

        let specification = &self.specification;
        if !is_contract_identifier(&specification.provenance_id)
            || !is_repository_relative_path(&specification.artifact_path)
            || !is_canonical_sha256_digest(&specification.content_digest)
            || specification.parent_provenance_ids.is_empty()
            || specification
                .parent_provenance_ids
                .iter()
                .any(|identifier| !is_contract_identifier(identifier))
            || has_duplicates(&specification.parent_provenance_ids)
        {
            violations.push(ContractViolation {
                code: "behavior-admission.specification.invalid",
                message: format!("admission {} has malformed specification metadata", self.id),
            });
        }

        let review = &specification.technical_review;
        let has_valid_decision_metadata = review
            .reviewed_by
            .as_ref()
            .is_some_and(GovernanceActorIdentity::is_well_formed)
            && review
                .decided_at
                .as_deref()
                .is_some_and(is_iso_utc_timestamp)
            && review
                .decision_evidence
                .as_ref()
                .is_some_and(SpecificationReviewEvidenceReference::is_well_formed);
        if !has_schema_non_whitespace(&review.rationale) {
            violations.push(ContractViolation {
                code: "behavior-admission.review.rationale-empty",
                message: format!("admission {} requires a review rationale", self.id),
            });
        }
        match review.status {
            SpecificationReviewStatus::Pending => {
                if review.reviewed_by.is_some()
                    || review.decided_at.is_some()
                    || review.decision_evidence.is_some()
                {
                    violations.push(ContractViolation {
                        code: "behavior-admission.review.pending-has-decision",
                        message: format!(
                            "pending admission {} cannot contain review decision metadata",
                            self.id
                        ),
                    });
                }
            }
            SpecificationReviewStatus::Approved | SpecificationReviewStatus::Rejected => {
                if !has_valid_decision_metadata {
                    violations.push(ContractViolation {
                        code: "behavior-admission.review.decision-metadata-missing",
                        message: format!(
                            "decided admission {} requires reviewer, time, and evidence",
                            self.id
                        ),
                    });
                }
            }
        }

        if let Some(handoff) = &self.implementation_handoff
            && (!is_contract_identifier(&handoff.specification_provenance_id)
                || !is_canonical_sha256_digest(&handoff.specification_digest)
                || !handoff.implementer.is_well_formed()
                || !is_iso_utc_timestamp(&handoff.handed_off_at))
        {
            violations.push(ContractViolation {
                code: "behavior-admission.handoff.invalid",
                message: format!("admission {} has malformed handoff metadata", self.id),
            });
        }

        if has_equal_duplicates(&self.derived_tests) {
            violations.push(ContractViolation {
                code: "behavior-admission.derived-test.duplicate",
                message: format!("admission {} repeats a derived test", self.id),
            });
        }
        for derived_test in &self.derived_tests {
            if !is_contract_identifier(&derived_test.provenance_id)
                || !is_repository_relative_path(&derived_test.artifact_path)
                || !is_canonical_sha256_digest(&derived_test.content_digest)
                || derived_test.parent_provenance_ids.is_empty()
                || derived_test
                    .parent_provenance_ids
                    .iter()
                    .any(|identifier| !is_contract_identifier(identifier))
                || has_duplicates(&derived_test.parent_provenance_ids)
            {
                violations.push(ContractViolation {
                    code: "behavior-admission.derived-test.invalid",
                    message: format!("admission {} has malformed derived-test metadata", self.id),
                });
            }
        }

        violations
    }

    /// Validates clean-room role, chronology, disposition, and handoff invariants.
    #[must_use]
    pub fn validate_document_semantics(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let assignments = admission_role_assignments(&self.roles);
        let mut actor_ids = BTreeSet::new();
        let mut timestamps = assignments
            .iter()
            .map(|assignment| assignment.assigned_at.as_str())
            .chain([
                self.observation.started_at.as_str(),
                self.observation.completed_at.as_str(),
            ])
            .chain(
                self.raw_evidence
                    .cleanup_events
                    .iter()
                    .map(|event| event.occurred_at.as_str()),
            )
            .collect::<Vec<_>>();
        match &self.raw_evidence.disposition {
            RawEvidenceDisposition::Protected { confirmed_at, .. } => {
                timestamps.push(confirmed_at);
            }
            RawEvidenceDisposition::Deleted { deleted_at } => timestamps.push(deleted_at),
        }
        if let Some(decided_at) = self.specification.technical_review.decided_at.as_deref() {
            timestamps.push(decided_at);
        }
        if let Some(handoff) = &self.implementation_handoff {
            timestamps.push(&handoff.handed_off_at);
        }
        if timestamps
            .iter()
            .any(|timestamp| !is_valid_iso_utc_timestamp(timestamp))
        {
            violations.push(ContractViolation {
                code: "behavior-admission.timestamp.out-of-range",
                message: format!("admission {} contains an invalid UTC timestamp", self.id),
            });
        }

        if assignments
            .iter()
            .any(|assignment| !actor_ids.insert(assignment.actor.github_account_id))
        {
            violations.push(ContractViolation {
                code: "behavior-admission.role.not-separated",
                message: format!(
                    "admission {} requires four distinct clean-room actors",
                    self.id
                ),
            });
        }
        if assignments
            .iter()
            .any(|assignment| assignment.assigned_at >= self.observation.started_at)
        {
            violations.push(ContractViolation {
                code: "behavior-admission.role.assigned-late",
                message: format!(
                    "admission {} assigned a role after observation began",
                    self.id
                ),
            });
        }
        if self.observation.completed_at < self.observation.started_at {
            violations.push(ContractViolation {
                code: "behavior-admission.observation.time-order",
                message: format!("admission {} observation ends before it starts", self.id),
            });
        }
        for (field, facts) in [
            ("session settings", &self.observation.session_settings),
            ("environment", &self.observation.environment),
        ] {
            let mut names = BTreeSet::new();
            if facts.iter().any(|fact| !names.insert(fact.name.as_str())) {
                violations.push(ContractViolation {
                    code: "behavior-admission.environment.name-duplicate",
                    message: format!("admission {} repeats a {field} name", self.id),
                });
            }
        }

        let raw_evidence_actors = [
            &self.roles.observer.actor,
            &self.roles.specification_reviewer.actor,
        ];
        if self.raw_evidence.cleanup_events.iter().any(|event| {
            event.occurred_at <= self.observation.completed_at
                || !raw_evidence_actors.contains(&&event.actor)
        }) {
            violations.push(ContractViolation {
                code: "behavior-admission.cleanup-event.invalid-actor-or-time",
                message: format!(
                    "admission {} cleanup must follow observation and use a raw-evidence role",
                    self.id
                ),
            });
        }

        let (disposition_time, required_cleanup_kind) = match &self.raw_evidence.disposition {
            RawEvidenceDisposition::Protected { confirmed_at, .. } => {
                (confirmed_at, CleanupEventKind::ProtectedRetentionConfirmed)
            }
            RawEvidenceDisposition::Deleted { deleted_at } => {
                (deleted_at, CleanupEventKind::WorkingCopyDeleted)
            }
        };
        if disposition_time <= &self.observation.completed_at
            || !self.raw_evidence.cleanup_events.iter().any(|event| {
                event.kind == required_cleanup_kind && &event.occurred_at == disposition_time
            })
        {
            violations.push(ContractViolation {
                code: "behavior-admission.disposition.unconfirmed",
                message: format!(
                    "admission {} lacks cleanup confirmation for its evidence disposition",
                    self.id
                ),
            });
        }

        if set_of_strings(&self.specification.parent_provenance_ids)
            != set_of_strings(&self.source_provenance_ids)
        {
            violations.push(ContractViolation {
                code: "behavior-admission.specification.parent-mismatch",
                message: format!(
                    "admission {} specification parents do not equal its sources",
                    self.id
                ),
            });
        }

        let review = &self.specification.technical_review;
        if let Some(reviewed_by) = &review.reviewed_by
            && reviewed_by != &self.roles.specification_reviewer.actor
        {
            violations.push(ContractViolation {
                code: "behavior-admission.review.reviewer-mismatch",
                message: format!(
                    "admission {} review was not made by its assigned reviewer",
                    self.id
                ),
            });
        }
        if review
            .decided_at
            .as_deref()
            .is_some_and(|decided_at| decided_at <= self.observation.completed_at.as_str())
        {
            violations.push(ContractViolation {
                code: "behavior-admission.review.decided-before-observation",
                message: format!(
                    "admission {} review predates completed observation",
                    self.id
                ),
            });
        }
        if review.decided_at.as_deref().is_some_and(|decided_at| {
            disposition_time.as_str() >= decided_at
                || self
                    .raw_evidence
                    .cleanup_events
                    .iter()
                    .any(|event| event.occurred_at.as_str() >= decided_at)
        }) {
            violations.push(ContractViolation {
                code: "behavior-admission.review.before-cleanup",
                message: format!(
                    "admission {} review predates evidence disposition or cleanup",
                    self.id
                ),
            });
        }

        match review.status {
            SpecificationReviewStatus::Pending | SpecificationReviewStatus::Rejected => {
                if self.implementation_handoff.is_some() {
                    violations.push(ContractViolation {
                        code: "behavior-admission.handoff.not-approved",
                        message: format!(
                            "admission {} cannot hand off an unapproved specification",
                            self.id
                        ),
                    });
                }
            }
            SpecificationReviewStatus::Approved => {
                if self.implementation_handoff.is_none() {
                    violations.push(ContractViolation {
                        code: "behavior-admission.handoff.missing",
                        message: format!(
                            "approved admission {} requires a controlled handoff",
                            self.id
                        ),
                    });
                }
                if self.derived_tests.is_empty() {
                    violations.push(ContractViolation {
                        code: "behavior-admission.derived-test.missing",
                        message: format!(
                            "approved admission {} requires an independently derived test",
                            self.id
                        ),
                    });
                }
            }
        }

        if let Some(handoff) = &self.implementation_handoff {
            if handoff.specification_provenance_id != self.specification.provenance_id
                || handoff.specification_digest != self.specification.content_digest
                || handoff.implementer != self.roles.implementer.actor
            {
                violations.push(ContractViolation {
                    code: "behavior-admission.handoff.mismatch",
                    message: format!(
                        "admission {} handoff does not match specification and implementer",
                        self.id
                    ),
                });
            }
            if review
                .decided_at
                .as_deref()
                .is_none_or(|decided_at| handoff.handed_off_at.as_str() <= decided_at)
            {
                violations.push(ContractViolation {
                    code: "behavior-admission.handoff.time-order",
                    message: format!("admission {} handoff predates technical approval", self.id),
                });
            }
        }

        let mut test_ids = BTreeSet::new();
        let mut test_paths = BTreeSet::new();
        for derived_test in &self.derived_tests {
            if !test_ids.insert(derived_test.provenance_id.as_str())
                || !test_paths.insert(derived_test.artifact_path.as_str())
            {
                violations.push(ContractViolation {
                    code: "behavior-admission.derived-test.identity-duplicate",
                    message: format!(
                        "admission {} repeats a derived-test identity or path",
                        self.id
                    ),
                });
            }
            if !derived_test
                .parent_provenance_ids
                .contains(&self.specification.provenance_id)
            {
                violations.push(ContractViolation {
                    code: "behavior-admission.derived-test.specification-parent-missing",
                    message: format!(
                        "admission {} derived test does not name the specification parent",
                        self.id
                    ),
                });
            }
        }

        violations
    }
}

impl BehaviorSpecificationAdmissionLedger {
    /// Validates constraints expressed by the published admission-ledger schema.
    #[must_use]
    pub fn validate_schema_semantics(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        if self.schema_version != BEHAVIOR_SPECIFICATION_ADMISSION_SCHEMA_VERSION {
            violations.push(ContractViolation {
                code: "behavior-admission.schema-version.unsupported",
                message: format!(
                    "unsupported behavior admission schema version: {}",
                    self.schema_version
                ),
            });
        }
        for admission in &self.admissions {
            violations.extend(admission.validate_schema_semantics());
        }
        violations
    }

    /// Validates standalone admission identities and clean-room invariants.
    #[must_use]
    pub fn validate(&self) -> Vec<ContractViolation> {
        let mut violations = self.validate_schema_semantics();
        let mut admission_ids = BTreeSet::new();
        let mut case_ids = BTreeSet::new();

        for admission in &self.admissions {
            violations.extend(admission.validate_document_semantics());
            if !admission_ids.insert(admission.id.as_str()) {
                violations.push(ContractViolation {
                    code: "behavior-admission.id.duplicate",
                    message: format!("duplicate behavior admission id: {}", admission.id),
                });
            }
            if !case_ids.insert(admission.case_id.as_str()) {
                violations.push(ContractViolation {
                    code: "behavior-admission.case-id.duplicate",
                    message: format!("duplicate behavior case id: {}", admission.case_id),
                });
            }
        }

        violations
    }

    /// Validates exact feature, target, provenance, and legal-ledger references.
    #[must_use]
    pub fn validate_references(
        &self,
        targets: &TargetMatrix,
        features: &FeatureMatrix,
        provenance: &ProvenanceLedger,
        legal_reviews: &LegalReviewLedger,
    ) -> Vec<ContractViolation> {
        let mut violations = self.validate();

        for admission in &self.admissions {
            violations.extend(admission.validate_references(
                targets,
                features,
                provenance,
                legal_reviews,
            ));
        }

        violations
    }

    /// Validates whether one exact feature and target may guide implementation.
    ///
    /// Candidate-authored specification-review metadata is never sufficient
    /// authority. Until a protected review authority is added, an otherwise
    /// approved admission remains fail closed.
    #[must_use]
    fn validate_exact_implementation(
        &self,
        feature_id: &str,
        target_id: &str,
        context: ImplementationAdmissionContext<'_>,
    ) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let matches = self
            .admissions
            .iter()
            .filter(|admission| {
                admission.target_id == target_id
                    && admission.feature_ids.iter().any(|id| id == feature_id)
            })
            .collect::<Vec<_>>();
        let [admission] = matches.as_slice() else {
            violations.push(ContractViolation {
                code: if matches.is_empty() {
                    "behavior-admission.missing"
                } else {
                    "behavior-admission.ambiguous"
                },
                message: format!(
                    "implementation requires exactly one admission for feature {feature_id} and target {target_id}"
                ),
            });
            return violations;
        };

        match admission.specification.technical_review.status {
            SpecificationReviewStatus::Pending => violations.push(ContractViolation {
                code: "behavior-admission.review.pending",
                message: format!("admission {} technical review is pending", admission.id),
            }),
            SpecificationReviewStatus::Rejected => violations.push(ContractViolation {
                code: "behavior-admission.review.rejected",
                message: format!("admission {} technical review was rejected", admission.id),
            }),
            SpecificationReviewStatus::Approved => violations.push(ContractViolation {
                code: "behavior-admission.review-authority.required",
                message: format!(
                    "admission {} requires protected specification-review authority",
                    admission.id
                ),
            }),
        }
        let observation_deadline = AdmissionUseDeadline {
            timestamp: &admission.observation.started_at,
            boundary: "observation",
        };

        if let Some(target) = context
            .targets
            .targets
            .iter()
            .find(|target| target.id == admission.target_id)
        {
            violations.extend(validate_admission_closure_uses(
                context,
                &admission.id,
                &target.provenance_id,
                ProvenanceUse::OracleOperation,
                Some(observation_deadline),
            ));
        }
        for source_id in &admission.source_provenance_ids {
            violations.extend(validate_admission_closure_uses(
                context,
                &admission.id,
                source_id,
                ProvenanceUse::ImplementationInput,
                Some(observation_deadline),
            ));
        }
        let specification_deadline = admission
            .specification
            .technical_review
            .decided_at
            .as_deref()
            .map(|timestamp| AdmissionUseDeadline {
                timestamp,
                boundary: "technical review",
            });
        violations.extend(validate_admission_closure_uses(
            context,
            &admission.id,
            &admission.specification.provenance_id,
            ProvenanceUse::ImplementationInput,
            specification_deadline,
        ));
        let derived_test_deadline =
            admission
                .implementation_handoff
                .as_ref()
                .map(|handoff| AdmissionUseDeadline {
                    timestamp: &handoff.handed_off_at,
                    boundary: "implementation handoff",
                });
        for derived_test in &admission.derived_tests {
            violations.extend(validate_admission_closure_uses(
                context,
                &admission.id,
                &derived_test.provenance_id,
                ProvenanceUse::ConformanceEvidence,
                derived_test_deadline,
            ));
        }

        violations
    }
}

impl BehaviorSpecificationAdmissionRecord {
    fn validate_references(
        &self,
        targets: &TargetMatrix,
        features: &FeatureMatrix,
        provenance: &ProvenanceLedger,
        legal_reviews: &LegalReviewLedger,
    ) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let target_matches = targets
            .targets
            .iter()
            .filter(|target| target.id == self.target_id)
            .collect::<Vec<_>>();
        let target = match target_matches.as_slice() {
            [target] => Some(*target),
            _ => {
                violations.push(ContractViolation {
                    code: if target_matches.is_empty() {
                        "behavior-admission.target.unknown"
                    } else {
                        "behavior-admission.target.duplicate"
                    },
                    message: format!(
                        "admission {} requires one target {}",
                        self.id, self.target_id
                    ),
                });
                None
            }
        };

        for feature_id in &self.feature_ids {
            let feature_matches = features
                .features
                .iter()
                .filter(|feature| feature.id == *feature_id)
                .collect::<Vec<_>>();
            let [feature] = feature_matches.as_slice() else {
                violations.push(ContractViolation {
                    code: if feature_matches.is_empty() {
                        "behavior-admission.feature.unknown"
                    } else {
                        "behavior-admission.feature.duplicate"
                    },
                    message: format!("admission {} requires one feature {feature_id}", self.id),
                });
                continue;
            };
            if !feature.oracle_targets.contains(&self.target_id) {
                violations.push(ContractViolation {
                    code: "behavior-admission.feature.target-mismatch",
                    message: format!(
                        "feature {feature_id} does not name admission target {}",
                        self.target_id
                    ),
                });
            }
            if !feature.evidence.contains(&self.specification.provenance_id) {
                violations.push(ContractViolation {
                    code: "behavior-admission.feature.specification-unlinked",
                    message: format!(
                        "feature {feature_id} does not name specification {}",
                        self.specification.provenance_id
                    ),
                });
            }
        }

        let mut referenced_records = Vec::new();
        if let Some(target) = target {
            match find_exact_provenance(provenance, &target.provenance_id, "target", &self.id) {
                Ok(record) => referenced_records.push(record),
                Err(violation) => violations.push(violation),
            }
        }
        for source_id in &self.source_provenance_ids {
            match find_exact_provenance(provenance, source_id, "source", &self.id) {
                Ok(record) => {
                    referenced_records.push(record);
                    if !matches!(
                        record.source_kind,
                        ProvenanceSourceKind::PublicDocumentation
                            | ProvenanceSourceKind::OpenSpecification
                            | ProvenanceSourceKind::Standard
                            | ProvenanceSourceKind::PublicApi
                            | ProvenanceSourceKind::OracleObservation
                    ) {
                        violations.push(ContractViolation {
                            code: "behavior-admission.source.kind-invalid",
                            message: format!(
                                "admission {} source {} has unsupported kind",
                                self.id, source_id
                            ),
                        });
                    }
                }
                Err(violation) => violations.push(violation),
            }
        }

        let specification_record = find_exact_provenance(
            provenance,
            &self.specification.provenance_id,
            "specification",
            &self.id,
        );
        match specification_record {
            Ok(record) => {
                referenced_records.push(record);
                if record.source_kind != ProvenanceSourceKind::BehaviorSpecification
                    || record.artifact_path.as_deref() != Some(&self.specification.artifact_path)
                    || record.content_digest != self.specification.content_digest
                    || set_of_strings(&record.parent_provenance_ids)
                        != set_of_strings(&self.specification.parent_provenance_ids)
                {
                    violations.push(ContractViolation {
                        code: "behavior-admission.specification.provenance-mismatch",
                        message: format!(
                            "admission {} does not exactly match specification provenance {}",
                            self.id, record.id
                        ),
                    });
                }
            }
            Err(violation) => violations.push(violation),
        }

        for derived_test in &self.derived_tests {
            let test_record = find_exact_provenance(
                provenance,
                &derived_test.provenance_id,
                "derived test",
                &self.id,
            );
            match test_record {
                Ok(record) => {
                    referenced_records.push(record);
                    if record.source_kind != ProvenanceSourceKind::Test
                        || record.artifact_path.as_deref() != Some(&derived_test.artifact_path)
                        || record.content_digest != derived_test.content_digest
                        || set_of_strings(&record.parent_provenance_ids)
                            != set_of_strings(&derived_test.parent_provenance_ids)
                    {
                        violations.push(ContractViolation {
                            code: "behavior-admission.derived-test.provenance-mismatch",
                            message: format!(
                                "admission {} does not exactly match test provenance {}",
                                self.id, record.id
                            ),
                        });
                    }
                }
                Err(violation) => violations.push(violation),
            }
        }

        let roots = referenced_records
            .iter()
            .map(|record| record.id.clone())
            .collect::<Vec<_>>();
        let expected_legal_ids =
            provenance_closure_ids(&roots, &provenance.records).map(|closure| {
                closure
                    .iter()
                    .filter_map(|id| {
                        provenance
                            .records
                            .iter()
                            .find(|record| record.id == *id)
                            .map(|record| record.legal_review_id.as_str())
                    })
                    .collect::<BTreeSet<_>>()
            });
        if expected_legal_ids.as_ref() != Some(&set_of_strings(&self.legal_review_ids)) {
            violations.push(ContractViolation {
                code: "behavior-admission.legal-review-set.mismatch",
                message: format!(
                    "admission {} legal reviews do not equal its provenance closure",
                    self.id
                ),
            });
        }
        for legal_review_id in &self.legal_review_ids {
            let count = legal_reviews
                .reviews
                .iter()
                .filter(|review| review.id == *legal_review_id)
                .count();
            if count != 1 {
                violations.push(ContractViolation {
                    code: if count == 0 {
                        "behavior-admission.legal-review.unknown"
                    } else {
                        "behavior-admission.legal-review.duplicate"
                    },
                    message: format!(
                        "admission {} requires one legal review {}",
                        self.id, legal_review_id
                    ),
                });
            }
        }

        violations
    }
}

fn authenticated_legal_review_time<'a>(
    review: &LegalReviewRecord,
    provenance: &ProvenanceLedger,
    verification: LegalDecisionVerificationContext<'a>,
) -> Option<&'a str> {
    let reference = review.decision_evidence.as_ref()?;
    let matches = verification
        .authority
        .pull_requests
        .iter()
        .filter(|pull_request| {
            pull_request.repository == reference.repository
                && pull_request.pull_request_number == reference.pull_request_number
        })
        .flat_map(|pull_request| {
            pull_request
                .authenticated_reviews
                .iter()
                .filter(move |evidence| {
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
        })
        .collect::<Vec<_>>();
    let [evidence] = matches.as_slice() else {
        return None;
    };
    Some(authenticated_review_effective_at(evidence))
}

#[derive(Clone, Copy)]
struct AdmissionUseDeadline<'a> {
    timestamp: &'a str,
    boundary: &'static str,
}

fn validate_admission_closure_uses(
    context: ImplementationAdmissionContext<'_>,
    admission_id: &str,
    root_id: &str,
    requested_use: ProvenanceUse,
    deadline: Option<AdmissionUseDeadline<'_>>,
) -> Vec<ContractViolation> {
    let roots = [root_id.to_owned()];
    let Some(closure) = provenance_closure_ids(&roots, &context.provenance.records) else {
        return vec![ContractViolation {
            code: "behavior-admission.provenance-closure.invalid",
            message: format!("cannot validate governed uses for provenance closure {root_id}"),
        }];
    };
    let mut violations = Vec::new();
    for provenance_id in closure {
        violations.extend(context.provenance.validate_use(
            context.legal_reviews,
            context.legal_verification,
            provenance_id,
            requested_use,
        ));
        let Some(deadline) = deadline else {
            continue;
        };
        let Some(verification) = context.legal_verification else {
            continue;
        };
        let Some(record) = context
            .provenance
            .records
            .iter()
            .find(|record| record.id == provenance_id)
        else {
            continue;
        };
        let reviews = context
            .legal_reviews
            .reviews
            .iter()
            .filter(|review| review.id == record.legal_review_id)
            .collect::<Vec<_>>();
        let [review] = reviews.as_slice() else {
            continue;
        };
        let Some(effective_at) =
            authenticated_legal_review_time(review, context.provenance, verification)
        else {
            continue;
        };
        if !is_valid_iso_utc_timestamp(effective_at) || effective_at >= deadline.timestamp {
            violations.push(ContractViolation {
                code: "behavior-admission.legal-review.not-prior",
                message: format!(
                    "admission {admission_id} legal review {} for provenance {provenance_id} does not predate {}",
                    review.id, deadline.boundary
                ),
            });
        }
    }
    violations
}

fn find_exact_provenance<'a>(
    provenance: &'a ProvenanceLedger,
    provenance_id: &str,
    role: &str,
    admission_id: &str,
) -> Result<&'a ProvenanceRecord, ContractViolation> {
    let matches = provenance
        .records
        .iter()
        .filter(|record| record.id == provenance_id)
        .collect::<Vec<_>>();
    let [record] = matches.as_slice() else {
        return Err(ContractViolation {
            code: if matches.is_empty() {
                "behavior-admission.provenance.unknown"
            } else {
                "behavior-admission.provenance.duplicate"
            },
            message: format!(
                "admission {} requires one {role} provenance {provenance_id}",
                admission_id
            ),
        });
    };
    Ok(*record)
}

fn admission_role_assignments(roles: &CleanRoomRoles) -> [&CleanRoomRoleAssignment; 4] {
    [
        &roles.observer,
        &roles.specification_reviewer,
        &roles.implementer,
        &roles.conformance_reviewer,
    ]
}

fn set_of_strings(values: &[String]) -> BTreeSet<&str> {
    values.iter().map(String::as_str).collect()
}

fn digest_length_is_consistent(digest: &str, byte_length: u64) -> bool {
    (byte_length == 0) == (digest == SHA256_EMPTY_CONTENT)
}

impl ProvenanceRecord {
    /// Validates constraints expressed by the published provenance record schema.
    #[must_use]
    pub fn validate_schema_semantics(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if !is_contract_identifier(&self.id) {
            violations.push(ContractViolation {
                code: "provenance.id.invalid",
                message: "provenance id is malformed".to_owned(),
            });
        }

        if !has_schema_non_whitespace(&self.title)
            || !has_schema_non_whitespace(&self.revision)
            || !has_schema_non_whitespace(&self.author)
            || !has_schema_non_whitespace(&self.generation_method)
            || !has_schema_non_whitespace(&self.license)
        {
            violations.push(ContractViolation {
                code: "provenance.metadata.empty",
                message: format!("provenance {} has incomplete source metadata", self.id),
            });
        }
        if self
            .environment
            .as_deref()
            .is_some_and(|environment| !has_schema_non_whitespace(environment))
        {
            violations.push(ContractViolation {
                code: "provenance.environment.empty",
                message: format!("provenance {} has an empty environment", self.id),
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

        let mut parent_ids = BTreeSet::new();
        for parent_id in &self.parent_provenance_ids {
            if !is_contract_identifier(parent_id) {
                violations.push(ContractViolation {
                    code: "provenance.parent.invalid",
                    message: format!("provenance {} has malformed parent {parent_id}", self.id),
                });
            }
            if !parent_ids.insert(parent_id.as_str()) {
                violations.push(ContractViolation {
                    code: "provenance.parent.duplicate",
                    message: format!("provenance {} repeats parent {parent_id}", self.id),
                });
            }
        }

        if !is_contract_identifier(&self.legal_review_id) {
            violations.push(ContractViolation {
                code: "provenance.legal-review.invalid",
                message: format!(
                    "provenance {} has a malformed legal review reference",
                    self.id
                ),
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
    /// Validates constraints expressed by the published provenance-ledger schema.
    #[must_use]
    pub fn validate_schema_semantics(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if self.schema_version != COMPATIBILITY_SCHEMA_VERSION {
            violations.push(ContractViolation {
                code: "provenance.schema-version.unsupported",
                message: format!(
                    "unsupported provenance schema version: {}",
                    self.schema_version
                ),
            });
        }
        if self.records.is_empty() {
            violations.push(ContractViolation {
                code: "provenance.record.missing",
                message: "provenance ledger requires at least one record".to_owned(),
            });
        }
        for record in &self.records {
            violations.extend(record.validate_schema_semantics());
        }

        violations
    }

    /// Validates provenance structure and every cross-ledger reference.
    #[must_use]
    pub fn validate(&self, legal_reviews: &LegalReviewLedger) -> Vec<ContractViolation> {
        let mut violations = self.validate_schema_semantics();
        let mut provenance_ids = BTreeSet::new();
        let legal_review_ids: BTreeSet<&str> = legal_reviews
            .reviews
            .iter()
            .map(|review| review.id.as_str())
            .collect();

        for record in &self.records {
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
            for parent_id in &record.parent_provenance_ids {
                if parent_id == &record.id {
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

impl ConformanceObservations {
    fn entries(&self) -> [(&'static str, &DimensionObservation); 7] {
        [
            ("syntax", &self.syntax),
            ("wire", &self.wire),
            ("result", &self.result),
            ("metadata", &self.metadata),
            ("diagnostic", &self.diagnostic),
            ("transactional_side_effect", &self.transactional_side_effect),
            ("operational", &self.operational),
        ]
    }
}

impl RawEvidence {
    fn validate_schema_semantics(&self, dimension: &str, side: &str) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let Self::Artifact {
            store_id,
            artifact_id,
            content_digest,
            byte_length,
            media_type,
            access: _,
        } = self
        else {
            return violations;
        };

        for (field, identifier) in [("store_id", store_id), ("artifact_id", artifact_id)] {
            if !is_contract_identifier(identifier) {
                violations.push(ContractViolation {
                    code: "conformance.raw-artifact.identifier.invalid",
                    message: format!(
                        "conformance {dimension} {side} raw artifact {field} is malformed"
                    ),
                });
            }
        }
        if !is_canonical_sha256_digest(content_digest) {
            violations.push(ContractViolation {
                code: "conformance.raw-artifact.digest.invalid",
                message: format!(
                    "conformance {dimension} {side} raw artifact requires a SHA-256 digest"
                ),
            });
        }
        if (*byte_length == 0) != (content_digest == SHA256_EMPTY_CONTENT) {
            violations.push(ContractViolation {
                code: "conformance.raw-artifact.empty-digest-mismatch",
                message: format!(
                    "conformance {dimension} {side} raw artifact zero length and empty-content digest must agree"
                ),
            });
        }
        if !has_schema_non_whitespace(media_type) {
            violations.push(ContractViolation {
                code: "conformance.raw-artifact.media-type.empty",
                message: format!(
                    "conformance {dimension} {side} raw artifact requires a media type"
                ),
            });
        }
        violations
    }
}

impl ConformanceRecord {
    /// Validates constraints expressed by the published conformance-record schema.
    #[must_use]
    pub fn validate_schema_semantics(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if self.schema_version != CONFORMANCE_SCHEMA_VERSION {
            violations.push(ContractViolation {
                code: "conformance.schema-version.unsupported",
                message: format!(
                    "unsupported conformance schema version: {}",
                    self.schema_version
                ),
            });
        }
        for (field, identifier) in [
            ("case_id", self.case_id.as_str()),
            ("feature_id", self.feature_id.as_str()),
            ("target_id", self.target_id.as_str()),
            ("provenance_id", self.provenance_id.as_str()),
            ("runner_id", self.reproduction.runner_id.as_str()),
        ] {
            if !is_contract_identifier(identifier) {
                violations.push(ContractViolation {
                    code: "conformance.identifier.invalid",
                    message: format!("conformance {field} is malformed: {identifier}"),
                });
            }
        }
        if self.owner_issue == 0 {
            violations.push(ContractViolation {
                code: "conformance.owner-issue.invalid",
                message: "conformance owner_issue must be positive".to_owned(),
            });
        }
        if !is_iso_utc_timestamp(&self.observed_at) {
            violations.push(ContractViolation {
                code: "conformance.observed-at.invalid",
                message: "conformance observed_at must use UTC second precision".to_owned(),
            });
        }
        for (field, revision) in [
            (
                "runner_revision",
                self.reproduction.runner_revision.as_str(),
            ),
            (
                "subject_revision",
                self.reproduction.subject_revision.as_str(),
            ),
        ] {
            if !is_git_commit_sha(revision) {
                violations.push(ContractViolation {
                    code: "conformance.revision.invalid",
                    message: format!("conformance {field} must be a lowercase Git commit SHA"),
                });
            }
        }
        for (field, digest) in [
            ("runner_digest", self.reproduction.runner_digest.as_str()),
            ("subject_digest", self.reproduction.subject_digest.as_str()),
            ("input_digest", self.reproduction.input_digest.as_str()),
        ] {
            if !is_canonical_sha256_digest(digest) {
                violations.push(ContractViolation {
                    code: "conformance.reproduction.digest.invalid",
                    message: format!("conformance {field} requires a SHA-256 digest"),
                });
            }
        }
        if !has_schema_non_whitespace(&self.reproduction.case_seed) {
            violations.push(ContractViolation {
                code: "conformance.case-seed.empty",
                message: "conformance case_seed must be nonempty".to_owned(),
            });
        }
        if self.reproduction.environment.is_empty() {
            violations.push(ContractViolation {
                code: "conformance.environment.empty",
                message: "conformance environment requires at least one fact".to_owned(),
            });
        }
        for fact in &self.reproduction.environment {
            if !is_contract_identifier(&fact.name) {
                violations.push(ContractViolation {
                    code: "conformance.environment.name.invalid",
                    message: format!(
                        "conformance environment fact name is malformed: {}",
                        fact.name
                    ),
                });
            }
            if !has_schema_non_whitespace(&fact.value) {
                violations.push(ContractViolation {
                    code: "conformance.environment.value.empty",
                    message: format!(
                        "conformance environment fact {} requires a value",
                        fact.name
                    ),
                });
            }
        }
        if has_duplicates(&self.reproduction.environment) {
            violations.push(ContractViolation {
                code: "conformance.environment.duplicate",
                message: "conformance environment contains a duplicate fact".to_owned(),
            });
        }
        for argument in &self.reproduction.arguments {
            if !has_schema_non_whitespace(argument) {
                violations.push(ContractViolation {
                    code: "conformance.rerun-argument.empty",
                    message: "conformance rerun arguments must be nonempty".to_owned(),
                });
            }
            if argument.contains('\0') {
                violations.push(ContractViolation {
                    code: "conformance.rerun-argument.nul",
                    message: "conformance rerun arguments cannot contain NUL".to_owned(),
                });
            }
        }
        for rule in &self.normalization_rules {
            for (field, identifier) in [
                ("id", rule.id.as_str()),
                ("provenance_id", rule.provenance_id.as_str()),
            ] {
                if !is_contract_identifier(identifier) {
                    violations.push(ContractViolation {
                        code: "conformance.normalization-rule.identifier.invalid",
                        message: format!(
                            "conformance normalization rule {field} is malformed: {identifier}"
                        ),
                    });
                }
            }
            if rule.revision == 0 {
                violations.push(ContractViolation {
                    code: "conformance.normalization-rule.revision.invalid",
                    message: format!(
                        "conformance normalization rule {} requires a positive revision",
                        rule.id
                    ),
                });
            }
            if !has_schema_non_whitespace(&rule.description) {
                violations.push(ContractViolation {
                    code: "conformance.normalization-rule.description.empty",
                    message: format!(
                        "conformance normalization rule {} requires a description",
                        rule.id
                    ),
                });
            }
        }
        if has_duplicates(&self.normalization_rules) {
            violations.push(ContractViolation {
                code: "conformance.normalization-rule.duplicate",
                message: "conformance normalization rules contain an exact duplicate".to_owned(),
            });
        }

        for (dimension, observation) in self.observations.entries() {
            match observation {
                DimensionObservation::NotObserved { reason } => {
                    if !has_schema_non_whitespace(reason) {
                        violations.push(ContractViolation {
                            code: "conformance.observation.reason.empty",
                            message: format!("conformance {dimension} requires a reason"),
                        });
                    }
                }
                DimensionObservation::Observed {
                    raw,
                    normalized: _,
                    normalization_rules,
                    status: _,
                } => {
                    violations.extend(raw.oracle.validate_schema_semantics(dimension, "oracle"));
                    violations.extend(raw.subject.validate_schema_semantics(dimension, "subject"));
                    for rule in normalization_rules {
                        if !is_contract_identifier(&rule.id) {
                            violations.push(ContractViolation {
                                code: "conformance.normalization-reference.id.invalid",
                                message: format!(
                                    "conformance {dimension} normalization rule id is malformed: {}",
                                    rule.id
                                ),
                            });
                        }
                        if rule.revision == 0 {
                            violations.push(ContractViolation {
                                code: "conformance.normalization-reference.revision.invalid",
                                message: format!(
                                    "conformance {dimension} normalization rule {} requires a positive revision",
                                    rule.id
                                ),
                            });
                        }
                    }
                    if has_duplicates(normalization_rules) {
                        violations.push(ContractViolation {
                            code: "conformance.normalization-reference.duplicate",
                            message: format!(
                                "conformance {dimension} repeats a normalization rule"
                            ),
                        });
                    }
                }
            }
        }

        violations
    }

    /// Validates invariants that JSON Schema cannot express within one record.
    #[must_use]
    pub fn validate_document_semantics(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let mut environment_names = BTreeSet::new();
        for fact in &self.reproduction.environment {
            if !environment_names.insert(fact.name.as_str()) {
                violations.push(ContractViolation {
                    code: "conformance.environment.name.duplicate",
                    message: format!("conformance environment repeats fact name {}", fact.name),
                });
            }
        }

        let mut defined_rules = BTreeSet::new();
        for rule in &self.normalization_rules {
            if !defined_rules.insert((rule.id.as_str(), rule.revision)) {
                violations.push(ContractViolation {
                    code: "conformance.normalization-rule.identity.duplicate",
                    message: format!(
                        "conformance repeats normalization rule {} revision {}",
                        rule.id, rule.revision
                    ),
                });
            }
        }

        let mut referenced_rules = BTreeSet::new();
        for (dimension, observation) in self.observations.entries() {
            let DimensionObservation::Observed {
                raw,
                normalized,
                normalization_rules,
                status,
            } = observation
            else {
                continue;
            };
            if normalization_rules.is_empty()
                && (!raw_evidence_is_identity(&raw.oracle, &normalized.oracle)
                    || !raw_evidence_is_identity(&raw.subject, &normalized.subject))
            {
                violations.push(ContractViolation {
                    code: "conformance.normalization.unrecorded",
                    message: format!(
                        "conformance {dimension} changes or projects raw evidence without a normalization rule"
                    ),
                });
            }
            let normalized_values_match =
                json_values_are_identical(&normalized.oracle, &normalized.subject);
            if *status == ComparisonStatus::Compatible && !normalized_values_match {
                violations.push(ContractViolation {
                    code: "conformance.comparison.compatible-mismatch",
                    message: format!(
                        "conformance {dimension} marks unequal normalized values compatible"
                    ),
                });
            }
            if *status == ComparisonStatus::Divergent && normalized_values_match {
                violations.push(ContractViolation {
                    code: "conformance.comparison.divergent-match",
                    message: format!(
                        "conformance {dimension} marks equal normalized values divergent"
                    ),
                });
            }
            for rule in normalization_rules {
                let identity = (rule.id.as_str(), rule.revision);
                if !defined_rules.contains(&identity) {
                    violations.push(ContractViolation {
                        code: "conformance.normalization-reference.unknown",
                        message: format!(
                            "conformance {dimension} references unknown normalization rule {} revision {}",
                            rule.id, rule.revision
                        ),
                    });
                }
                referenced_rules.insert(identity);
            }
        }
        for rule in &self.normalization_rules {
            if !referenced_rules.contains(&(rule.id.as_str(), rule.revision)) {
                violations.push(ContractViolation {
                    code: "conformance.normalization-rule.unused",
                    message: format!(
                        "conformance normalization rule {} revision {} is unused",
                        rule.id, rule.revision
                    ),
                });
            }
        }

        violations
    }

    /// Validates target, feature, owner, and provenance references.
    #[must_use]
    pub fn validate_references(
        &self,
        targets: &TargetMatrix,
        features: &FeatureMatrix,
        provenance: &ProvenanceLedger,
    ) -> Vec<ContractViolation> {
        let mut violations = Vec::new();
        let mut matching_targets = targets
            .targets
            .iter()
            .filter(|target| target.id == self.target_id);
        match (matching_targets.next(), matching_targets.next()) {
            (None, _) => violations.push(ContractViolation {
                code: "conformance.target.unknown",
                message: format!("unknown conformance target: {}", self.target_id),
            }),
            (Some(_), Some(_)) => violations.push(ContractViolation {
                code: "conformance.target.ambiguous",
                message: format!("conformance target {} is not unique", self.target_id),
            }),
            (Some(_), None) => {}
        }

        let mut matching_features = features
            .features
            .iter()
            .filter(|feature| feature.id == self.feature_id);
        match (matching_features.next(), matching_features.next()) {
            (Some(feature), None) => {
                if feature.status == CompatibilityStatus::BlockedLegal {
                    violations.push(ContractViolation {
                        code: "conformance.feature.blocked-legal",
                        message: format!(
                            "conformance feature {} is blocked by legal review",
                            feature.id
                        ),
                    });
                }
                if feature.owner_issue != self.owner_issue {
                    violations.push(ContractViolation {
                        code: "conformance.feature.owner-mismatch",
                        message: format!(
                            "conformance feature {} is owned by issue {}, not {}",
                            feature.id, feature.owner_issue, self.owner_issue
                        ),
                    });
                }
                if !feature.oracle_targets.contains(&self.target_id) {
                    violations.push(ContractViolation {
                        code: "conformance.feature.target-mismatch",
                        message: format!(
                            "conformance feature {} does not include target {}",
                            feature.id, self.target_id
                        ),
                    });
                }
            }
            (None, _) => violations.push(ContractViolation {
                code: "conformance.feature.unknown",
                message: format!("unknown conformance feature: {}", self.feature_id),
            }),
            (Some(_), Some(_)) => violations.push(ContractViolation {
                code: "conformance.feature.ambiguous",
                message: format!("conformance feature {} is not unique", self.feature_id),
            }),
        }

        let provenance_ids = provenance
            .records
            .iter()
            .map(|record| record.id.as_str())
            .collect::<BTreeSet<_>>();
        if !provenance_ids.contains(self.provenance_id.as_str()) {
            violations.push(ContractViolation {
                code: "conformance.provenance.unknown",
                message: format!(
                    "conformance references unknown provenance {}",
                    self.provenance_id
                ),
            });
        }
        for rule in &self.normalization_rules {
            if !provenance_ids.contains(rule.provenance_id.as_str()) {
                violations.push(ContractViolation {
                    code: "conformance.normalization-rule.provenance.unknown",
                    message: format!(
                        "conformance normalization rule {} references unknown provenance {}",
                        rule.id, rule.provenance_id
                    ),
                });
            }
        }

        violations
    }

    /// Validates that a conformance run used an approved oracle and evidence source.
    #[must_use]
    pub fn validate_governance(
        &self,
        targets: &TargetMatrix,
        features: &FeatureMatrix,
        provenance: &ProvenanceLedger,
        legal_reviews: &LegalReviewLedger,
        legal_verification: Option<LegalDecisionVerificationContext<'_>>,
    ) -> Vec<ContractViolation> {
        let mut violations = self.validate_schema_semantics();
        violations.extend(self.validate_document_semantics());
        violations.extend(self.validate_references(targets, features, provenance));

        if let Some(target) = targets
            .targets
            .iter()
            .find(|target| target.id == self.target_id)
        {
            violations.extend(provenance.validate_use(
                legal_reviews,
                legal_verification,
                &target.provenance_id,
                ProvenanceUse::OracleOperation,
            ));
        }
        let mut evidence_ids = BTreeSet::from([self.provenance_id.as_str()]);
        evidence_ids.extend(
            self.normalization_rules
                .iter()
                .map(|rule| rule.provenance_id.as_str()),
        );
        for provenance_id in evidence_ids {
            violations.extend(provenance.validate_use(
                legal_reviews,
                legal_verification,
                provenance_id,
                ProvenanceUse::ConformanceEvidence,
            ));
        }
        violations
    }
}

impl FeatureMatrix {
    /// Validates the approved clean-room inputs for implementation of one feature.
    #[must_use]
    pub fn validate_implementation_inputs(
        &self,
        feature_id: &str,
        target_id: &str,
        context: ImplementationAdmissionContext<'_>,
    ) -> Vec<ContractViolation> {
        let mut violations = context.targets.validate();
        violations.extend(self.validate(context.targets));
        violations.extend(validate_governance_references(
            context.targets,
            self,
            context.provenance,
            context.legal_reviews,
        ));
        violations.extend(context.admissions.validate_references(
            context.targets,
            self,
            context.provenance,
            context.legal_reviews,
        ));

        let feature_matches = self
            .features
            .iter()
            .filter(|feature| feature.id == feature_id)
            .collect::<Vec<_>>();
        match feature_matches.as_slice() {
            [] => violations.push(ContractViolation {
                code: "feature.id.unknown",
                message: format!("unknown feature: {feature_id}"),
            }),
            [feature] if feature.status == CompatibilityStatus::BlockedLegal => {
                violations.push(ContractViolation {
                    code: "feature.implementation.blocked-legal",
                    message: format!("feature {} is blocked by legal review", feature.id),
                });
            }
            [_, _, ..] => {}
            [_] => violations.extend(
                context
                    .admissions
                    .validate_exact_implementation(feature_id, target_id, context),
            ),
        }

        violations
    }
}

fn has_duplicates<T: Ord>(values: &[T]) -> bool {
    let mut unique = BTreeSet::new();
    values.iter().any(|value| !unique.insert(value))
}

fn has_equal_duplicates<T: PartialEq>(values: &[T]) -> bool {
    values
        .iter()
        .enumerate()
        .any(|(index, value)| values[..index].contains(value))
}

fn raw_evidence_is_identity(raw: &RawEvidence, normalized: &Value) -> bool {
    matches!(
        raw,
        RawEvidence::Inline { value } if json_values_are_identical(value, normalized)
    )
}

/// Compares conformance JSON values without numeric coercion.
///
/// Numeric lexical representations remain significant so integer and float
/// kinds, arbitrary-width integers, and signed zero cannot collapse.
#[must_use]
pub fn json_values_are_identical(left: &JsonValue, right: &JsonValue) -> bool {
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => left.to_string() == right.to_string(),
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| json_values_are_identical(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| json_values_are_identical(left, right))
                })
        }
        _ => false,
    }
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

fn is_valid_iso_utc_timestamp(value: &str) -> bool {
    if !is_iso_utc_timestamp(value) {
        return false;
    }
    let Some(year) = value[0..4].parse::<u32>().ok() else {
        return false;
    };
    let Some(month) = value[5..7].parse::<u8>().ok() else {
        return false;
    };
    let Some(day) = value[8..10].parse::<u8>().ok() else {
        return false;
    };
    let Some(hour) = value[11..13].parse::<u8>().ok() else {
        return false;
    };
    let Some(minute) = value[14..16].parse::<u8>().ok() else {
        return false;
    };
    let Some(second) = value[17..19].parse::<u8>().ok() else {
        return false;
    };
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => return false,
    };

    day > 0 && day <= days_in_month && hour < 24 && minute < 60 && second < 60
}

const fn is_leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
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
        && has_schema_non_whitespace(value)
        && !value.starts_with('/')
        && !value.contains('\\')
        && !value.split('/').any(|component| component == "..")
}

fn has_schema_non_whitespace(value: &str) -> bool {
    value.chars().any(|character| {
        !matches!(
            character,
            '\u{0009}'..='\u{000D}'
                | '\u{0020}'
                | '\u{00A0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
        )
    })
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
    /// Validates constraints expressed by the published feature record schema.
    #[must_use]
    pub fn validate(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if !is_contract_identifier(&self.id) {
            violations.push(ContractViolation {
                code: "feature.id.invalid",
                message: "feature id is malformed".to_owned(),
            });
        }
        if self.title.is_empty() {
            violations.push(ContractViolation {
                code: "feature.title.empty",
                message: "feature title must not be empty".to_owned(),
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
        for (field, identifiers) in [
            ("oracle_targets", &self.oracle_targets),
            ("evidence", &self.evidence),
        ] {
            if identifiers
                .iter()
                .any(|identifier| !is_contract_identifier(identifier))
            {
                violations.push(ContractViolation {
                    code: "feature.reference.invalid",
                    message: format!("feature {} has a malformed {field} reference", self.id),
                });
            }
            if has_duplicates(identifiers) {
                violations.push(ContractViolation {
                    code: "feature.reference.duplicate",
                    message: format!("feature {} repeats a {field} reference", self.id),
                });
            }
        }
        if self.differences.iter().any(String::is_empty) {
            violations.push(ContractViolation {
                code: "feature.difference.empty",
                message: format!("feature {} contains an empty difference", self.id),
            });
        }
        if has_duplicates(&self.differences) {
            violations.push(ContractViolation {
                code: "feature.difference.duplicate",
                message: format!("feature {} repeats a known difference", self.id),
            });
        }
        if self
            .legal_review_id
            .as_deref()
            .is_some_and(|identifier| !is_contract_identifier(identifier))
        {
            violations.push(ContractViolation {
                code: "feature.legal-review.invalid",
                message: format!("feature {} has a malformed legal review id", self.id),
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
    /// Validates constraints expressed by the published feature-matrix schema.
    #[must_use]
    pub fn validate_schema_semantics(&self) -> Vec<ContractViolation> {
        let mut violations = Vec::new();

        if self.schema_version != COMPATIBILITY_SCHEMA_VERSION {
            violations.push(ContractViolation {
                code: "feature.schema-version.unsupported",
                message: format!(
                    "unsupported feature schema version: {}",
                    self.schema_version
                ),
            });
        }
        if self.features.is_empty() {
            violations.push(ContractViolation {
                code: "feature.missing",
                message: "feature matrix requires at least one feature".to_owned(),
            });
        }
        for feature in &self.features {
            violations.extend(feature.validate());
        }

        violations
    }

    /// Validates every feature and all cross-record invariants.
    #[must_use]
    pub fn validate(&self, targets: &TargetMatrix) -> Vec<ContractViolation> {
        let mut violations = self.validate_schema_semantics();
        let mut feature_ids = BTreeSet::new();
        let mut categories = BTreeSet::new();
        let target_ids = targets.target_ids();

        for feature in &self.features {
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
        BehaviorSpecificationAdmissionLedger, CompatibilityStatus, ConformanceRecord,
        FEATURE_CATEGORIES, FeatureCategory, FeatureMatrix, FeatureRecord, FixtureArtifact,
        ImplementationAdmissionContext, LEGAL_DECISION_AUTHORITY_SCHEMA_VERSION,
        LegalDecisionAttestation, LegalDecisionAuthority, LegalDecisionEvidenceReference,
        LegalDecisionVerificationContext, LegalReviewLedger, LegalReviewRecord, LegalReviewStatus,
        LegalReviewerIdentity, OracleTarget, ProvenanceLedger, ProvenanceRecord,
        ProvenanceSourceKind, ProvenanceUse, SpecificationReviewStatus, TargetMatrix,
        validate_governance_references,
    };
    use serde_json::{Value, json};

    struct BehaviorAdmissionContracts {
        admissions: BehaviorSpecificationAdmissionLedger,
        targets: TargetMatrix,
        features: FeatureMatrix,
        provenance: ProvenanceLedger,
        legal_reviews: LegalReviewLedger,
    }

    #[test]
    fn conformance_record_requires_every_dimension() {
        let mut missing_operational = valid_conformance_document();
        let removed = missing_operational
            .get_mut("observations")
            .and_then(Value::as_object_mut)
            .and_then(|observations| observations.remove("operational"));
        assert!(removed.is_some());

        let result = serde_json::from_value::<ConformanceRecord>(missing_operational);

        assert!(result.is_err());
    }

    #[test]
    fn observed_dimension_rejects_feature_only_status() {
        let mut blocked_observation = valid_conformance_document();
        let replaced = blocked_observation
            .pointer_mut("/observations/syntax")
            .map(|observation| {
                *observation = json!({
                    "state": "observed",
                    "raw": {
                        "oracle": { "kind": "inline", "value": "accepted" },
                        "subject": { "kind": "inline", "value": "accepted" }
                    },
                    "normalized": {
                        "oracle": "accepted",
                        "subject": "accepted"
                    },
                    "normalization_rules": [],
                    "status": "blocked-legal"
                });
            })
            .is_some();
        assert!(replaced);

        let result = serde_json::from_value::<ConformanceRecord>(blocked_observation);

        assert!(result.is_err());
    }

    #[test]
    fn conformance_record_rejects_unknown_rules_and_duplicate_environment_names()
    -> Result<(), serde_json::Error> {
        let mut unknown_rule_document = valid_conformance_document();
        let replaced = unknown_rule_document
            .pointer_mut("/observations/result")
            .map(|observation| {
                *observation = json!({
                    "state": "observed",
                    "raw": {
                        "oracle": { "kind": "inline", "value": 1 },
                        "subject": { "kind": "inline", "value": 1 }
                    },
                    "normalized": { "oracle": 1, "subject": 1 },
                    "normalization_rules": [
                        { "id": "normalize.unknown", "revision": 1 }
                    ],
                    "status": "compatible"
                });
            })
            .is_some();
        assert!(replaced);
        let unknown_rule: ConformanceRecord = serde_json::from_value(unknown_rule_document)?;
        let unknown_rule_violations = unknown_rule.validate_document_semantics();

        let mut duplicate_environment_document = valid_conformance_document();
        let environment = duplicate_environment_document
            .pointer_mut("/reproduction/environment")
            .and_then(Value::as_array_mut);
        assert!(environment.is_some());
        if let Some(environment) = environment {
            environment.push(json!({
                "name": "subject.architecture",
                "value": "different"
            }));
        }
        let duplicate_environment: ConformanceRecord =
            serde_json::from_value(duplicate_environment_document)?;
        let duplicate_environment_violations = duplicate_environment.validate_document_semantics();

        assert!(
            unknown_rule_violations.iter().any(|violation| {
                violation.code == "conformance.normalization-reference.unknown"
            })
        );
        assert!(
            duplicate_environment_violations
                .iter()
                .any(|violation| { violation.code == "conformance.environment.name.duplicate" })
        );
        Ok(())
    }

    #[test]
    fn conformance_record_resolves_feature_owner_target_and_provenance()
    -> Result<(), serde_json::Error> {
        let record: ConformanceRecord = serde_json::from_value(valid_conformance_document())?;
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
                evidence: vec!["prov-public-specification".to_owned()],
                differences: Vec::new(),
                owner_issue: 8,
                legal_review_id: None,
            }],
        };
        let provenance = provenance_ledger();

        let valid = record.validate_references(&targets, &features, &provenance);
        let mut blocked_features = features.clone();
        blocked_features.features[0].status = CompatibilityStatus::BlockedLegal;
        blocked_features.features[0].legal_review_id =
            Some("legal-review-public-specification".to_owned());
        let blocked_references =
            record.validate_references(&targets, &blocked_features, &provenance);
        let mut duplicate_features = features.clone();
        duplicate_features
            .features
            .push(blocked_features.features[0].clone());
        let duplicate_feature_references =
            record.validate_references(&targets, &duplicate_features, &provenance);
        let mut duplicate_targets = targets.clone();
        duplicate_targets
            .targets
            .push(oracle_target("baseline", "2022-CU26-ubuntu-22.04"));
        let duplicate_target_references =
            record.validate_references(&duplicate_targets, &features, &provenance);
        let mut wrong_owner = record.clone();
        wrong_owner.owner_issue = 9;
        let mut unknown_feature = record.clone();
        unknown_feature.feature_id = "language.unknown".to_owned();
        let mut unknown_target = record.clone();
        unknown_target.target_id = "target.unknown".to_owned();
        let mut unknown_provenance = record;
        unknown_provenance.provenance_id = "prov-unknown".to_owned();

        assert!(valid.is_empty(), "{valid:#?}");
        assert!(
            blocked_references
                .iter()
                .any(|violation| violation.code == "conformance.feature.blocked-legal")
        );
        assert!(
            duplicate_feature_references
                .iter()
                .any(|violation| violation.code == "conformance.feature.ambiguous")
        );
        assert!(
            duplicate_target_references
                .iter()
                .any(|violation| violation.code == "conformance.target.ambiguous")
        );
        assert!(
            wrong_owner
                .validate_references(&targets, &features, &provenance)
                .iter()
                .any(|violation| violation.code == "conformance.feature.owner-mismatch")
        );
        assert!(
            unknown_feature
                .validate_references(&targets, &features, &provenance)
                .iter()
                .any(|violation| violation.code == "conformance.feature.unknown")
        );
        assert!(
            unknown_target
                .validate_references(&targets, &features, &provenance)
                .iter()
                .any(|violation| violation.code == "conformance.target.unknown")
        );
        assert!(
            unknown_provenance
                .validate_references(&targets, &features, &provenance)
                .iter()
                .any(|violation| violation.code == "conformance.provenance.unknown")
        );
        Ok(())
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
    fn behavior_admission_references_resolve_exactly() -> Result<(), serde_json::Error> {
        let contracts = approved_behavior_admission_contracts()?;

        let violations = contracts.admissions.validate_references(
            &contracts.targets,
            &contracts.features,
            &contracts.provenance,
            &contracts.legal_reviews,
        );

        assert!(violations.is_empty(), "{violations:#?}");
        Ok(())
    }

    #[test]
    fn behavior_admission_rejects_cross_ledger_drift() -> Result<(), serde_json::Error> {
        let mut contracts = approved_behavior_admission_contracts()?;
        contracts.features.features[0].oracle_targets[0] = "different-target".to_owned();
        if let Some(record) = contracts
            .provenance
            .records
            .iter_mut()
            .find(|record| record.id == "prov-synthetic-specification")
        {
            record.content_digest = format!("sha256:{}", "e".repeat(64));
        }
        contracts.admissions.admissions[0].derived_tests[0]
            .parent_provenance_ids
            .clear();
        contracts.admissions.admissions[0].legal_review_ids[0] =
            "different-legal-review".to_owned();

        let violations = contracts.admissions.validate_references(
            &contracts.targets,
            &contracts.features,
            &contracts.provenance,
            &contracts.legal_reviews,
        );

        for code in [
            "behavior-admission.feature.target-mismatch",
            "behavior-admission.specification.provenance-mismatch",
            "behavior-admission.derived-test.specification-parent-missing",
            "behavior-admission.legal-review-set.mismatch",
            "behavior-admission.legal-review.unknown",
        ] {
            assert!(
                violations.iter().any(|violation| violation.code == code),
                "missing {code}: {violations:#?}"
            );
        }
        Ok(())
    }

    #[test]
    fn implementation_admission_requires_protected_specification_review_authority()
    -> Result<(), serde_json::Error> {
        let contracts = approved_behavior_admission_contracts()?;
        let authority = legal_decision_authority_for_provenance(
            &contracts.legal_reviews.reviews[0],
            &contracts.provenance,
        );

        let violations = contracts.features.validate_implementation_inputs(
            "language.query.select",
            "target.synthetic",
            ImplementationAdmissionContext {
                targets: &contracts.targets,
                admissions: &contracts.admissions,
                provenance: &contracts.provenance,
                legal_reviews: &contracts.legal_reviews,
                legal_verification: Some(legal_decision_verification(&authority)),
            },
        );

        assert!(
            violations.iter().any(|violation| {
                violation.code == "behavior-admission.review-authority.required"
            }),
            "{violations:#?}"
        );
        Ok(())
    }

    #[test]
    fn implementation_admission_validates_every_governance_ledger() -> Result<(), serde_json::Error>
    {
        let mut contracts = approved_behavior_admission_contracts()?;
        contracts.targets.schema_version = "invalid".to_owned();
        contracts.features.schema_version = "invalid".to_owned();
        contracts.provenance.schema_version = "invalid".to_owned();
        contracts.legal_reviews.schema_version = "invalid".to_owned();

        let violations = contracts.features.validate_implementation_inputs(
            "language.query.select",
            "target.synthetic",
            ImplementationAdmissionContext {
                targets: &contracts.targets,
                admissions: &contracts.admissions,
                provenance: &contracts.provenance,
                legal_reviews: &contracts.legal_reviews,
                legal_verification: None,
            },
        );

        for code in [
            "target.schema-version.unsupported",
            "feature.schema-version.unsupported",
            "provenance.schema-version.unsupported",
            "legal-review.schema-version.unsupported",
        ] {
            assert!(
                violations.iter().any(|violation| violation.code == code),
                "missing {code}: {violations:#?}"
            );
        }
        Ok(())
    }

    #[test]
    fn implementation_admission_rejects_legal_approval_after_observation()
    -> Result<(), serde_json::Error> {
        let mut contracts = approved_behavior_admission_contracts()?;
        contracts.legal_reviews.reviews[0].decided_on = Some("2026-01-01".to_owned());
        let authority = legal_decision_authority_for_provenance(
            &contracts.legal_reviews.reviews[0],
            &contracts.provenance,
        );

        let violations = contracts.features.validate_implementation_inputs(
            "language.query.select",
            "target.synthetic",
            ImplementationAdmissionContext {
                targets: &contracts.targets,
                admissions: &contracts.admissions,
                provenance: &contracts.provenance,
                legal_reviews: &contracts.legal_reviews,
                legal_verification: Some(legal_decision_verification(&authority)),
            },
        );

        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "behavior-admission.legal-review.not-prior"),
            "{violations:#?}"
        );
        Ok(())
    }

    #[test]
    fn implementation_admission_uses_legal_review_last_edit_time() -> Result<(), serde_json::Error>
    {
        let contracts = approved_behavior_admission_contracts()?;
        let mut authority = legal_decision_authority_for_provenance(
            &contracts.legal_reviews.reviews[0],
            &contracts.provenance,
        );
        authority.pull_requests[0].authenticated_reviews[0].last_edited_at =
            Some("2026-01-01T01:00:00Z".to_owned());

        let violations = contracts.features.validate_implementation_inputs(
            "language.query.select",
            "target.synthetic",
            ImplementationAdmissionContext {
                targets: &contracts.targets,
                admissions: &contracts.admissions,
                provenance: &contracts.provenance,
                legal_reviews: &contracts.legal_reviews,
                legal_verification: Some(legal_decision_verification(&authority)),
            },
        );

        assert!(violations.iter().any(|violation| {
            violation.code == "behavior-admission.legal-review.not-prior"
                && violation.message.contains("observation")
        }));
        Ok(())
    }

    #[test]
    fn legal_review_chronology_uses_each_artifact_boundary() -> Result<(), serde_json::Error> {
        let mut contracts = approved_behavior_admission_contracts()?;
        contracts.legal_reviews.reviews[0].decided_on = Some("2026-01-01".to_owned());
        let mut authority = legal_decision_authority_for_provenance(
            &contracts.legal_reviews.reviews[0],
            &contracts.provenance,
        );
        authority.pull_requests[0].authenticated_reviews[0].submitted_at =
            "2026-01-01T01:03:30Z".to_owned();
        let context = ImplementationAdmissionContext {
            targets: &contracts.targets,
            admissions: &contracts.admissions,
            provenance: &contracts.provenance,
            legal_reviews: &contracts.legal_reviews,
            legal_verification: Some(legal_decision_verification(&authority)),
        };

        let derived_test_violations = super::validate_admission_closure_uses(
            context,
            "admission.synthetic.select",
            "prov-synthetic-test",
            ProvenanceUse::ConformanceEvidence,
            Some(super::AdmissionUseDeadline {
                timestamp: "2026-01-01T01:04:00Z",
                boundary: "implementation handoff",
            }),
        );
        assert!(
            !derived_test_violations
                .iter()
                .any(|violation| violation.code == "behavior-admission.legal-review.not-prior"),
            "{derived_test_violations:#?}"
        );

        let source_violations = super::validate_admission_closure_uses(
            context,
            "admission.synthetic.select",
            "prov-synthetic-source",
            ProvenanceUse::ImplementationInput,
            Some(super::AdmissionUseDeadline {
                timestamp: "2026-01-01T01:00:00Z",
                boundary: "observation",
            }),
        );
        assert!(source_violations.iter().any(|violation| {
            violation.code == "behavior-admission.legal-review.not-prior"
                && violation.message.contains("observation")
        }));
        Ok(())
    }

    #[test]
    fn implementation_admission_validates_governed_use_across_provenance_ancestors()
    -> Result<(), serde_json::Error> {
        let mut target_contracts = approved_behavior_admission_contracts()?;
        let target = target_contracts
            .provenance
            .records
            .iter_mut()
            .find(|record| record.id == "prov-synthetic-target");
        assert!(target.is_some());
        if let Some(target) = target {
            target.parent_provenance_ids = vec!["prov-synthetic-source".to_owned()];
        }
        let target_authority = legal_decision_authority_for_provenance(
            &target_contracts.legal_reviews.reviews[0],
            &target_contracts.provenance,
        );

        let target_violations = target_contracts.features.validate_implementation_inputs(
            "language.query.select",
            "target.synthetic",
            ImplementationAdmissionContext {
                targets: &target_contracts.targets,
                admissions: &target_contracts.admissions,
                provenance: &target_contracts.provenance,
                legal_reviews: &target_contracts.legal_reviews,
                legal_verification: Some(legal_decision_verification(&target_authority)),
            },
        );

        assert!(target_violations.iter().any(|violation| {
            violation.code == "provenance.use.undeclared"
                && violation.message.contains("prov-synthetic-source")
                && violation.message.contains("OracleOperation")
        }));

        let mut test_contracts = approved_behavior_admission_contracts()?;
        let source = test_contracts
            .provenance
            .records
            .iter_mut()
            .find(|record| record.id == "prov-synthetic-source");
        assert!(source.is_some());
        if let Some(source) = source {
            source
                .intended_uses
                .retain(|use_kind| *use_kind != ProvenanceUse::ConformanceEvidence);
        }
        let test_authority = legal_decision_authority_for_provenance(
            &test_contracts.legal_reviews.reviews[0],
            &test_contracts.provenance,
        );

        let test_violations = test_contracts.features.validate_implementation_inputs(
            "language.query.select",
            "target.synthetic",
            ImplementationAdmissionContext {
                targets: &test_contracts.targets,
                admissions: &test_contracts.admissions,
                provenance: &test_contracts.provenance,
                legal_reviews: &test_contracts.legal_reviews,
                legal_verification: Some(legal_decision_verification(&test_authority)),
            },
        );

        assert!(test_violations.iter().any(|violation| {
            violation.code == "provenance.use.undeclared"
                && violation.message.contains("prov-synthetic-source")
                && violation.message.contains("ConformanceEvidence")
        }));
        Ok(())
    }

    #[test]
    fn pending_behavior_admission_cannot_authorize_implementation() -> Result<(), serde_json::Error>
    {
        let mut contracts = approved_behavior_admission_contracts()?;
        let admission = &mut contracts.admissions.admissions[0];
        admission.specification.technical_review.status = SpecificationReviewStatus::Pending;
        admission.specification.technical_review.reviewed_by = None;
        admission.specification.technical_review.decided_at = None;
        admission.specification.technical_review.decision_evidence = None;
        admission.implementation_handoff = None;
        admission.derived_tests.clear();
        let legal_review = &mut contracts.legal_reviews.reviews[0];
        legal_review.status = LegalReviewStatus::Pending;
        legal_review.approved_uses.clear();
        legal_review.reviewed_by = None;
        legal_review.decided_on = None;
        legal_review.decision_evidence = None;

        let violations = contracts.features.validate_implementation_inputs(
            "language.query.select",
            "target.synthetic",
            ImplementationAdmissionContext {
                targets: &contracts.targets,
                admissions: &contracts.admissions,
                provenance: &contracts.provenance,
                legal_reviews: &contracts.legal_reviews,
                legal_verification: None,
            },
        );

        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "behavior-admission.review.pending")
        );
        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "provenance.legal-review.pending")
        );
        Ok(())
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
        let targets = TargetMatrix {
            schema_version: "1.0.0".to_owned(),
            baseline_target_id: "baseline".to_owned(),
            targets: vec![oracle_target("baseline", "2022-CU26-ubuntu-22.04")],
            expansion_order: Vec::new(),
        };
        let admissions = BehaviorSpecificationAdmissionLedger {
            schema_version: "1.0.0".to_owned(),
            admissions: Vec::new(),
        };

        let violations = features.validate_implementation_inputs(
            "storage.native-file-formats",
            "baseline",
            ImplementationAdmissionContext {
                targets: &targets,
                admissions: &admissions,
                provenance: &provenance_ledger(),
                legal_reviews: &pending_legal_reviews(),
                legal_verification: None,
            },
        );

        assert!(
            violations
                .iter()
                .any(|violation| violation.code == "feature.implementation.blocked-legal")
        );
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

    fn valid_conformance_document() -> Value {
        json!({
            "schema_version": "2.0.0",
            "case_id": "select.literal.integer",
            "feature_id": "language.select",
            "owner_issue": 8,
            "target_id": "baseline",
            "observed_at": "2026-08-02T00:00:00Z",
            "provenance_id": "prov-public-specification",
            "behavior_class": "documented",
            "reproduction": {
                "runner_id": "ntsql-testkit.synthetic",
                "runner_revision": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "runner_digest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "subject_revision": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "subject_digest": "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "case_seed": "seed.select-literal-integer",
                "input_digest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                "environment": [
                    { "name": "subject.operating-system", "value": "synthetic" },
                    { "name": "subject.architecture", "value": "synthetic" }
                ],
                "arguments": ["--case", "select.literal.integer"]
            },
            "normalization_rules": [],
            "observations": {
                "syntax": { "state": "not-observed", "reason": "pending" },
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
        })
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

    fn approved_behavior_admission_contracts()
    -> Result<BehaviorAdmissionContracts, serde_json::Error> {
        let corpus: Value = serde_json::from_str(include_str!(
            "../../../contracts/schema-corpus/behavior-specification-admission-ledger.json"
        ))?;
        let admissions = serde_json::from_value(corpus["base_document"]["inline"].clone())?;
        let legal_review_id = "legal-review-synthetic";
        let source = ProvenanceRecord {
            id: "prov-synthetic-source".to_owned(),
            source_kind: ProvenanceSourceKind::OpenSpecification,
            title: "Synthetic source".to_owned(),
            source_url: Some("https://example.invalid/synthetic-source".to_owned()),
            artifact_path: None,
            revision: "synthetic".to_owned(),
            retrieved_on: "2026-01-01".to_owned(),
            author: "ntsql tests".to_owned(),
            generation_method: "Repository-authored synthetic test data".to_owned(),
            environment: None,
            license: "Synthetic".to_owned(),
            content_digest: format!("sha256:{}", "a".repeat(64)),
            intended_uses: vec![
                ProvenanceUse::ImplementationInput,
                ProvenanceUse::ConformanceEvidence,
            ],
            parent_provenance_ids: Vec::new(),
            legal_review_id: legal_review_id.to_owned(),
        };
        let target_provenance = ProvenanceRecord {
            id: "prov-synthetic-target".to_owned(),
            source_kind: ProvenanceSourceKind::OracleObservation,
            title: "Synthetic target".to_owned(),
            source_url: None,
            artifact_path: Some("contracts/synthetic-target.json".to_owned()),
            revision: "synthetic".to_owned(),
            retrieved_on: "2026-01-01".to_owned(),
            author: "ntsql tests".to_owned(),
            generation_method: "Repository-authored synthetic test data".to_owned(),
            environment: Some("synthetic".to_owned()),
            license: "Synthetic".to_owned(),
            content_digest: format!("sha256:{}", "b".repeat(64)),
            intended_uses: vec![ProvenanceUse::OracleOperation],
            parent_provenance_ids: Vec::new(),
            legal_review_id: legal_review_id.to_owned(),
        };
        let specification = ProvenanceRecord {
            id: "prov-synthetic-specification".to_owned(),
            source_kind: ProvenanceSourceKind::BehaviorSpecification,
            title: "Synthetic behavior specification".to_owned(),
            source_url: None,
            artifact_path: Some("specifications/synthetic-select.json".to_owned()),
            revision: "synthetic".to_owned(),
            retrieved_on: "2026-01-01".to_owned(),
            author: "ntsql tests".to_owned(),
            generation_method: "Repository-authored synthetic test data".to_owned(),
            environment: None,
            license: "Synthetic".to_owned(),
            content_digest: format!("sha256:{}", "c".repeat(64)),
            intended_uses: vec![
                ProvenanceUse::ImplementationInput,
                ProvenanceUse::ConformanceEvidence,
            ],
            parent_provenance_ids: vec![source.id.clone()],
            legal_review_id: legal_review_id.to_owned(),
        };
        let derived_test = ProvenanceRecord {
            id: "prov-synthetic-test".to_owned(),
            source_kind: ProvenanceSourceKind::Test,
            title: "Synthetic derived test".to_owned(),
            source_url: None,
            artifact_path: Some("tests/synthetic-select.rs".to_owned()),
            revision: "synthetic".to_owned(),
            retrieved_on: "2026-01-01".to_owned(),
            author: "ntsql tests".to_owned(),
            generation_method: "Repository-authored synthetic test data".to_owned(),
            environment: None,
            license: "Synthetic".to_owned(),
            content_digest: format!("sha256:{}", "d".repeat(64)),
            intended_uses: vec![ProvenanceUse::ConformanceEvidence],
            parent_provenance_ids: vec![specification.id.clone()],
            legal_review_id: legal_review_id.to_owned(),
        };
        let provenance = ProvenanceLedger {
            schema_version: "1.0.0".to_owned(),
            records: vec![source, target_provenance, specification, derived_test],
        };
        let legal_reviews = LegalReviewLedger {
            schema_version: "2.0.0".to_owned(),
            reviews: vec![LegalReviewRecord {
                id: legal_review_id.to_owned(),
                subject: "Synthetic governed uses".to_owned(),
                status: LegalReviewStatus::Approved,
                approved_uses: vec![
                    ProvenanceUse::ImplementationInput,
                    ProvenanceUse::OracleOperation,
                    ProvenanceUse::ConformanceEvidence,
                ],
                prohibited_uses: Vec::new(),
                individual_review_uses: Vec::new(),
                source_provenance_ids: provenance
                    .records
                    .iter()
                    .map(|record| record.id.clone())
                    .collect(),
                reviewed_by: Some(reviewer_identity()),
                decided_on: Some("2025-12-31".to_owned()),
                decision_evidence: Some(decision_evidence_reference()),
                rationale: "Synthetic schema and validation test only".to_owned(),
            }],
        };
        let mut target = oracle_target("target.synthetic", "2022-CU26-synthetic");
        target.provenance_id = "prov-synthetic-target".to_owned();
        let targets = TargetMatrix {
            schema_version: "1.0.0".to_owned(),
            baseline_target_id: target.id.clone(),
            targets: vec![target],
            expansion_order: Vec::new(),
        };
        let mut features = FeatureMatrix {
            schema_version: "1.0.0".to_owned(),
            features: vec![FeatureRecord {
                id: "language.query.select".to_owned(),
                title: "Synthetic SELECT".to_owned(),
                category: FeatureCategory::Language,
                status: CompatibilityStatus::NotTested,
                oracle_targets: vec!["target.synthetic".to_owned()],
                evidence: vec!["prov-synthetic-specification".to_owned()],
                differences: Vec::new(),
                owner_issue: 8,
                legal_review_id: None,
            }],
        };
        features.features.extend(
            FEATURE_CATEGORIES
                .into_iter()
                .filter(|category| *category != FeatureCategory::Language)
                .enumerate()
                .map(|(index, category)| FeatureRecord {
                    id: format!("synthetic.category.{index}"),
                    title: format!("Synthetic category {index}"),
                    category,
                    status: CompatibilityStatus::NotTested,
                    oracle_targets: vec!["target.synthetic".to_owned()],
                    evidence: vec!["prov-synthetic-specification".to_owned()],
                    differences: Vec::new(),
                    owner_issue: 54,
                    legal_review_id: None,
                }),
        );

        Ok(BehaviorAdmissionContracts {
            admissions,
            targets,
            features,
            provenance,
            legal_reviews,
        })
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
        let submitted_at = decision.decided_on.as_deref().map_or_else(
            || "2026-08-02T12:34:56Z".to_owned(),
            |decided_on| format!("{decided_on}T12:34:56Z"),
        );
        LegalDecisionAuthority {
            schema_version: LEGAL_DECISION_AUTHORITY_SCHEMA_VERSION.to_owned(),
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
                    submitted_at,
                    last_edited_at: None,
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
