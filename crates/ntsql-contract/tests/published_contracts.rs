use std::error::Error;

use ntsql_contract::{
    FeatureMatrix, LegalReviewLedger, ProvenanceLedger, TargetMatrix,
    validate_governance_references,
};
use serde_json::Value;

const FEATURES: &str = include_str!("../../../contracts/compatibility/features.json");
const LEGAL_REVIEWS: &str = include_str!("../../../contracts/compatibility/legal-reviews.json");
const PROVENANCE: &str = include_str!("../../../contracts/compatibility/provenance.json");
const TARGETS: &str = include_str!("../../../contracts/compatibility/targets.json");
const SCHEMAS: [(&str, &str); 5] = [
    (
        "conformance-record",
        include_str!("../../../contracts/schemas/conformance-record.schema.json"),
    ),
    (
        "feature-matrix",
        include_str!("../../../contracts/schemas/feature-matrix.schema.json"),
    ),
    (
        "legal-review-ledger",
        include_str!("../../../contracts/schemas/legal-review-ledger.schema.json"),
    ),
    (
        "provenance-ledger",
        include_str!("../../../contracts/schemas/provenance-ledger.schema.json"),
    ),
    (
        "target-matrix",
        include_str!("../../../contracts/schemas/target-matrix.schema.json"),
    ),
];

#[test]
fn published_target_matrix_is_valid() -> Result<(), Box<dyn Error>> {
    let targets: TargetMatrix = serde_json::from_str(TARGETS)?;
    let violations = targets.validate();

    assert!(violations.is_empty(), "{violations:#?}");
    Ok(())
}

#[test]
fn published_feature_matrix_is_valid() -> Result<(), Box<dyn Error>> {
    let targets: TargetMatrix = serde_json::from_str(TARGETS)?;
    let features: FeatureMatrix = serde_json::from_str(FEATURES)?;
    let violations = features.validate(&targets);

    assert!(violations.is_empty(), "{violations:#?}");
    Ok(())
}

#[test]
fn published_governance_ledgers_are_structurally_valid() -> Result<(), Box<dyn Error>> {
    let legal_reviews: LegalReviewLedger = serde_json::from_str(LEGAL_REVIEWS)?;
    let provenance: ProvenanceLedger = serde_json::from_str(PROVENANCE)?;
    let violations = legal_reviews
        .validate()
        .into_iter()
        .chain(provenance.validate(&legal_reviews))
        .collect::<Vec<_>>();

    assert!(violations.is_empty(), "{violations:#?}");
    Ok(())
}

#[test]
fn published_compatibility_references_resolve_to_governance_records() -> Result<(), Box<dyn Error>>
{
    let targets: TargetMatrix = serde_json::from_str(TARGETS)?;
    let features: FeatureMatrix = serde_json::from_str(FEATURES)?;
    let legal_reviews: LegalReviewLedger = serde_json::from_str(LEGAL_REVIEWS)?;
    let provenance: ProvenanceLedger = serde_json::from_str(PROVENANCE)?;
    let violations =
        validate_governance_references(&targets, &features, &provenance, &legal_reviews);

    assert!(violations.is_empty(), "{violations:#?}");
    Ok(())
}

#[test]
fn published_schemas_are_valid_json_schema_documents() -> Result<(), Box<dyn Error>> {
    for (name, document) in SCHEMAS {
        let schema: Value = serde_json::from_str(document)?;

        assert_eq!(
            schema.get("$schema").and_then(Value::as_str),
            Some("https://json-schema.org/draft/2020-12/schema")
        );
        assert!(
            schema.get("$id").and_then(Value::as_str).is_some(),
            "schema {name} requires an identifier"
        );
    }

    Ok(())
}
