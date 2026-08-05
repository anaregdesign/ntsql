//! Oracle-free orchestration for deterministic synthetic conformance cases.

use std::{error::Error, fmt};

use ntsql_contract::{
    BehaviorClass, CONFORMANCE_SCHEMA_VERSION, ComparisonStatus, CompatibilityDimension,
    ConformanceObservations, ConformanceRecord, ConformanceReproduction, ContractViolation,
    DimensionObservation, FeatureMatrix, JsonValue, NormalizationRule, NormalizationRuleReference,
    NormalizedObservationPair, ProvenanceLedger, RawEvidence, RawObservationPair, TargetMatrix,
    json_values_are_identical,
};

/// Verifies that exact input bytes match a recorded canonical SHA-256 identity.
///
/// The core owns the ordering guarantee but not a cryptographic implementation.
/// Implementations are injected so this crate does not perform I/O or introduce
/// an unreviewed hashing dependency.
pub trait InputDigestVerifier {
    /// Verifier-specific failure that must remain distinct from a mismatch.
    type Error: fmt::Display;

    /// Returns whether `input` has the recorded `expected_digest`.
    fn matches(&self, input: &[u8], expected_digest: &str) -> Result<bool, Self::Error>;
}

/// One synthetic observation before it enters the conformance contract.
#[derive(Clone, Debug, PartialEq)]
pub struct SyntheticObservation {
    /// Lossless inline raw value.
    pub raw: JsonValue,
    /// Value produced by the explicitly recorded normalization rules.
    pub normalized: JsonValue,
}

/// In-memory source of one observation per requested compatibility dimension.
pub trait ObservationSource {
    /// Source-specific failure that can be retained by the runner.
    type Error: fmt::Display;

    /// Observes one dimension for the exact case input.
    fn observe(
        &self,
        dimension: CompatibilityDimension,
        input: &[u8],
    ) -> Result<SyntheticObservation, Self::Error>;
}

/// Whether one dimension is compared or explicitly omitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DimensionPlan {
    /// Compare both sources using these ordered normalization rules.
    Compare {
        /// Ordered rule identities applied by both sources.
        normalization_rules: Vec<NormalizationRuleReference>,
    },
    /// Do not call either source for this dimension.
    NotObserved {
        /// Nonempty explanation retained in the conformance record.
        reason: String,
    },
}

/// Explicit plan for every required compatibility dimension.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DimensionPlans {
    /// Syntax acceptance and parsing.
    pub syntax: DimensionPlan,
    /// Client-visible protocol behavior.
    pub wire: DimensionPlan,
    /// Values and row ordering.
    pub result: DimensionPlan,
    /// Catalog and result metadata.
    pub metadata: DimensionPlan,
    /// Client-visible diagnostics.
    pub diagnostic: DimensionPlan,
    /// Transactional side effects.
    pub transactional_side_effect: DimensionPlan,
    /// Startup, configuration, and administration.
    pub operational: DimensionPlan,
}

/// Complete metadata and plan for one deterministic synthetic run.
#[derive(Clone, Debug, PartialEq)]
pub struct SyntheticCase {
    /// Stable conformance case identifier.
    pub case_id: String,
    /// Exact target ledger identifier.
    pub target_id: String,
    /// Feature ledger identifier.
    pub feature_id: String,
    /// GitHub issue that owns this case.
    pub owner_issue: u64,
    /// Authority classification for the expected behavior.
    pub behavior_class: BehaviorClass,
    /// Provenance record supporting this case.
    pub provenance_id: String,
    /// Explicit observation timestamp supplied by the caller.
    pub observed_at: String,
    /// Complete deterministic rerun metadata, including input identity.
    pub reproduction: ConformanceReproduction,
    /// Complete definitions for rules referenced by dimension plans.
    pub normalization_rules: Vec<NormalizationRule>,
    /// Plan for all seven compatibility dimensions.
    pub dimensions: DimensionPlans,
}

/// Independently maintained ledgers used to validate generated references.
#[derive(Clone, Copy, Debug)]
pub struct ContractLedgers<'a> {
    /// Exact compatibility targets.
    pub targets: &'a TargetMatrix,
    /// Feature inventory and ownership.
    pub features: &'a FeatureMatrix,
    /// Provenance records referenced by the case and normalization rules.
    pub provenance: &'a ProvenanceLedger,
}

/// Identifies the source side that failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationSide {
    /// The synthetic reference implementation.
    Reference,
    /// The synthetic subject implementation.
    Subject,
}

/// Failure to produce a validated synthetic conformance record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunError {
    /// Exact input bytes did not match the recorded input identity.
    InputDigestMismatch {
        /// Expected canonical SHA-256 identity.
        expected_digest: String,
    },
    /// The injected input verifier could not determine the identity.
    InputDigestVerification {
        /// Verifier-provided diagnostic retained without reinterpretation.
        message: String,
    },
    /// One observation source failed.
    Source {
        /// Source side that failed.
        side: ObservationSide,
        /// Dimension requested when the source failed.
        dimension: CompatibilityDimension,
        /// Source-provided diagnostic retained without reinterpretation.
        message: String,
    },
    /// The generated record violated its contract or ledger references.
    InvalidRecord {
        /// Complete validation failures.
        violations: Vec<ContractViolation>,
    },
}

impl fmt::Display for RunError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputDigestMismatch { expected_digest } => write!(
                formatter,
                "input bytes do not match recorded digest {expected_digest}"
            ),
            Self::InputDigestVerification { message } => {
                write!(formatter, "input digest verification failed: {message}")
            }
            Self::Source {
                side,
                dimension,
                message,
            } => write!(
                formatter,
                "{side:?} source failed for {dimension:?}: {message}"
            ),
            Self::InvalidRecord { violations } => write!(
                formatter,
                "synthetic conformance record validation failed with {} violation(s)",
                violations.len()
            ),
        }
    }
}

impl Error for RunError {}

/// A conformance record promoted only after all local contract checks pass.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedConformanceRecord(ConformanceRecord);

impl ValidatedConformanceRecord {
    /// Borrows the validated wire-contract value.
    #[must_use]
    pub fn record(&self) -> &ConformanceRecord {
        &self.0
    }

    /// Consumes the typestate and returns the wire-contract value.
    #[must_use]
    pub fn into_record(self) -> ConformanceRecord {
        self.0
    }
}

/// Runs the same exact input against two synthetic sources in canonical order.
///
/// Input identity is checked before either source is called. Every compared
/// dimension is classified solely from the contract-owned exact JSON identity
/// relation; this minimal runner never infers `partial`.
pub fn run_synthetic_case<D, R, S>(
    case: &SyntheticCase,
    input: &[u8],
    digest_verifier: &D,
    reference: &R,
    subject: &S,
    ledgers: ContractLedgers<'_>,
) -> Result<ValidatedConformanceRecord, RunError>
where
    D: InputDigestVerifier,
    R: ObservationSource,
    S: ObservationSource,
{
    let input_matches = digest_verifier
        .matches(input, &case.reproduction.input_digest)
        .map_err(|error| RunError::InputDigestVerification {
            message: error.to_string(),
        })?;
    if !input_matches {
        return Err(RunError::InputDigestMismatch {
            expected_digest: case.reproduction.input_digest.clone(),
        });
    }

    let observations = ConformanceObservations {
        syntax: run_dimension(
            CompatibilityDimension::Syntax,
            &case.dimensions.syntax,
            input,
            reference,
            subject,
        )?,
        wire: run_dimension(
            CompatibilityDimension::Wire,
            &case.dimensions.wire,
            input,
            reference,
            subject,
        )?,
        result: run_dimension(
            CompatibilityDimension::Result,
            &case.dimensions.result,
            input,
            reference,
            subject,
        )?,
        metadata: run_dimension(
            CompatibilityDimension::Metadata,
            &case.dimensions.metadata,
            input,
            reference,
            subject,
        )?,
        diagnostic: run_dimension(
            CompatibilityDimension::Diagnostic,
            &case.dimensions.diagnostic,
            input,
            reference,
            subject,
        )?,
        transactional_side_effect: run_dimension(
            CompatibilityDimension::TransactionalSideEffect,
            &case.dimensions.transactional_side_effect,
            input,
            reference,
            subject,
        )?,
        operational: run_dimension(
            CompatibilityDimension::Operational,
            &case.dimensions.operational,
            input,
            reference,
            subject,
        )?,
    };

    let record = ConformanceRecord {
        schema_version: CONFORMANCE_SCHEMA_VERSION.to_owned(),
        case_id: case.case_id.clone(),
        target_id: case.target_id.clone(),
        feature_id: case.feature_id.clone(),
        owner_issue: case.owner_issue,
        behavior_class: case.behavior_class,
        provenance_id: case.provenance_id.clone(),
        observed_at: case.observed_at.clone(),
        reproduction: case.reproduction.clone(),
        normalization_rules: case.normalization_rules.clone(),
        observations,
    };

    let mut violations = record.validate_schema_semantics();
    violations.extend(record.validate_document_semantics());
    violations.extend(record.validate_references(
        ledgers.targets,
        ledgers.features,
        ledgers.provenance,
    ));
    if violations.is_empty() {
        Ok(ValidatedConformanceRecord(record))
    } else {
        Err(RunError::InvalidRecord { violations })
    }
}

fn run_dimension<R, S>(
    dimension: CompatibilityDimension,
    plan: &DimensionPlan,
    input: &[u8],
    reference: &R,
    subject: &S,
) -> Result<DimensionObservation, RunError>
where
    R: ObservationSource,
    S: ObservationSource,
{
    let normalization_rules = match plan {
        DimensionPlan::NotObserved { reason } => {
            return Ok(DimensionObservation::NotObserved {
                reason: reason.clone(),
            });
        }
        DimensionPlan::Compare {
            normalization_rules,
        } => normalization_rules,
    };

    let reference_observation =
        reference
            .observe(dimension, input)
            .map_err(|error| RunError::Source {
                side: ObservationSide::Reference,
                dimension,
                message: error.to_string(),
            })?;
    let subject_observation =
        subject
            .observe(dimension, input)
            .map_err(|error| RunError::Source {
                side: ObservationSide::Subject,
                dimension,
                message: error.to_string(),
            })?;
    let status = if json_values_are_identical(
        &reference_observation.normalized,
        &subject_observation.normalized,
    ) {
        ComparisonStatus::Compatible
    } else {
        ComparisonStatus::Divergent
    };

    Ok(DimensionObservation::Observed {
        raw: Box::new(RawObservationPair {
            oracle: RawEvidence::Inline {
                value: reference_observation.raw,
            },
            subject: RawEvidence::Inline {
                value: subject_observation.raw,
            },
        }),
        normalized: NormalizedObservationPair {
            oracle: reference_observation.normalized,
            subject: subject_observation.normalized,
        },
        normalization_rules: normalization_rules.clone(),
        status,
    })
}
