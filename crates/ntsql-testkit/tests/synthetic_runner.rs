use std::cell::Cell;

use ntsql_contract::{
    BehaviorClass, COMPATIBILITY_SCHEMA_VERSION, ComparisonStatus, CompatibilityDimension,
    CompatibilityStatus, ConformanceEnvironmentFact, ConformanceReproduction, DimensionObservation,
    FeatureCategory, FeatureMatrix, FeatureRecord, JsonValue, NormalizationRule,
    NormalizationRuleReference, OracleTarget, ProvenanceLedger, ProvenanceRecord,
    ProvenanceSourceKind, ProvenanceUse, TargetMatrix,
};
use ntsql_testkit::{
    ContractLedgers, DimensionPlan, DimensionPlans, InputDigestVerifier, ObservationSide,
    ObservationSource, RunError, SyntheticCase, SyntheticObservation, ValidatedConformanceRecord,
    run_synthetic_case,
};

const INPUT: &[u8] = b"select 1";
const INPUT_DIGEST: &str =
    "sha256:822ae07d4783158bc1912bb623e5107cc9002d519e1143a9c200ed6ee18b6d0f";
const ARTIFACT_DIGEST: &str =
    "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const RUNNER_REVISION: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SUBJECT_REVISION: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

struct ExactDigestVerifier {
    expected_input: &'static [u8],
    calls: Cell<usize>,
}

impl InputDigestVerifier for ExactDigestVerifier {
    type Error = &'static str;

    fn matches(&self, input: &[u8], expected_digest: &str) -> Result<bool, Self::Error> {
        self.calls.set(self.calls.get() + 1);
        Ok(input == self.expected_input && expected_digest == INPUT_DIGEST)
    }
}

struct FailingDigestVerifier {
    calls: Cell<usize>,
}

impl InputDigestVerifier for FailingDigestVerifier {
    type Error = &'static str;

    fn matches(&self, _input: &[u8], _expected_digest: &str) -> Result<bool, Self::Error> {
        self.calls.set(self.calls.get() + 1);
        Err("digest adapter unavailable")
    }
}

struct RecordingSource {
    expected_input: &'static [u8],
    divergent_dimension: Option<CompatibilityDimension>,
    failure: Option<(CompatibilityDimension, &'static str)>,
    calls: Cell<usize>,
}

impl RecordingSource {
    fn matching() -> Self {
        Self {
            expected_input: INPUT,
            divergent_dimension: None,
            failure: None,
            calls: Cell::new(0),
        }
    }
}

impl ObservationSource for RecordingSource {
    type Error = &'static str;

    fn observe(
        &self,
        dimension: CompatibilityDimension,
        input: &[u8],
    ) -> Result<SyntheticObservation, Self::Error> {
        self.calls.set(self.calls.get() + 1);
        if input != self.expected_input {
            return Err("source received different input bytes");
        }
        if let Some((failed_dimension, message)) = self.failure
            && dimension == failed_dimension
        {
            return Err(message);
        }

        let mut value = dimension_name(dimension).to_owned();
        if self.divergent_dimension == Some(dimension) {
            value.push_str(".different");
        }
        let value = JsonValue::String(value);
        Ok(SyntheticObservation {
            raw: value.clone(),
            normalized: value,
        })
    }
}

struct NormalizingSource {
    raw: &'static str,
    calls: Cell<usize>,
}

impl ObservationSource for NormalizingSource {
    type Error = &'static str;

    fn observe(
        &self,
        _dimension: CompatibilityDimension,
        input: &[u8],
    ) -> Result<SyntheticObservation, Self::Error> {
        self.calls.set(self.calls.get() + 1);
        if input != INPUT {
            return Err("source received different input bytes");
        }
        Ok(SyntheticObservation {
            raw: JsonValue::String(self.raw.to_owned()),
            normalized: JsonValue::String("normalized".to_owned()),
        })
    }
}

#[test]
fn compatible_run_is_complete_and_deterministic() -> Result<(), RunError> {
    let verifier = ExactDigestVerifier {
        expected_input: INPUT,
        calls: Cell::new(0),
    };
    let reference = RecordingSource::matching();
    let subject = RecordingSource::matching();
    let case = synthetic_case(all_compared());

    let first = run(&case, &verifier, &reference, &subject)?;
    let second = run(&case, &verifier, &reference, &subject)?;

    assert_eq!(first, second);
    assert_eq!(verifier.calls.get(), 2);
    assert_eq!(reference.calls.get(), 14);
    assert_eq!(subject.calls.get(), 14);
    assert_all_compatible(first.record());
    Ok(())
}

#[test]
fn difference_is_divergent_and_omissions_are_explicit() -> Result<(), RunError> {
    let verifier = accepting_verifier();
    let reference = RecordingSource::matching();
    let subject = RecordingSource {
        divergent_dimension: Some(CompatibilityDimension::Result),
        ..RecordingSource::matching()
    };
    let case = synthetic_case(only_compared(CompatibilityDimension::Result));

    let validated = run(&case, &verifier, &reference, &subject)?;
    let record = validated.record();

    assert!(matches!(
        &record.observations.result,
        DimensionObservation::Observed {
            status: ComparisonStatus::Divergent,
            ..
        }
    ));
    assert!(matches!(
        &record.observations.syntax,
        DimensionObservation::NotObserved { reason }
            if reason == "outside this synthetic case"
    ));
    assert_eq!(reference.calls.get(), 1);
    assert_eq!(subject.calls.get(), 1);
    Ok(())
}

#[test]
fn normalization_rule_order_is_preserved() -> Result<(), RunError> {
    let verifier = accepting_verifier();
    let reference = NormalizingSource {
        raw: "reference raw",
        calls: Cell::new(0),
    };
    let subject = NormalizingSource {
        raw: "subject raw",
        calls: Cell::new(0),
    };
    let expected_rules = vec![
        NormalizationRuleReference {
            id: "normalize.second".to_owned(),
            revision: 2,
        },
        NormalizationRuleReference {
            id: "normalize.first".to_owned(),
            revision: 1,
        },
    ];
    let mut case = synthetic_case(only_compared(CompatibilityDimension::Syntax));
    case.normalization_rules = vec![
        NormalizationRule {
            id: "normalize.first".to_owned(),
            revision: 1,
            provenance_id: "prov-testkit".to_owned(),
            description: "First deterministic synthetic transformation.".to_owned(),
        },
        NormalizationRule {
            id: "normalize.second".to_owned(),
            revision: 2,
            provenance_id: "prov-testkit".to_owned(),
            description: "Second deterministic synthetic transformation.".to_owned(),
        },
    ];
    case.dimensions.syntax = DimensionPlan::Compare {
        normalization_rules: expected_rules.clone(),
    };

    let validated = run(&case, &verifier, &reference, &subject)?;

    assert!(matches!(
        &validated.record().observations.syntax,
        DimensionObservation::Observed {
            normalization_rules,
            status: ComparisonStatus::Compatible,
            ..
        } if normalization_rules == &expected_rules
    ));
    Ok(())
}

#[test]
fn digest_mismatch_stops_before_sources() {
    let verifier = ExactDigestVerifier {
        expected_input: b"different input",
        calls: Cell::new(0),
    };
    let reference = RecordingSource::matching();
    let subject = RecordingSource::matching();
    let case = synthetic_case(all_compared());

    let result = run(&case, &verifier, &reference, &subject);

    assert!(matches!(
        result,
        Err(RunError::InputDigestMismatch { expected_digest })
            if expected_digest == INPUT_DIGEST
    ));
    assert_eq!(verifier.calls.get(), 1);
    assert_eq!(reference.calls.get(), 0);
    assert_eq!(subject.calls.get(), 0);
}

#[test]
fn digest_verifier_failure_is_not_reported_as_a_mismatch() {
    let verifier = FailingDigestVerifier {
        calls: Cell::new(0),
    };
    let reference = RecordingSource::matching();
    let subject = RecordingSource::matching();
    let case = synthetic_case(all_compared());

    let result = run(&case, &verifier, &reference, &subject);

    assert!(matches!(
        result,
        Err(RunError::InputDigestVerification { message })
            if message == "digest adapter unavailable"
    ));
    assert_eq!(verifier.calls.get(), 1);
    assert_eq!(reference.calls.get(), 0);
    assert_eq!(subject.calls.get(), 0);
}

#[test]
fn source_failure_retains_side_and_dimension() {
    let verifier = accepting_verifier();
    let reference = RecordingSource::matching();
    let subject = RecordingSource {
        failure: Some((CompatibilityDimension::Diagnostic, "synthetic failure")),
        ..RecordingSource::matching()
    };
    let case = synthetic_case(only_compared(CompatibilityDimension::Diagnostic));

    let result = run(&case, &verifier, &reference, &subject);

    assert!(matches!(
        result,
        Err(RunError::Source {
            side: ObservationSide::Subject,
            dimension: CompatibilityDimension::Diagnostic,
            message,
        }) if message == "synthetic failure"
    ));
    assert_eq!(reference.calls.get(), 1);
    assert_eq!(subject.calls.get(), 1);
}

#[test]
fn reference_failure_prevents_subject_execution() {
    let verifier = accepting_verifier();
    let reference = RecordingSource {
        failure: Some((CompatibilityDimension::Wire, "reference failure")),
        ..RecordingSource::matching()
    };
    let subject = RecordingSource::matching();
    let case = synthetic_case(only_compared(CompatibilityDimension::Wire));

    let result = run(&case, &verifier, &reference, &subject);

    assert!(matches!(
        result,
        Err(RunError::Source {
            side: ObservationSide::Reference,
            dimension: CompatibilityDimension::Wire,
            message,
        }) if message == "reference failure"
    ));
    assert_eq!(reference.calls.get(), 1);
    assert_eq!(subject.calls.get(), 0);
}

#[test]
fn unknown_ledger_references_are_rejected() {
    let verifier = accepting_verifier();
    let reference = RecordingSource::matching();
    let subject = RecordingSource::matching();

    let mut unknown_target = synthetic_case(all_not_observed());
    unknown_target.target_id = "unknown-target".to_owned();
    assert_invalid_code(
        run(&unknown_target, &verifier, &reference, &subject),
        "conformance.target.unknown",
    );

    let mut unknown_feature = synthetic_case(all_not_observed());
    unknown_feature.feature_id = "unknown-feature".to_owned();
    assert_invalid_code(
        run(&unknown_feature, &verifier, &reference, &subject),
        "conformance.feature.unknown",
    );

    let mut unknown_provenance = synthetic_case(all_not_observed());
    unknown_provenance.provenance_id = "unknown-provenance".to_owned();
    assert_invalid_code(
        run(&unknown_provenance, &verifier, &reference, &subject),
        "conformance.provenance.unknown",
    );
    assert_eq!(reference.calls.get(), 0);
    assert_eq!(subject.calls.get(), 0);
}

fn run<D, R, S>(
    case: &SyntheticCase,
    verifier: &D,
    reference: &R,
    subject: &S,
) -> Result<ValidatedConformanceRecord, RunError>
where
    D: InputDigestVerifier,
    R: ObservationSource,
    S: ObservationSource,
{
    let (targets, features, provenance) = ledgers();
    run_synthetic_case(
        case,
        INPUT,
        verifier,
        reference,
        subject,
        ContractLedgers {
            targets: &targets,
            features: &features,
            provenance: &provenance,
        },
    )
}

fn accepting_verifier() -> ExactDigestVerifier {
    ExactDigestVerifier {
        expected_input: INPUT,
        calls: Cell::new(0),
    }
}

fn synthetic_case(dimensions: DimensionPlans) -> SyntheticCase {
    SyntheticCase {
        case_id: "synthetic.select-literal".to_owned(),
        target_id: "baseline".to_owned(),
        feature_id: "language.select".to_owned(),
        owner_issue: 44,
        behavior_class: BehaviorClass::Documented,
        provenance_id: "prov-testkit".to_owned(),
        observed_at: "2026-08-02T00:00:00Z".to_owned(),
        reproduction: ConformanceReproduction {
            runner_id: "ntsql-testkit.synthetic".to_owned(),
            runner_revision: RUNNER_REVISION.to_owned(),
            runner_digest: ARTIFACT_DIGEST.to_owned(),
            subject_revision: SUBJECT_REVISION.to_owned(),
            subject_digest: ARTIFACT_DIGEST.to_owned(),
            case_seed: "seed.select-literal".to_owned(),
            input_digest: INPUT_DIGEST.to_owned(),
            environment: vec![ConformanceEnvironmentFact {
                name: "subject.environment".to_owned(),
                value: "synthetic".to_owned(),
            }],
            arguments: vec!["synthetic.select-literal".to_owned()],
        },
        normalization_rules: Vec::new(),
        dimensions,
    }
}

fn all_compared() -> DimensionPlans {
    DimensionPlans {
        syntax: compared(),
        wire: compared(),
        result: compared(),
        metadata: compared(),
        diagnostic: compared(),
        transactional_side_effect: compared(),
        operational: compared(),
    }
}

fn all_not_observed() -> DimensionPlans {
    DimensionPlans {
        syntax: not_observed(),
        wire: not_observed(),
        result: not_observed(),
        metadata: not_observed(),
        diagnostic: not_observed(),
        transactional_side_effect: not_observed(),
        operational: not_observed(),
    }
}

fn only_compared(dimension: CompatibilityDimension) -> DimensionPlans {
    let mut plans = all_not_observed();
    let plan = compared();
    match dimension {
        CompatibilityDimension::Syntax => plans.syntax = plan,
        CompatibilityDimension::Wire => plans.wire = plan,
        CompatibilityDimension::Result => plans.result = plan,
        CompatibilityDimension::Metadata => plans.metadata = plan,
        CompatibilityDimension::Diagnostic => plans.diagnostic = plan,
        CompatibilityDimension::TransactionalSideEffect => {
            plans.transactional_side_effect = plan;
        }
        CompatibilityDimension::Operational => plans.operational = plan,
    }
    plans
}

fn compared() -> DimensionPlan {
    DimensionPlan::Compare {
        normalization_rules: Vec::new(),
    }
}

fn not_observed() -> DimensionPlan {
    DimensionPlan::NotObserved {
        reason: "outside this synthetic case".to_owned(),
    }
}

fn dimension_name(dimension: CompatibilityDimension) -> &'static str {
    match dimension {
        CompatibilityDimension::Syntax => "syntax",
        CompatibilityDimension::Wire => "wire",
        CompatibilityDimension::Result => "result",
        CompatibilityDimension::Metadata => "metadata",
        CompatibilityDimension::Diagnostic => "diagnostic",
        CompatibilityDimension::TransactionalSideEffect => "transactional-side-effect",
        CompatibilityDimension::Operational => "operational",
    }
}

fn assert_all_compatible(record: &ntsql_contract::ConformanceRecord) {
    for observation in [
        &record.observations.syntax,
        &record.observations.wire,
        &record.observations.result,
        &record.observations.metadata,
        &record.observations.diagnostic,
        &record.observations.transactional_side_effect,
        &record.observations.operational,
    ] {
        assert!(matches!(
            observation,
            DimensionObservation::Observed {
                status: ComparisonStatus::Compatible,
                ..
            }
        ));
    }
}

fn assert_invalid_code(
    result: Result<ValidatedConformanceRecord, RunError>,
    expected_code: &'static str,
) {
    assert!(matches!(
        result,
        Err(RunError::InvalidRecord { violations })
            if violations
                .iter()
                .any(|violation| violation.code == expected_code)
    ));
}

fn ledgers() -> (TargetMatrix, FeatureMatrix, ProvenanceLedger) {
    let targets = TargetMatrix {
        schema_version: COMPATIBILITY_SCHEMA_VERSION.to_owned(),
        baseline_target_id: "baseline".to_owned(),
        targets: vec![OracleTarget {
            id: "baseline".to_owned(),
            provenance_id: "prov-target".to_owned(),
            product_release: "synthetic".to_owned(),
            servicing_update: "synthetic".to_owned(),
            product_version: "1.0.0.0".to_owned(),
            edition: "synthetic".to_owned(),
            operating_system: "synthetic".to_owned(),
            architecture: "synthetic".to_owned(),
            container_repository: "synthetic".to_owned(),
            container_tag: "synthetic".to_owned(),
            container_digest: ARTIFACT_DIGEST.to_owned(),
            compatibility_level: 160,
            collation: "synthetic".to_owned(),
            language: "synthetic".to_owned(),
            lcid: 1033,
            timezone: "UTC".to_owned(),
            session_settings: vec!["synthetic".to_owned()],
        }],
        expansion_order: Vec::new(),
    };
    let features = FeatureMatrix {
        schema_version: COMPATIBILITY_SCHEMA_VERSION.to_owned(),
        features: vec![FeatureRecord {
            id: "language.select".to_owned(),
            title: "Synthetic SELECT".to_owned(),
            category: FeatureCategory::Language,
            status: CompatibilityStatus::NotTested,
            oracle_targets: vec!["baseline".to_owned()],
            evidence: vec!["prov-testkit".to_owned()],
            differences: Vec::new(),
            owner_issue: 44,
            legal_review_id: None,
        }],
    };
    let provenance = ProvenanceLedger {
        schema_version: COMPATIBILITY_SCHEMA_VERSION.to_owned(),
        records: vec![ProvenanceRecord {
            id: "prov-testkit".to_owned(),
            source_kind: ProvenanceSourceKind::Test,
            title: "Repository-authored synthetic test".to_owned(),
            source_url: None,
            artifact_path: Some("crates/ntsql-testkit/tests/synthetic_runner.rs".to_owned()),
            revision: "working-tree".to_owned(),
            retrieved_on: "2026-08-02".to_owned(),
            author: "ntsql contributors".to_owned(),
            generation_method: "Repository-authored synthetic construction.".to_owned(),
            environment: None,
            license: "Apache-2.0".to_owned(),
            content_digest: ARTIFACT_DIGEST.to_owned(),
            intended_uses: vec![ProvenanceUse::ConformanceEvidence],
            parent_provenance_ids: Vec::new(),
            legal_review_id: "legal-testkit".to_owned(),
        }],
    };
    (targets, features, provenance)
}
