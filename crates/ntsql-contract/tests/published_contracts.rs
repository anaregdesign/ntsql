use std::{collections::BTreeSet, error::Error, io};

use ntsql_contract::{
    CompatibilityStatus, FeatureCategory, FeatureMatrix, LegalReviewLedger, ProvenanceLedger,
    TargetMatrix, validate_governance_references,
};
use serde_json::Value;

const FEATURES: &str = include_str!("../../../contracts/compatibility/features.json");
const LEGAL_REVIEWS: &str = include_str!("../../../contracts/compatibility/legal-reviews.json");
const PROVENANCE: &str = include_str!("../../../contracts/compatibility/provenance.json");
const TARGETS: &str = include_str!("../../../contracts/compatibility/targets.json");
const SCHEMAS: [(&str, &str); 6] = [
    (
        "conformance-record",
        include_str!("../../../contracts/schemas/conformance-record.schema.json"),
    ),
    (
        "feature-matrix",
        include_str!("../../../contracts/schemas/feature-matrix.schema.json"),
    ),
    (
        "legal-decision-authority",
        include_str!("../../../contracts/schemas/legal-decision-authority.schema.json"),
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
fn first_query_slice_inventory_is_explicit() -> Result<(), Box<dyn Error>> {
    const BASELINE_TARGET: &str = "sqlserver-2022-cu26-linux-x86_64-developer-compat160";
    const INVENTORY_PROVENANCE: &str = "prov-ms-sql-engine-overview-2022";
    let features: FeatureMatrix = serde_json::from_str(FEATURES)?;

    for (id, category, owner_issue) in [
        ("language.query.select", FeatureCategory::Language, 8),
        ("data-types.literal.integer", FeatureCategory::DataTypes, 6),
        (
            "query-processing.constant-projection",
            FeatureCategory::QueryProcessing,
            14,
        ),
    ] {
        let feature = features
            .features
            .iter()
            .find(|feature| feature.id == id)
            .ok_or_else(|| invalid_data(format!("missing first query-slice feature {id}")))?;

        assert_eq!(feature.category, category);
        assert_eq!(feature.status, CompatibilityStatus::NotTested);
        assert_eq!(feature.oracle_targets, [BASELINE_TARGET]);
        assert_eq!(feature.evidence, [INVENTORY_PROVENANCE]);
        assert!(feature.differences.is_empty());
        assert_eq!(feature.owner_issue, owner_issue);
        assert_eq!(feature.legal_review_id, None);
    }

    Ok(())
}

#[test]
fn first_wal_surface_inventory_is_explicit() -> Result<(), Box<dyn Error>> {
    let features: FeatureMatrix = serde_json::from_str(FEATURES)?;
    let feature = features
        .features
        .iter()
        .find(|feature| feature.id == "storage-recovery.write-ahead-commit")
        .ok_or_else(|| invalid_data("missing write-ahead commit feature".to_owned()))?;

    assert_eq!(feature.category, FeatureCategory::StorageRecovery);
    assert_eq!(feature.status, CompatibilityStatus::NotTested);
    assert_eq!(
        feature.oracle_targets,
        ["sqlserver-2022-cu26-linux-x86_64-developer-compat160"]
    );
    assert_eq!(feature.evidence, ["prov-ms-sql-engine-overview-2022"]);
    assert!(feature.differences.is_empty());
    assert_eq!(feature.owner_issue, 9);
    assert_eq!(feature.legal_review_id, None);
    Ok(())
}

#[test]
fn first_transaction_surface_inventory_is_explicit() -> Result<(), Box<dyn Error>> {
    let features: FeatureMatrix = serde_json::from_str(FEATURES)?;
    let feature = features
        .features
        .iter()
        .find(|feature| feature.id == "transactions-concurrency.commit-lifecycle")
        .ok_or_else(|| invalid_data("missing transaction commit feature".to_owned()))?;

    assert_eq!(feature.category, FeatureCategory::TransactionsConcurrency);
    assert_eq!(feature.status, CompatibilityStatus::NotTested);
    assert_eq!(
        feature.oracle_targets,
        ["sqlserver-2022-cu26-linux-x86_64-developer-compat160"]
    );
    assert_eq!(feature.evidence, ["prov-ms-sql-engine-overview-2022"]);
    assert!(feature.differences.is_empty());
    assert_eq!(feature.owner_issue, 7);
    assert_eq!(feature.legal_review_id, None);
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
fn published_schema_documents_have_unique_ids_and_reachable_references()
-> Result<(), Box<dyn Error>> {
    let schemas = SCHEMAS
        .iter()
        .map(|(name, document)| Ok((*name, serde_json::from_str::<Value>(document)?)))
        .collect::<Result<Vec<_>, serde_json::Error>>()?;
    let mut schema_ids = BTreeSet::new();

    for (name, schema) in &schemas {
        assert_eq!(
            schema.get("$schema").and_then(Value::as_str),
            Some("https://json-schema.org/draft/2020-12/schema")
        );
        let schema_id = schema
            .get("$id")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_data(format!("schema {name} requires an identifier")))?;
        assert!(
            schema_ids.insert(schema_id),
            "duplicate schema id: {schema_id}"
        );
        assert_schema_references_resolve(name, schema, &schemas)?;
    }

    Ok(())
}

fn assert_schema_references_resolve(
    schema_name: &str,
    node: &Value,
    schemas: &[(&str, Value)],
) -> Result<(), io::Error> {
    match node {
        Value::Array(values) => {
            for value in values {
                assert_schema_references_resolve(schema_name, value, schemas)?;
            }
        }
        Value::Object(object) => {
            if let Some(reference) = object.get("$ref") {
                let reference = reference.as_str().ok_or_else(|| {
                    invalid_data(format!("schema {schema_name} contains a non-string $ref"))
                })?;
                let (document, pointer) = reference.split_once('#').unwrap_or((reference, ""));
                let target_name = if document.is_empty() {
                    schema_name
                } else {
                    document.strip_suffix(".schema.json").ok_or_else(|| {
                        invalid_data(format!(
                            "schema {schema_name} contains unsupported $ref {reference}"
                        ))
                    })?
                };
                let target = schemas
                    .iter()
                    .find(|(name, _)| *name == target_name)
                    .map(|(_, schema)| schema)
                    .ok_or_else(|| {
                        invalid_data(format!(
                            "schema {schema_name} references missing document {document}"
                        ))
                    })?;
                if !pointer.is_empty()
                    && (!pointer.starts_with('/') || target.pointer(pointer).is_none())
                {
                    return Err(invalid_data(format!(
                        "schema {schema_name} contains unreachable $ref {reference}"
                    )));
                }
            }
            for value in object.values() {
                assert_schema_references_resolve(schema_name, value, schemas)?;
            }
        }
        _ => {}
    }

    Ok(())
}

#[test]
fn schema_required_nullable_fields_cannot_be_omitted() -> Result<(), Box<dyn Error>> {
    for pointer in [
        "/records/0/source_url",
        "/records/0/artifact_path",
        "/records/0/environment",
    ] {
        let mut document: Value = serde_json::from_str(PROVENANCE)?;
        remove_pointer(&mut document, pointer)?;
        assert!(
            serde_json::from_value::<ProvenanceLedger>(document).is_err(),
            "omitting {pointer} must fail"
        );
    }

    for pointer in [
        "/reviews/0/reviewed_by",
        "/reviews/0/decided_on",
        "/reviews/0/decision_evidence",
    ] {
        let mut document: Value = serde_json::from_str(LEGAL_REVIEWS)?;
        remove_pointer(&mut document, pointer)?;
        assert!(
            serde_json::from_value::<LegalReviewLedger>(document).is_err(),
            "omitting {pointer} must fail"
        );
    }

    let mut document: Value = serde_json::from_str(FEATURES)?;
    remove_pointer(&mut document, "/features/0/legal_review_id")?;
    assert!(
        serde_json::from_value::<FeatureMatrix>(document).is_err(),
        "omitting /features/0/legal_review_id must fail"
    );

    Ok(())
}

fn remove_pointer(document: &mut Value, pointer: &str) -> Result<(), io::Error> {
    let (parent, field) = pointer
        .rsplit_once('/')
        .ok_or_else(|| invalid_data(format!("pointer has no field: {pointer}")))?;
    let removed = document
        .pointer_mut(parent)
        .and_then(Value::as_object_mut)
        .and_then(|object| object.remove(field));
    if removed.is_none() {
        return Err(invalid_data(format!(
            "contract pointer does not exist: {pointer}"
        )));
    }

    Ok(())
}

fn invalid_data(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
