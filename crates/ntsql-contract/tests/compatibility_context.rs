use std::error::Error;

use ntsql_compatibility::OBSERVATION_DIMENSIONS;
use ntsql_contract::{CompatibilityDimension, TargetMatrix, ValidatedTargetMatrix};

const TARGETS: &str = include_str!("../../../contracts/compatibility/targets.json");

#[test]
fn validated_baseline_produces_the_exact_compatibility_context() -> Result<(), Box<dyn Error>> {
    let matrix: TargetMatrix = serde_json::from_str(TARGETS)?;
    let validated = ValidatedTargetMatrix::try_from(matrix)?;
    let context = validated.baseline_context();

    assert_eq!(
        context.target_id().as_str(),
        "sqlserver-2022-cu26-linux-x86_64-developer-compat160"
    );
    assert_eq!(context.product_version().as_str(), "16.0.4265.3");
    assert_eq!(context.edition(), "Developer");
    assert_eq!(context.compatibility_level().get(), 160);
    assert_eq!(context.collation(), "SQL_Latin1_General_CP1_CI_AS");
    assert_eq!(context.language(), "us_english");
    assert_eq!(context.lcid(), 1033);
    assert_eq!(
        context.operating_system(),
        "Ubuntu 22.04 container on Linux"
    );
    assert_eq!(context.architecture(), "x86_64");
    assert_eq!(context.timezone(), "UTC");
    assert_eq!(context.session_defaults().len(), 18);
    assert_eq!(
        context.session_defaults().first().map(String::as_str),
        Some("SET ANSI_NULLS ON")
    );
    assert_eq!(
        context.session_defaults().last().map(String::as_str),
        Some("SET TEXTSIZE 2147483647")
    );
    Ok(())
}

#[test]
fn invalid_target_matrix_cannot_produce_a_context() -> Result<(), Box<dyn Error>> {
    let mut matrix: TargetMatrix = serde_json::from_str(TARGETS)?;
    matrix.targets[0].product_version = "16.0.4265".to_owned();

    let error = match ValidatedTargetMatrix::try_from(matrix) {
        Ok(_) => return Err("invalid target matrix was accepted".into()),
        Err(error) => error,
    };

    assert!(
        error
            .violations()
            .iter()
            .any(|violation| violation.code == "target.product-version.invalid")
    );
    Ok(())
}

#[test]
fn unknown_baseline_cannot_produce_a_context() -> Result<(), Box<dyn Error>> {
    let mut matrix: TargetMatrix = serde_json::from_str(TARGETS)?;
    matrix.baseline_target_id = "unknown-target".to_owned();

    let error = match ValidatedTargetMatrix::try_from(matrix) {
        Ok(_) => return Err("unknown baseline target was accepted".into()),
        Err(error) => error,
    };

    assert!(
        error
            .violations()
            .iter()
            .any(|violation| violation.code == "target.baseline.unknown")
    );
    Ok(())
}

#[test]
fn validated_matrix_selects_each_target_without_shared_state() -> Result<(), Box<dyn Error>> {
    let mut matrix: TargetMatrix = serde_json::from_str(TARGETS)?;
    let mut alternate = matrix.targets[0].clone();
    alternate.id = "test-target-context-b".to_owned();
    alternate.product_release = "test-release".to_owned();
    alternate.servicing_update = "test-update".to_owned();
    alternate.product_version = "1.2.3.4".to_owned();
    alternate.edition = "test-edition".to_owned();
    alternate.operating_system = "test-operating-system".to_owned();
    alternate.architecture = "test-architecture".to_owned();
    alternate.compatibility_level = 42;
    alternate.collation = "test-collation".to_owned();
    alternate.language = "test-language".to_owned();
    alternate.lcid = 1;
    alternate.timezone = "test-timezone".to_owned();
    alternate.session_settings = vec!["SET TEST_OPTION ON".to_owned()];
    matrix.targets.push(alternate);

    let validated = ValidatedTargetMatrix::try_from(matrix)?;

    assert_eq!(
        validated.baseline_context().compatibility_level().get(),
        160
    );
    assert_eq!(
        validated
            .select_context("test-target-context-b")?
            .compatibility_level()
            .get(),
        42
    );
    let error = match validated.select_context("unknown-target") {
        Ok(_) => return Err("unknown target was accepted".into()),
        Err(error) => error,
    };
    assert_eq!(error.target_id(), "unknown-target");
    Ok(())
}

#[test]
fn duplicate_target_cannot_enter_the_validated_typestate() -> Result<(), Box<dyn Error>> {
    let mut matrix: TargetMatrix = serde_json::from_str(TARGETS)?;
    matrix.targets.push(matrix.targets[0].clone());

    let error = match ValidatedTargetMatrix::try_from(matrix) {
        Ok(_) => return Err("duplicate target matrix was accepted".into()),
        Err(error) => error,
    };

    assert!(
        error
            .violations()
            .iter()
            .any(|violation| violation.code == "target.id.duplicate")
    );
    Ok(())
}

#[test]
fn domain_observation_dimensions_match_the_wire_contract() {
    let contract_dimensions = [
        CompatibilityDimension::Syntax,
        CompatibilityDimension::Wire,
        CompatibilityDimension::Result,
        CompatibilityDimension::Metadata,
        CompatibilityDimension::Diagnostic,
        CompatibilityDimension::TransactionalSideEffect,
        CompatibilityDimension::Operational,
    ];

    assert_eq!(
        OBSERVATION_DIMENSIONS.map(CompatibilityDimension::from),
        contract_dimensions
    );
}
