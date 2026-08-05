use std::{collections::BTreeSet, error::Error};

use ntsql_compatibility::{
    CompatibilityContext, CompatibilityContextError, CompatibilityProfile, OBSERVATION_DIMENSIONS,
};

#[test]
fn context_preserves_a_complete_synthetic_profile() -> Result<(), Box<dyn Error>> {
    let context = CompatibilityContext::try_new(synthetic_profile())?;

    assert_eq!(context.target_id().as_str(), "test-target-a");
    assert_eq!(context.product_release(), "test-release");
    assert_eq!(context.servicing_update(), "test-update");
    assert_eq!(context.product_version().as_str(), "1.2.3.4");
    assert_eq!(context.compatibility_level().get(), 42);
    assert_eq!(context.session_defaults(), ["SET TEST_OPTION ON"]);
    Ok(())
}

#[test]
fn request_scope_preserves_the_exact_context() -> Result<(), Box<dyn Error>> {
    let context = CompatibilityContext::try_new(synthetic_profile())?;

    context.with_scope(|scope| {
        let copied_scope = scope;

        assert!(std::ptr::eq(scope.context(), &context));
        assert!(std::ptr::eq(copied_scope.context(), &context));
        assert_eq!(scope.context(), &context);
        assert_eq!(copied_scope.context().target_id().as_str(), "test-target-a");
    });

    Ok(())
}

#[test]
fn context_rejects_invalid_identifiers_and_versions() {
    let mut invalid_id = synthetic_profile();
    invalid_id.target_id = "invalid target".to_owned();
    assert_eq!(
        CompatibilityContext::try_new(invalid_id),
        Err(CompatibilityContextError::InvalidTargetId)
    );

    let mut invalid_version = synthetic_profile();
    invalid_version.product_version = "1.2.3".to_owned();
    assert_eq!(
        CompatibilityContext::try_new(invalid_version),
        Err(CompatibilityContextError::InvalidProductVersion)
    );
}

#[test]
fn context_rejects_incomplete_or_ambiguous_session_defaults() {
    let mut missing = synthetic_profile();
    missing.session_defaults.clear();
    assert_eq!(
        CompatibilityContext::try_new(missing),
        Err(CompatibilityContextError::MissingSessionDefaults)
    );

    let mut duplicate = synthetic_profile();
    duplicate
        .session_defaults
        .push("SET TEST_OPTION ON".to_owned());
    assert_eq!(
        CompatibilityContext::try_new(duplicate),
        Err(CompatibilityContextError::DuplicateSessionDefault(
            "SET TEST_OPTION ON".to_owned()
        ))
    );
}

#[test]
fn observation_dimension_set_is_complete_and_unique() {
    let unique = OBSERVATION_DIMENSIONS.into_iter().collect::<BTreeSet<_>>();

    assert_eq!(OBSERVATION_DIMENSIONS.len(), 7);
    assert_eq!(unique.len(), OBSERVATION_DIMENSIONS.len());
}

fn synthetic_profile() -> CompatibilityProfile {
    CompatibilityProfile {
        target_id: "test-target-a".to_owned(),
        product_release: "test-release".to_owned(),
        servicing_update: "test-update".to_owned(),
        product_version: "1.2.3.4".to_owned(),
        edition: "test-edition".to_owned(),
        operating_system: "test-operating-system".to_owned(),
        architecture: "test-architecture".to_owned(),
        compatibility_level: 42,
        collation: "test-collation".to_owned(),
        language: "test-language".to_owned(),
        lcid: 1,
        timezone: "test-timezone".to_owned(),
        session_defaults: vec!["SET TEST_OPTION ON".to_owned()],
    }
}
