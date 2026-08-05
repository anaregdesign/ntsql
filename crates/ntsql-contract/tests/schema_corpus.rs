use std::{collections::BTreeSet, error::Error, io};

use ntsql_contract::{
    BehaviorSpecificationAdmissionLedger, ConformanceRecord, FeatureMatrix, LegalDecisionAuthority,
    LegalReviewLedger, ProvenanceLedger, TargetMatrix, validate_governance_references,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

const CORPUS_VERSION: &str = "1.0.0";
const PUBLISHED_SCHEMAS: [&str; 7] = [
    include_str!("../../../contracts/schemas/behavior-specification-admission-ledger.schema.json"),
    include_str!("../../../contracts/schemas/conformance-record.schema.json"),
    include_str!("../../../contracts/schemas/feature-matrix.schema.json"),
    include_str!("../../../contracts/schemas/legal-decision-authority.schema.json"),
    include_str!("../../../contracts/schemas/legal-review-ledger.schema.json"),
    include_str!("../../../contracts/schemas/provenance-ledger.schema.json"),
    include_str!("../../../contracts/schemas/target-matrix.schema.json"),
];
const TARGETS: &str = include_str!("../../../contracts/compatibility/targets.json");
const FEATURES: &str = include_str!("../../../contracts/compatibility/features.json");
const LEGAL_REVIEWS: &str = include_str!("../../../contracts/compatibility/legal-reviews.json");
const PROVENANCE: &str = include_str!("../../../contracts/compatibility/provenance.json");
const BEHAVIOR_ADMISSIONS: &str =
    include_str!("../../../contracts/compatibility/behavior-specification-admissions.json");
const CORPORA: [&str; 7] = [
    include_str!("../../../contracts/schema-corpus/behavior-specification-admission-ledger.json"),
    include_str!("../../../contracts/schema-corpus/target-matrix.json"),
    include_str!("../../../contracts/schema-corpus/feature-matrix.json"),
    include_str!("../../../contracts/schema-corpus/provenance-ledger.json"),
    include_str!("../../../contracts/schema-corpus/legal-review-ledger.json"),
    include_str!("../../../contracts/schema-corpus/conformance-record.json"),
    include_str!("../../../contracts/schema-corpus/legal-decision-authority.json"),
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaCorpus {
    corpus_version: String,
    schema_id: String,
    base_document: CorpusBase,
    cases: Vec<CorpusCase>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum CorpusBase {
    Source(SourceBase),
    Inline(InlineBase),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceBase {
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InlineBase {
    inline: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CorpusCase {
    case_id: String,
    constraint: String,
    patch: Vec<PatchOperation>,
    expected: ExpectedResults,
    #[serde(default)]
    trusted_candidate: Option<TrustedCandidate>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
enum PatchOperation {
    Add { path: String, value: Value },
    Remove { path: String },
    Replace { path: String, value: Value },
    Copy { from: String, path: String },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct ExpectedResults {
    json_schema: bool,
    rust_deserialize: bool,
    rust_schema_semantics: bool,
    rust_full_validation: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrustedCandidate {
    repository: String,
    commit_sha: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RustResults {
    deserialize: bool,
    schema_semantics: bool,
    full_validation: bool,
}

#[test]
fn shared_schema_corpus_matches_rust_contract_boundaries() -> Result<(), Box<dyn Error>> {
    let mut case_ids = BTreeSet::new();
    let mut schema_ids = BTreeSet::new();
    let mut published_schema_ids = BTreeSet::new();

    for schema_document in PUBLISHED_SCHEMAS {
        let schema: Value = serde_json::from_str(schema_document)?;
        let schema_id = schema.get("$id").and_then(Value::as_str).ok_or_else(|| {
            invalid_data("published corpus schema requires an identifier".to_owned())
        })?;
        assert!(
            published_schema_ids.insert(schema_id.to_owned()),
            "duplicate published schema id: {schema_id}"
        );
    }

    for corpus_document in CORPORA {
        let corpus: SchemaCorpus = serde_json::from_str(corpus_document)?;
        assert_eq!(corpus.corpus_version, CORPUS_VERSION);
        assert!(
            schema_ids.insert(corpus.schema_id.clone()),
            "duplicate corpus schema id: {}",
            corpus.schema_id
        );
        assert!(
            !corpus.cases.is_empty(),
            "{} has no cases",
            corpus.schema_id
        );
        let base_document = load_base_document(&corpus.base_document)?;

        for case in &corpus.cases {
            assert!(
                case_ids.insert(case.case_id.clone()),
                "duplicate corpus case id: {}",
                case.case_id
            );
            assert!(
                !case.constraint.trim().is_empty(),
                "{} requires a constraint description",
                case.case_id
            );
            if case.expected.rust_deserialize {
                assert_eq!(
                    case.expected.json_schema, case.expected.rust_schema_semantics,
                    "{} records divergent JSON Schema and typed Rust schema semantics",
                    case.case_id
                );
            }
            assert!(
                !case.expected.rust_schema_semantics || case.expected.rust_deserialize,
                "{} accepts Rust schema semantics without deserializing",
                case.case_id
            );
            assert!(
                !case.expected.rust_full_validation || case.expected.rust_schema_semantics,
                "{} accepts full validation after schema-semantics rejection",
                case.case_id
            );

            let mut instance = base_document.clone();
            apply_patch(&mut instance, &case.patch)?;
            let actual = evaluate_rust_contract(&corpus.schema_id, instance, case)?;

            assert_eq!(
                actual.deserialize, case.expected.rust_deserialize,
                "{} has an unexpected deserialization result",
                case.case_id
            );
            assert_eq!(
                actual.schema_semantics, case.expected.rust_schema_semantics,
                "{} has an unexpected schema-semantics result",
                case.case_id
            );
            assert_eq!(
                actual.full_validation, case.expected.rust_full_validation,
                "{} has an unexpected full-validation result",
                case.case_id
            );
        }
    }

    assert_eq!(schema_ids, published_schema_ids);

    Ok(())
}

fn load_base_document(base: &CorpusBase) -> Result<Value, Box<dyn Error>> {
    let document = match base {
        CorpusBase::Source(source) => match source.source.as_str() {
            "../compatibility/targets.json" => TARGETS,
            "../compatibility/features.json" => FEATURES,
            "../compatibility/legal-reviews.json" => LEGAL_REVIEWS,
            "../compatibility/provenance.json" => PROVENANCE,
            "../compatibility/behavior-specification-admissions.json" => BEHAVIOR_ADMISSIONS,
            other => return Err(invalid_data(format!("unknown corpus source: {other}")).into()),
        },
        CorpusBase::Inline(inline) => return Ok(inline.inline.clone()),
    };

    Ok(serde_json::from_str(document)?)
}

fn apply_patch(document: &mut Value, operations: &[PatchOperation]) -> Result<(), Box<dyn Error>> {
    for operation in operations {
        match operation {
            PatchOperation::Add { path, value } => add_value(document, path, value.clone())?,
            PatchOperation::Remove { path } => remove_value(document, path)?,
            PatchOperation::Replace { path, value } => {
                replace_value(document, path, value.clone())?;
            }
            PatchOperation::Copy { from, path } => {
                let value = value_at_pointer(document, from)?
                    .cloned()
                    .ok_or_else(|| invalid_data(format!("copy source does not exist: {from}")))?;
                add_value(document, path, value)?;
            }
        }
    }

    Ok(())
}

fn add_value(document: &mut Value, path: &str, value: Value) -> Result<(), Box<dyn Error>> {
    if path.is_empty() {
        *document = value;
        return Ok(());
    }

    let (parent_tokens, token) = split_pointer(path)?;
    let parent = value_at_tokens_mut(document, &parent_tokens)?
        .ok_or_else(|| invalid_data(format!("add parent does not exist: {path}")))?;

    match parent {
        Value::Object(object) => {
            object.insert(token, value);
        }
        Value::Array(array) if token == "-" => array.push(value),
        Value::Array(array) => {
            let index = parse_array_index(&token)?;
            if index > array.len() {
                return Err(invalid_data(format!("add index is out of bounds: {index}")).into());
            }
            array.insert(index, value);
        }
        _ => return Err(invalid_data(format!("add parent is not a container: {path}")).into()),
    }

    Ok(())
}

fn remove_value(document: &mut Value, path: &str) -> Result<(), Box<dyn Error>> {
    if path.is_empty() {
        return Err(invalid_data(
            "remove cannot delete the corpus document root because a JSON instance must remain"
                .to_owned(),
        )
        .into());
    }

    let (parent_tokens, token) = split_pointer(path)?;
    let parent = value_at_tokens_mut(document, &parent_tokens)?
        .ok_or_else(|| invalid_data(format!("remove parent does not exist: {path}")))?;

    let removed = match parent {
        Value::Object(object) => object.remove(&token).is_some(),
        Value::Array(array) => {
            let index = parse_array_index(&token)?;
            if index >= array.len() {
                false
            } else {
                array.remove(index);
                true
            }
        }
        _ => false,
    };
    if !removed {
        return Err(invalid_data(format!("remove path does not exist: {path}")).into());
    }

    Ok(())
}

fn replace_value(document: &mut Value, path: &str, value: Value) -> Result<(), Box<dyn Error>> {
    let target = value_at_pointer_mut(document, path)?
        .ok_or_else(|| invalid_data(format!("replace path does not exist: {path}")))?;
    *target = value;
    Ok(())
}

fn value_at_pointer<'a>(
    document: &'a Value,
    path: &str,
) -> Result<Option<&'a Value>, Box<dyn Error>> {
    let tokens = parse_pointer(path)?;
    value_at_tokens(document, &tokens)
}

fn value_at_pointer_mut<'a>(
    document: &'a mut Value,
    path: &str,
) -> Result<Option<&'a mut Value>, Box<dyn Error>> {
    let tokens = parse_pointer(path)?;
    value_at_tokens_mut(document, &tokens)
}

fn value_at_tokens<'a>(
    document: &'a Value,
    tokens: &[String],
) -> Result<Option<&'a Value>, Box<dyn Error>> {
    let mut current = document;
    for token in tokens {
        current = match current {
            Value::Object(object) => match object.get(token) {
                Some(value) => value,
                None => return Ok(None),
            },
            Value::Array(array) => match array.get(parse_array_index(token)?) {
                Some(value) => value,
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
    }
    Ok(Some(current))
}

fn value_at_tokens_mut<'a>(
    document: &'a mut Value,
    tokens: &[String],
) -> Result<Option<&'a mut Value>, Box<dyn Error>> {
    let mut current = document;
    for token in tokens {
        current = match current {
            Value::Object(object) => match object.get_mut(token) {
                Some(value) => value,
                None => return Ok(None),
            },
            Value::Array(array) => match array.get_mut(parse_array_index(token)?) {
                Some(value) => value,
                None => return Ok(None),
            },
            _ => return Ok(None),
        };
    }
    Ok(Some(current))
}

fn split_pointer(path: &str) -> Result<(Vec<String>, String), Box<dyn Error>> {
    let mut tokens = parse_pointer(path)?;
    let token = tokens
        .pop()
        .ok_or_else(|| invalid_data("document root has no parent token".to_owned()))?;
    Ok((tokens, token))
}

fn parse_pointer(path: &str) -> Result<Vec<String>, Box<dyn Error>> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let Some(encoded_tokens) = path.strip_prefix('/') else {
        return Err(invalid_data(format!("invalid JSON Pointer: {path}")).into());
    };

    encoded_tokens
        .split('/')
        .map(|token| decode_pointer_token(path, token))
        .collect()
}

fn decode_pointer_token(path: &str, token: &str) -> Result<String, Box<dyn Error>> {
    let mut decoded = String::new();
    let mut characters = token.chars();
    while let Some(character) = characters.next() {
        if character != '~' {
            decoded.push(character);
            continue;
        }

        match characters.next() {
            Some('0') => decoded.push('~'),
            Some('1') => decoded.push('/'),
            _ => return Err(invalid_data(format!("invalid JSON Pointer escape: {path}")).into()),
        }
    }

    Ok(decoded)
}

fn parse_array_index(token: &str) -> Result<usize, Box<dyn Error>> {
    if token.is_empty()
        || (token.len() > 1 && token.starts_with('0'))
        || !token.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(invalid_data(format!("invalid array index: {token}")).into());
    }

    token
        .parse::<usize>()
        .map_err(|error| invalid_data(format!("invalid array index {token}: {error}")).into())
}

#[test]
fn patch_runner_supports_root_and_empty_object_keys() -> Result<(), Box<dyn Error>> {
    let mut document = json!({"": 1, "a/b": 2, "m~n": 3});

    remove_value(&mut document, "/")?;
    replace_value(&mut document, "/a~1b", json!(4))?;
    replace_value(&mut document, "/m~0n", json!(5))?;
    assert_eq!(document, json!({"a/b": 4, "m~n": 5}));

    add_value(&mut document, "", json!({"added": true}))?;
    assert_eq!(document, json!({"added": true}));
    replace_value(&mut document, "", json!({"root": true}))?;
    apply_patch(
        &mut document,
        &[PatchOperation::Copy {
            from: "".to_owned(),
            path: "/snapshot".to_owned(),
        }],
    )?;

    assert_eq!(document["root"], json!(true));
    assert_eq!(document["snapshot"], json!({"root": true}));
    Ok(())
}

#[test]
fn patch_runner_rejects_ambiguous_pointers() {
    let document = json!({"items": [1, 2], "x~2": true});
    let invalid_paths = ["missing-slash", "/x~2", "/items/01", "/items/+1"];

    for path in invalid_paths {
        assert!(
            value_at_pointer(&document, path).is_err(),
            "accepted {path}"
        );
    }
}

#[test]
fn patch_runner_rejects_root_removal() {
    let mut document = json!({"root": true});
    assert!(remove_value(&mut document, "").is_err());
}

fn evaluate_rust_contract(
    schema_id: &str,
    instance: Value,
    case: &CorpusCase,
) -> Result<RustResults, Box<dyn Error>> {
    match schema_id {
        "https://github.com/anaregdesign/ntsql/contracts/schemas/behavior-specification-admission-ledger.schema.json" => {
            Ok(evaluate_behavior_admissions(instance))
        }
        "https://github.com/anaregdesign/ntsql/contracts/schemas/target-matrix.schema.json" => {
            let features = serde_json::from_str(FEATURES)?;
            let provenance = serde_json::from_str(PROVENANCE)?;
            let legal_reviews = serde_json::from_str(LEGAL_REVIEWS)?;
            Ok(evaluate_target_matrix(
                instance,
                &features,
                &provenance,
                &legal_reviews,
            ))
        }
        "https://github.com/anaregdesign/ntsql/contracts/schemas/feature-matrix.schema.json" => {
            let targets = serde_json::from_str(TARGETS)?;
            let provenance = serde_json::from_str(PROVENANCE)?;
            let legal_reviews = serde_json::from_str(LEGAL_REVIEWS)?;
            Ok(evaluate_feature_matrix(
                instance,
                &targets,
                &provenance,
                &legal_reviews,
            ))
        }
        "https://github.com/anaregdesign/ntsql/contracts/schemas/legal-review-ledger.schema.json" =>
        {
            let provenance = serde_json::from_str(PROVENANCE)?;
            Ok(evaluate_legal_reviews(instance, &provenance))
        }
        "https://github.com/anaregdesign/ntsql/contracts/schemas/provenance-ledger.schema.json" => {
            let legal_reviews = serde_json::from_str(LEGAL_REVIEWS)?;
            Ok(evaluate_provenance(instance, &legal_reviews))
        }
        "https://github.com/anaregdesign/ntsql/contracts/schemas/conformance-record.schema.json" => {
            let targets = serde_json::from_str(TARGETS)?;
            let features = serde_json::from_str(FEATURES)?;
            let provenance = serde_json::from_str(PROVENANCE)?;
            Ok(evaluate_conformance(
                instance,
                &targets,
                &features,
                &provenance,
            ))
        }
        "https://github.com/anaregdesign/ntsql/contracts/schemas/legal-decision-authority.schema.json" => {
            Ok(evaluate_authority(
                instance,
                case.trusted_candidate.as_ref(),
            ))
        }
        _ => Err(invalid_data(format!("unknown corpus schema id: {schema_id}")).into()),
    }
}

fn evaluate_behavior_admissions(instance: Value) -> RustResults {
    let Ok(contract) = deserialize_wire::<BehaviorSpecificationAdmissionLedger>(&instance) else {
        return rejected_at_deserialization();
    };
    RustResults {
        deserialize: true,
        schema_semantics: contract.validate_schema_semantics().is_empty(),
        full_validation: contract.validate().is_empty(),
    }
}

fn evaluate_target_matrix(
    instance: Value,
    features: &FeatureMatrix,
    provenance: &ProvenanceLedger,
    legal_reviews: &LegalReviewLedger,
) -> RustResults {
    let Ok(contract) = deserialize_wire::<TargetMatrix>(&instance) else {
        return rejected_at_deserialization();
    };
    let schema_semantics = contract.validate_schema_semantics().is_empty();
    let mut violations = contract.validate();
    violations.extend(features.validate(&contract));
    violations.extend(validate_governance_references(
        &contract,
        features,
        provenance,
        legal_reviews,
    ));
    RustResults {
        deserialize: true,
        schema_semantics,
        full_validation: violations.is_empty(),
    }
}

fn evaluate_feature_matrix(
    instance: Value,
    targets: &TargetMatrix,
    provenance: &ProvenanceLedger,
    legal_reviews: &LegalReviewLedger,
) -> RustResults {
    let Ok(contract) = deserialize_wire::<FeatureMatrix>(&instance) else {
        return rejected_at_deserialization();
    };
    let schema_semantics = contract.validate_schema_semantics().is_empty();
    let mut violations = contract.validate(targets);
    violations.extend(validate_governance_references(
        targets,
        &contract,
        provenance,
        legal_reviews,
    ));
    RustResults {
        deserialize: true,
        schema_semantics,
        full_validation: violations.is_empty(),
    }
}

fn evaluate_legal_reviews(instance: Value, provenance: &ProvenanceLedger) -> RustResults {
    let Ok(contract) = deserialize_wire::<LegalReviewLedger>(&instance) else {
        return rejected_at_deserialization();
    };
    let schema_semantics = contract.validate_schema_semantics().is_empty();
    let mut violations = contract.validate();
    violations.extend(provenance.validate(&contract));
    RustResults {
        deserialize: true,
        schema_semantics,
        full_validation: violations.is_empty(),
    }
}

fn evaluate_provenance(instance: Value, legal_reviews: &LegalReviewLedger) -> RustResults {
    let Ok(contract) = deserialize_wire::<ProvenanceLedger>(&instance) else {
        return rejected_at_deserialization();
    };
    RustResults {
        deserialize: true,
        schema_semantics: contract.validate_schema_semantics().is_empty(),
        full_validation: contract.validate(legal_reviews).is_empty(),
    }
}

fn evaluate_conformance(
    instance: Value,
    targets: &TargetMatrix,
    features: &FeatureMatrix,
    provenance: &ProvenanceLedger,
) -> RustResults {
    let Ok(contract) = deserialize_wire::<ConformanceRecord>(&instance) else {
        return rejected_at_deserialization();
    };
    let schema_semantics = contract.validate_schema_semantics().is_empty();
    let full_validation = schema_semantics
        && contract.validate_document_semantics().is_empty()
        && contract
            .validate_references(targets, features, provenance)
            .is_empty();
    RustResults {
        deserialize: true,
        schema_semantics,
        full_validation,
    }
}

fn evaluate_authority(instance: Value, trusted: Option<&TrustedCandidate>) -> RustResults {
    let Ok(contract) = deserialize_wire::<LegalDecisionAuthority>(&instance) else {
        return rejected_at_deserialization();
    };
    let schema_semantics = contract.validate_schema_semantics().is_empty();
    let trusted_repository = trusted
        .map(|candidate| candidate.repository.as_str())
        .unwrap_or(contract.candidate_repository.as_str());
    let trusted_commit = trusted
        .map(|candidate| candidate.commit_sha.as_str())
        .unwrap_or(contract.candidate_commit_sha.as_str());
    let full_validation = schema_semantics
        && contract.validate_document_semantics().is_empty()
        && contract
            .validate_trusted_candidate(trusted_repository, trusted_commit)
            .is_empty();
    RustResults {
        deserialize: true,
        schema_semantics,
        full_validation,
    }
}

fn deserialize_wire<T: DeserializeOwned>(instance: &Value) -> Result<T, serde_json::Error> {
    let encoded = serde_json::to_vec(instance)?;
    serde_json::from_slice(&encoded)
}

const fn rejected_at_deserialization() -> RustResults {
    RustResults {
        deserialize: false,
        schema_semantics: false,
        full_validation: false,
    }
}

fn invalid_data(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
