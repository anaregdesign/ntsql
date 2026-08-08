use std::{error::Error, io};

use ntsql_compatibility::{CompatibilityContext, CompatibilityProfile};
use ntsql_database::{
    DatabaseCleanCloseCertificate, DatabaseCleanManifestPublicationState,
    DatabaseClosePreparationFailureCause, DatabaseClosePreparationPreflightError,
    DatabaseCompositionIdentity, DatabaseFileId, DatabaseFileIdentity, DatabaseFileRole,
    DatabaseId, DatabaseLifecycleGeneration, DatabaseLifecycleStage, DatabaseManifest,
    DatabaseManifestLifecycleState, DatabaseRecoveryRequiredBindingRejection,
    DatabaseRequiredFeatures, DatabaseStorageFormatRequirements, DatabaseStorageFormatVersion,
};
use ntsql_storage_memory::{
    InMemoryCommitLog, InMemoryDatabaseCloseBoundary, InMemoryDatabaseCloseFault,
    InMemoryDatabaseCloseFaultTiming, InMemoryDatabaseCreateBoundary, InMemoryDatabaseCreateError,
    InMemoryDatabaseCreateFault, InMemoryDatabaseCreateFaultTiming,
    InMemoryDatabaseCreateManifestError, InMemoryDatabaseCreateOutcome,
    InMemoryDatabaseCreatePhase, InMemoryDatabaseFileObservation, InMemoryDatabaseLiveOpenRequest,
    InMemoryDatabaseObjectId, InMemoryDatabaseObjectRole, InMemoryDatabaseOpenPhase,
    InMemoryDatabaseOwnershipError, InMemoryDatabaseOwnershipSlot,
    InMemoryDatabaseOwnershipSlotError, InMemoryDatabaseOwnershipWorld,
    InMemoryDatabaseRecoveryStorage, InMemoryPageStore,
    InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError,
    InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
    InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError, LiveInMemoryDatabase,
    RestartCheckpointCompletenessBaselineSourceFaultPoint,
    TransactionPageStorageCleanCloseCheckpointFaultPoint, open_live_in_memory_database,
    open_live_in_memory_database_with_observer,
};
use ntsql_transaction::{
    DurableTransactionPageStorageCleanCloseBeforePublicationError,
    DurableTransactionPageStorageCleanCloseEvidenceError,
    DurableTransactionPageStorageCleanCloseOutcomeIndeterminateError,
    DurableTransactionPageStorageCleanClosePreparationError, TransactionLifecycleStatus,
    TransactionPageStorageRecoveryHandoffPhase,
};
use ntsql_wal::PersistentLogId;

#[test]
fn successful_acquisition_contends_and_releases_on_owner_drop() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new(1, 101)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    let same_slot = composition.slot(&mut world)?;
    let opened = composition.acquire(&slot)?;
    assert!(slot.is_owned());
    assert_eq!(
        opened.identity(),
        composition.manifest.composition_identity()
    );
    assert_eq!(opened.stage(), DatabaseLifecycleStage::ManifestSelected);
    assert_eq!(
        composition.acquire(&same_slot).err(),
        Some(InMemoryDatabaseOwnershipError::Contended {
            database_id: composition.database_id,
        })
    );

    drop(opened);
    assert!(!slot.is_owned());
    drop(composition.acquire(&slot)?);
    assert!(!slot.is_owned());
    assert_eq!(
        world
            .slot(database_id(999)?, composition.owner_object_id)
            .err(),
        Some(InMemoryDatabaseOwnershipSlotError::ObjectBindingMismatch {
            object_id: composition.owner_object_id,
            bound_database_id: composition.database_id,
            bound_role: InMemoryDatabaseObjectRole::DatabaseOwner,
            requested_database_id: database_id(999)?,
            requested_role: InMemoryDatabaseObjectRole::DatabaseOwner,
        })
    );
    Ok(())
}

#[test]
fn exact_acquisition_reaches_recovery_required_across_manifest_generations()
-> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new(6, 106)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    let opened = composition.acquire_recovery_required(&slot, composition.manifest)?;
    assert_eq!(
        opened.identity(),
        composition.manifest.composition_identity()
    );
    assert_eq!(opened.stage(), DatabaseLifecycleStage::RecoveryRequired);
    assert!(slot.is_owned());
    drop(opened);

    let successor = composition.manifest.next_recovery_required()?;
    let reopened = composition.acquire_recovery_required(&slot, successor)?;
    assert_eq!(reopened.identity(), successor.composition_identity());
    assert_eq!(
        reopened.identity().storage_identity(),
        composition
            .manifest
            .composition_identity()
            .storage_identity()
    );
    Ok(())
}

#[test]
fn world_guards_every_database_object_across_distinct_owner_slots() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new(11, 111)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let primary_slot = composition.slot(&mut world)?;
    let opened = composition.acquire(&primary_slot)?;

    let secondary_owner = object_id(9_001)?;
    let secondary_manifest = object_id(9_002)?;
    let secondary_slot = world.slot(composition.database_id, secondary_owner)?;
    let secondary_files = [
        composition.files[0],
        replace_object_id(composition.files[1], object_id(9_004)?),
        replace_object_id(composition.files[2], object_id(9_005)?),
    ];
    assert_eq!(
        secondary_slot
            .try_acquire(
                composition.database_id,
                secondary_manifest,
                composition.manifest,
                &secondary_files,
            )
            .err(),
        Some(InMemoryDatabaseOwnershipError::ObjectContended {
            object_id: composition.files[0].object_id(),
            role: InMemoryDatabaseObjectRole::Wal,
        })
    );
    assert!(!secondary_slot.is_owned());
    assert_eq!(
        world
            .slot(composition.database_id, composition.files[0].object_id())
            .err(),
        Some(InMemoryDatabaseOwnershipSlotError::ObjectBindingMismatch {
            object_id: composition.files[0].object_id(),
            bound_database_id: composition.database_id,
            bound_role: InMemoryDatabaseObjectRole::Wal,
            requested_database_id: composition.database_id,
            requested_role: InMemoryDatabaseObjectRole::DatabaseOwner,
        })
    );

    drop(opened);
    drop(secondary_slot.try_acquire(
        composition.database_id,
        secondary_manifest,
        composition.manifest,
        &secondary_files,
    )?);
    assert!(!secondary_slot.is_owned());
    Ok(())
}

#[test]
fn owner_and_manifest_identity_rejections_release_acquisition() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new(2, 102)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    assert_eq!(
        slot.try_acquire(
            database_id(200)?,
            composition.manifest_object_id,
            composition.manifest,
            &composition.files,
        )
        .err(),
        Some(InMemoryDatabaseOwnershipError::DatabaseOwnerIdMismatch {
            expected: database_id(200)?,
            actual: composition.database_id,
        })
    );
    assert!(!slot.is_owned());

    let foreign_manifest = manifest(
        database_id(201)?,
        composition.persistent_log_id,
        [21, 22, 23],
        [3, 1, 1],
    )?;
    assert_eq!(
        slot.try_acquire(
            composition.database_id,
            composition.manifest_object_id,
            foreign_manifest,
            &composition.files,
        )
        .err(),
        Some(InMemoryDatabaseOwnershipError::ManifestDatabaseIdMismatch {
            owner: composition.database_id,
            manifest: database_id(201)?,
        })
    );
    assert!(!slot.is_owned());
    drop(composition.acquire(&slot)?);
    Ok(())
}

#[test]
fn missing_and_duplicate_roles_are_rejected_before_child_fields() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new(3, 103)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    assert_eq!(
        slot.try_acquire(
            composition.database_id,
            composition.manifest_object_id,
            composition.manifest,
            &composition.files[..2],
        )
        .err(),
        Some(InMemoryDatabaseOwnershipError::MissingRole {
            role: DatabaseFileRole::RestartCheckpoint,
        })
    );
    assert!(!slot.is_owned());

    let duplicate_page = [
        replace_log_id(composition.files[0], persistent_log_id(999)?),
        composition.files[1],
        composition.files[1],
        composition.files[2],
    ];
    assert_eq!(
        slot.try_acquire(
            composition.database_id,
            composition.manifest_object_id,
            composition.manifest,
            &duplicate_page,
        )
        .err(),
        Some(InMemoryDatabaseOwnershipError::DuplicateRole {
            role: DatabaseFileRole::PageStore,
        })
    );
    assert!(!slot.is_owned());
    Ok(())
}

#[test]
fn reversed_file_ids_and_each_foreign_lineage_fail_in_stable_role_order()
-> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new(4, 104)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    let mut reversed = composition.files;
    reversed[0] = replace_file_id(reversed[0], composition.files[1].file_id());
    reversed[1] = replace_file_id(reversed[1], composition.files[0].file_id());
    assert_eq!(
        slot.try_acquire(
            composition.database_id,
            composition.manifest_object_id,
            composition.manifest,
            &reversed,
        )
        .err(),
        Some(InMemoryDatabaseOwnershipError::FileIdMismatch {
            role: DatabaseFileRole::Wal,
            expected: composition.files[0].file_id(),
            actual: composition.files[1].file_id(),
        })
    );
    assert!(!slot.is_owned());

    for (index, role) in stable_roles().into_iter().enumerate() {
        let foreign = persistent_log_id(800 + index as u128)?;
        let mut files = composition.files;
        files[index] = replace_log_id(files[index], foreign);
        assert_eq!(
            slot.try_acquire(
                composition.database_id,
                composition.manifest_object_id,
                composition.manifest,
                &files,
            )
            .err(),
            Some(InMemoryDatabaseOwnershipError::PersistentLogIdMismatch {
                role,
                expected: composition.persistent_log_id,
                actual: foreign,
            })
        );
        assert!(!slot.is_owned());
    }
    Ok(())
}

#[test]
fn every_child_format_requirement_is_checked() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new(5, 105)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    for (index, role) in stable_roles().into_iter().enumerate() {
        let changed = format_version(composition.files[index].format_version().get() + 1)?;
        let mut files = composition.files;
        files[index] = replace_format(files[index], changed);
        assert_eq!(
            slot.try_acquire(
                composition.database_id,
                composition.manifest_object_id,
                composition.manifest,
                &files,
            )
            .err(),
            Some(
                InMemoryDatabaseOwnershipError::StorageFormatVersionMismatch {
                    role,
                    expected: composition.files[index].format_version(),
                    actual: changed,
                }
            )
        );
        assert!(!slot.is_owned());
    }
    Ok(())
}

#[test]
fn every_later_modeled_role_rejects_alias_with_an_earlier_object() -> Result<(), Box<dyn Error>> {
    let roles = [
        InMemoryDatabaseObjectRole::DatabaseOwner,
        InMemoryDatabaseObjectRole::Manifest,
        InMemoryDatabaseObjectRole::Wal,
        InMemoryDatabaseObjectRole::PageStore,
        InMemoryDatabaseObjectRole::RestartCheckpoint,
    ];
    let base = [
        object_id(1)?,
        object_id(2)?,
        object_id(3)?,
        object_id(4)?,
        object_id(5)?,
    ];
    for (first_index, first) in roles.iter().copied().enumerate() {
        for (second_index, second) in roles.iter().copied().enumerate().skip(first_index + 1) {
            let composition = TestComposition::new(10 + second_index as u128, 110)?;
            let mut objects = base;
            objects[second_index] = objects[first_index];
            let mut world = InMemoryDatabaseOwnershipWorld::new();
            let slot = world.slot(composition.database_id, objects[0])?;
            let files = [
                replace_object_id(composition.files[0], objects[2]),
                replace_object_id(composition.files[1], objects[3]),
                replace_object_id(composition.files[2], objects[4]),
            ];
            assert_eq!(
                slot.try_acquire(
                    composition.database_id,
                    objects[1],
                    composition.manifest,
                    &files,
                )
                .err(),
                Some(InMemoryDatabaseOwnershipError::ObjectAlias { first, second })
            );
            assert!(!slot.is_owned());
        }
    }
    Ok(())
}

#[test]
fn memory_create_is_manifest_last_and_repeated_create_is_explicit() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(100, 200)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;

    let created = composition.create(&slot, None)?;
    let InMemoryDatabaseCreateOutcome::Created(created) = created else {
        return Err(io::Error::other("fresh memory create did not report Created").into());
    };
    assert_eq!(slot.create_phase(), InMemoryDatabaseCreatePhase::Published);
    assert_eq!(created.stage(), DatabaseLifecycleStage::RecoveryRequired);
    assert_eq!(
        created.identity(),
        composition.manifest.composition_identity()
    );
    assert_eq!(
        composition.create(&slot, None).err(),
        Some(InMemoryDatabaseCreateError::Ownership(
            InMemoryDatabaseOwnershipError::Contended {
                database_id: composition.database_id,
            }
        ))
    );
    drop(created);

    let repeated = composition.create(&slot, None)?;
    let InMemoryDatabaseCreateOutcome::AlreadyPublished(repeated) = repeated else {
        return Err(
            io::Error::other("repeated memory create did not report AlreadyPublished").into(),
        );
    };
    assert_eq!(
        repeated.identity(),
        composition.manifest.composition_identity()
    );
    Ok(())
}

#[test]
fn ordinary_memory_open_rejects_unpublished_create_evidence() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(200, 300)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    let fault = InMemoryDatabaseCreateFault::new(
        InMemoryDatabaseCreateBoundary::ManifestCandidatePublication,
        InMemoryDatabaseCreateFaultTiming::AfterEffect,
    );
    assert_eq!(
        composition.create(&slot, Some(fault)).err(),
        Some(InMemoryDatabaseCreateError::InjectedFault(fault))
    );

    assert_eq!(
        composition
            .acquire_recovery_required(&slot, composition.manifest)
            .err(),
        Some(InMemoryDatabaseOwnershipError::UnpublishedCreate {
            phase: InMemoryDatabaseCreatePhase::ManifestCandidate,
        })
    );
    assert!(!slot.is_owned());

    let alternate_owner = world.slot(composition.database_id, object_id(999_998)?)?;
    let alternate_files = [
        replace_object_id(composition.files[0], object_id(999_995)?),
        replace_object_id(composition.files[1], object_id(999_994)?),
        replace_object_id(composition.files[2], object_id(999_993)?),
    ];
    assert_eq!(
        alternate_owner
            .try_acquire_recovery_required(
                composition.database_id,
                object_id(999_996)?,
                composition.manifest,
                &alternate_files,
            )
            .err(),
        Some(InMemoryDatabaseOwnershipError::UnpublishedCreate {
            phase: InMemoryDatabaseCreatePhase::ManifestCandidate,
        })
    );
    assert_eq!(
        composition.create(&alternate_owner, None).err(),
        Some(InMemoryDatabaseCreateError::EvidenceConflict {
            phase: InMemoryDatabaseCreatePhase::ManifestCandidate,
        })
    );
    assert!(!alternate_owner.is_owned());
    Ok(())
}

#[test]
fn published_memory_open_requires_the_selected_modeled_objects() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(201, 301)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    drop(composition.create(&slot, None)?);

    assert_eq!(
        slot.try_acquire_recovery_required(
            composition.database_id,
            object_id(999_999)?,
            composition.manifest,
            &composition.files,
        )
        .err(),
        Some(InMemoryDatabaseOwnershipError::PublishedCreateSelectionMismatch)
    );
    assert!(!slot.is_owned());

    let alternate_owner = world.slot(composition.database_id, object_id(999_998)?)?;
    let alternate_files = [
        replace_object_id(composition.files[0], object_id(999_995)?),
        replace_object_id(composition.files[1], object_id(999_994)?),
        replace_object_id(composition.files[2], object_id(999_993)?),
    ];
    assert_eq!(
        alternate_owner
            .try_acquire_recovery_required(
                composition.database_id,
                object_id(999_996)?,
                composition.manifest,
                &alternate_files,
            )
            .err(),
        Some(InMemoryDatabaseOwnershipError::PublishedCreateSelectionMismatch)
    );
    assert!(!alternate_owner.is_owned());
    Ok(())
}

#[test]
fn owner_before_effect_does_not_bind_an_absent_modeled_object() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(202, 302)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    let fault = InMemoryDatabaseCreateFault::new(
        InMemoryDatabaseCreateBoundary::OwnerPublication,
        InMemoryDatabaseCreateFaultTiming::BeforeEffect,
    );
    assert_eq!(
        composition.create(&slot, Some(fault)).err(),
        Some(InMemoryDatabaseCreateError::InjectedFault(fault))
    );
    assert_eq!(slot.create_phase(), InMemoryDatabaseCreatePhase::Absent);
    drop(world.slot(database_id(999_999)?, composition.owner_object_id)?);
    Ok(())
}

#[test]
fn published_owner_identity_blocks_alternate_memory_owner_slots() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(203, 303)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    let fault = InMemoryDatabaseCreateFault::new(
        InMemoryDatabaseCreateBoundary::OwnerPublication,
        InMemoryDatabaseCreateFaultTiming::AfterEffect,
    );
    assert_eq!(
        composition.create(&slot, Some(fault)).err(),
        Some(InMemoryDatabaseCreateError::InjectedFault(fault))
    );

    let alternate_owner = world.slot(composition.database_id, object_id(999_992)?)?;
    assert_eq!(
        composition
            .acquire_recovery_required(&alternate_owner, composition.manifest)
            .err(),
        Some(InMemoryDatabaseOwnershipError::UnpublishedCreate {
            phase: InMemoryDatabaseCreatePhase::Owner,
        })
    );
    assert_eq!(
        composition.create(&alternate_owner, None).err(),
        Some(InMemoryDatabaseCreateError::EvidenceConflict {
            phase: InMemoryDatabaseCreatePhase::Owner,
        })
    );
    assert!(!alternate_owner.is_owned());
    Ok(())
}

#[test]
fn every_memory_create_fault_timing_resumes_from_exact_phase() -> Result<(), Box<dyn Error>> {
    let boundaries = [
        InMemoryDatabaseCreateBoundary::OwnerPublication,
        InMemoryDatabaseCreateBoundary::ManifestCandidatePublication,
        InMemoryDatabaseCreateBoundary::WalCandidatePublication,
        InMemoryDatabaseCreateBoundary::PageStoreCandidatePublication,
        InMemoryDatabaseCreateBoundary::RestartCheckpointCandidatePublication,
        InMemoryDatabaseCreateBoundary::WalPublication,
        InMemoryDatabaseCreateBoundary::PageStorePublication,
        InMemoryDatabaseCreateBoundary::RestartCheckpointPublication,
        InMemoryDatabaseCreateBoundary::ManifestPublication,
    ];
    let timings = [
        InMemoryDatabaseCreateFaultTiming::BeforeEffect,
        InMemoryDatabaseCreateFaultTiming::AfterEffect,
        InMemoryDatabaseCreateFaultTiming::OutcomeIndeterminateBeforeEffect,
        InMemoryDatabaseCreateFaultTiming::OutcomeIndeterminateAfterEffect,
    ];

    for (boundary_index, boundary) in boundaries.into_iter().enumerate() {
        for (timing_index, timing) in timings.into_iter().enumerate() {
            let composition = TestComposition::new_create(
                300 + (boundary_index * timings.len() + timing_index) as u128,
                400 + (boundary_index * timings.len() + timing_index) as u128,
            )?;
            let mut world = InMemoryDatabaseOwnershipWorld::new();
            let slot = composition.slot(&mut world)?;
            let fault = InMemoryDatabaseCreateFault::new(boundary, timing);
            let error = composition
                .create(&slot, Some(fault))
                .err()
                .ok_or_else(|| io::Error::other("armed memory create fault did not fire"))?;
            assert_eq!(error, InMemoryDatabaseCreateError::InjectedFault(fault));
            assert_eq!(
                error.is_outcome_indeterminate(),
                timing.is_outcome_indeterminate()
            );
            assert_eq!(
                slot.create_phase(),
                expected_memory_fault_phase(boundary, timing)
            );

            let resumed = composition.create(&slot, None)?;
            match (boundary, timing, resumed) {
                (
                    InMemoryDatabaseCreateBoundary::ManifestPublication,
                    InMemoryDatabaseCreateFaultTiming::AfterEffect
                    | InMemoryDatabaseCreateFaultTiming::OutcomeIndeterminateAfterEffect,
                    InMemoryDatabaseCreateOutcome::AlreadyPublished(database),
                )
                | (_, _, InMemoryDatabaseCreateOutcome::Created(database)) => drop(database),
                _ => return Err(io::Error::other("memory retry returned wrong outcome").into()),
            }
            assert_eq!(slot.create_phase(), InMemoryDatabaseCreatePhase::Published);
        }
    }
    Ok(())
}

#[test]
fn memory_create_conflicts_and_invalid_inputs_do_not_normalize_evidence()
-> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(500, 600)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    let fault = InMemoryDatabaseCreateFault::new(
        InMemoryDatabaseCreateBoundary::ManifestCandidatePublication,
        InMemoryDatabaseCreateFaultTiming::AfterEffect,
    );
    assert_eq!(
        composition.create(&slot, Some(fault)).err(),
        Some(InMemoryDatabaseCreateError::InjectedFault(fault))
    );
    let mut conflicting_files = composition.files;
    conflicting_files[0] = replace_object_id(conflicting_files[0], object_id(999_999)?);
    assert_eq!(
        slot.try_create_recovery_required(
            composition.manifest_object_id,
            composition.manifest,
            &conflicting_files,
            None,
        )
        .err(),
        Some(InMemoryDatabaseCreateError::EvidenceConflict {
            phase: InMemoryDatabaseCreatePhase::ManifestCandidate,
        })
    );
    assert_eq!(
        slot.create_phase(),
        InMemoryDatabaseCreatePhase::ManifestCandidate
    );

    let invalid = TestComposition::new_create(501, 601)?;
    let invalid_slot = invalid.slot(&mut world)?;
    let successor = invalid.manifest.next_recovery_required()?;
    assert_eq!(
        invalid_slot
            .try_create_recovery_required(
                invalid.manifest_object_id,
                successor,
                &invalid.files,
                None,
            )
            .err(),
        Some(InMemoryDatabaseCreateError::ManifestRequirement(
            InMemoryDatabaseCreateManifestError::LifecycleGeneration { actual: 2 }
        ))
    );
    assert_eq!(
        invalid_slot.create_phase(),
        InMemoryDatabaseCreatePhase::Absent
    );

    let aliased = replace_object_id(invalid.files[0], invalid.manifest_object_id);
    assert!(matches!(
        invalid_slot.try_create_recovery_required(
            invalid.manifest_object_id,
            invalid.manifest,
            &[aliased, invalid.files[1], invalid.files[2]],
            None,
        ),
        Err(InMemoryDatabaseCreateError::Ownership(
            InMemoryDatabaseOwnershipError::ObjectAlias {
                first: InMemoryDatabaseObjectRole::Manifest,
                second: InMemoryDatabaseObjectRole::Wal,
            }
        ))
    ));
    assert_eq!(
        invalid_slot.create_phase(),
        InMemoryDatabaseCreatePhase::Absent
    );
    Ok(())
}

#[test]
fn live_open_bootstraps_absent_checkpoint_and_retains_context_and_ownership()
-> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(70, 170)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    drop(composition.create(&slot, None)?);

    let log = InMemoryCommitLog::<1>::with_persistent_lineage_id(composition.persistent_log_id);
    let store = InMemoryPageStore::new(&log);
    let storage = InMemoryDatabaseRecoveryStorage::new(
        log,
        store,
        InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty(),
    );
    let mut phases = Vec::new();
    let live = open_live_in_memory_database_with_observer(
        InMemoryDatabaseLiveOpenRequest::new(
            &slot,
            composition.database_id,
            composition.manifest_object_id,
            composition.manifest,
            &composition.files,
            storage,
            compatibility_context("memory-live")?,
        ),
        |phase| phases.push(phase),
    )?;

    assert_eq!(live.stage(), DatabaseLifecycleStage::Live);
    assert_eq!(live.identity(), composition.manifest.composition_identity());
    assert_eq!(
        live.compatibility_context().target_id().as_str(),
        "memory-live"
    );
    assert_eq!(live.transaction_parts().1.generation(), 0);
    assert!(slot.is_owned());
    assert_eq!(
        phases,
        [
            InMemoryDatabaseOpenPhase::CompositionValidated,
            InMemoryDatabaseOpenPhase::Recovery(
                TransactionPageStorageRecoveryHandoffPhase::CheckpointAbsent,
            ),
            InMemoryDatabaseOpenPhase::Recovery(
                TransactionPageStorageRecoveryHandoffPhase::FullRecoveryCompleted,
            ),
            InMemoryDatabaseOpenPhase::Recovery(
                TransactionPageStorageRecoveryHandoffPhase::FullRecoveryRestartAnalyzed,
            ),
            InMemoryDatabaseOpenPhase::Recovery(
                TransactionPageStorageRecoveryHandoffPhase::CheckpointBootstrapped,
            ),
            InMemoryDatabaseOpenPhase::Recovery(
                TransactionPageStorageRecoveryHandoffPhase::CheckpointSelected,
            ),
            InMemoryDatabaseOpenPhase::Recovery(
                TransactionPageStorageRecoveryHandoffPhase::ReplayPlanned,
            ),
            InMemoryDatabaseOpenPhase::Recovery(
                TransactionPageStorageRecoveryHandoffPhase::PageRepairsPrepared,
            ),
            InMemoryDatabaseOpenPhase::Recovery(
                TransactionPageStorageRecoveryHandoffPhase::PageRepairsCompleted,
            ),
            InMemoryDatabaseOpenPhase::Recovery(
                TransactionPageStorageRecoveryHandoffPhase::TransactionStateRestored,
            ),
            InMemoryDatabaseOpenPhase::Recovery(
                TransactionPageStorageRecoveryHandoffPhase::RestartCompleted,
            ),
            InMemoryDatabaseOpenPhase::Recovery(
                TransactionPageStorageRecoveryHandoffPhase::WalRetentionAnalyzed,
            ),
            InMemoryDatabaseOpenPhase::LiveReleased,
        ]
    );
    assert_eq!(
        composition.acquire(&slot).err(),
        Some(InMemoryDatabaseOwnershipError::Contended {
            database_id: composition.database_id,
        })
    );

    drop(live);
    assert!(!slot.is_owned());
    Ok(())
}

#[test]
fn empty_live_close_preparation_binds_adjacent_certificate_and_retains_ownership()
-> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(73, 173)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    drop(composition.create(&slot, None)?);
    let live = open_empty_live(
        &composition,
        &slot,
        InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty(),
        "memory-close",
    )?;

    let pending = live
        .prepare_close()
        .map_err(|_| io::Error::other("empty memory database close preparation failed"))?;

    assert_eq!(pending.stage(), DatabaseLifecycleStage::ClosePending);
    assert_eq!(
        pending.identity(),
        composition.manifest.composition_identity()
    );
    assert_eq!(
        pending.target_identity().lifecycle_generation().get(),
        pending.identity().lifecycle_generation().get() + 1
    );
    assert_eq!(
        pending.target_identity().storage_identity(),
        pending.identity().storage_identity()
    );
    assert_eq!(
        pending.target_manifest().lifecycle_state(),
        DatabaseManifestLifecycleState::Clean(pending.certificate())
    );
    assert_eq!(
        pending
            .target_manifest()
            .require_successor_of(pending.manifest()),
        Ok(())
    );
    assert_eq!(pending.manifest(), composition.manifest);
    assert_eq!(
        pending.compatibility_context().target_id().as_str(),
        "memory-close"
    );
    let certificate = pending.certificate();
    assert_eq!(
        certificate.source_generation(),
        composition
            .manifest
            .composition_identity()
            .lifecycle_generation()
    );
    assert_eq!(certificate.durable_wal_frontier(), None);
    assert_eq!(certificate.allocated_transaction_epoch_high_water(), 1);
    assert_ne!(certificate.checkpoint_anchor_version(), 0);
    assert_eq!(certificate.transaction_entry_count(), 0);
    assert_eq!(certificate.page_entry_count(), 0);
    assert!(slot.is_owned());
    assert_eq!(
        composition.acquire(&slot).err(),
        Some(InMemoryDatabaseOwnershipError::Contended {
            database_id: composition.database_id,
        })
    );

    let abandoned = pending.abandon();
    assert_eq!(abandoned.stage(), DatabaseLifecycleStage::Abandoned);
    assert_eq!(
        abandoned.identity(),
        composition.manifest.composition_identity()
    );
    assert!(!slot.is_owned());
    Ok(())
}

#[test]
fn committed_nonempty_live_close_binds_fresh_frontier_and_transaction_count()
-> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(79, 179)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    drop(composition.create(&slot, None)?);
    let mut live = open_empty_live(
        &composition,
        &slot,
        InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty(),
        "memory-nonempty-close",
    )?;
    let (coordinator, log, _) = live.transaction_parts_mut();
    let active = coordinator.begin()?;
    coordinator.commit(active, log)?;

    let pending = live
        .prepare_close()
        .map_err(|_| io::Error::other("committed memory database close preparation failed"))?;
    let certificate = pending.certificate();
    assert_eq!(certificate.durable_wal_frontier(), Some(1));
    assert_eq!(certificate.allocated_transaction_epoch_high_water(), 1);
    assert_eq!(certificate.transaction_entry_count(), 1);
    assert_eq!(certificate.page_entry_count(), 0);
    assert_eq!(
        pending.target_manifest().lifecycle_state(),
        DatabaseManifestLifecycleState::Clean(certificate)
    );
    drop(pending.abandon());
    assert!(!slot.is_owned());
    Ok(())
}

#[test]
fn clean_manifest_publication_reaches_closed_with_exact_memory_selection()
-> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(80, 180)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    drop(composition.create(&slot, None)?);
    let mut live = open_empty_live(
        &composition,
        &slot,
        InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty(),
        "memory-clean-publication",
    )?;
    let active = live.transaction_parts_mut().0.begin()?;
    let (coordinator, log, _) = live.transaction_parts_mut();
    coordinator.commit(active, log)?;
    let pending = live
        .prepare_close()
        .map_err(|_| io::Error::other("memory close preparation failed"))?;
    let target = pending.target_manifest();

    let closed = pending
        .close()
        .map_err(|_| io::Error::other("memory clean-manifest publication failed"))?;

    assert_eq!(closed.stage(), DatabaseLifecycleStage::Closed);
    assert_eq!(closed.identity(), target.composition_identity());
    assert_eq!(closed.manifest(), target);
    assert_eq!(
        closed.compatibility_context().target_id().as_str(),
        "memory-clean-publication"
    );
    assert_eq!(slot.close_candidate_manifest(), Some(target));
    assert_eq!(slot.selected_manifest(), Some(target));
    assert_eq!(slot.synchronized_manifest(), Some(target));
    assert!(slot.is_owned());
    assert_eq!(
        composition.acquire(&slot).err(),
        Some(InMemoryDatabaseOwnershipError::Contended {
            database_id: composition.database_id,
        })
    );

    drop(closed);
    assert!(!slot.is_owned());
    assert_eq!(
        composition.acquire(&slot).err(),
        Some(InMemoryDatabaseOwnershipError::SelectedManifestMismatch)
    );
    drop(slot.try_acquire(
        composition.database_id,
        composition.manifest_object_id,
        target,
        &composition.files,
    )?);
    assert!(!slot.is_owned());
    Ok(())
}

#[test]
fn every_memory_clean_manifest_fault_is_terminal_and_freshly_reopenable()
-> Result<(), Box<dyn Error>> {
    let boundaries = [
        InMemoryDatabaseCloseBoundary::CandidatePublication,
        InMemoryDatabaseCloseBoundary::ManifestPublication,
        InMemoryDatabaseCloseBoundary::SelectedManifestVerification,
        InMemoryDatabaseCloseBoundary::PublicationSynchronization,
    ];
    let timings = [
        InMemoryDatabaseCloseFaultTiming::BeforeEffect,
        InMemoryDatabaseCloseFaultTiming::AfterEffect,
        InMemoryDatabaseCloseFaultTiming::OutcomeIndeterminateBeforeEffect,
        InMemoryDatabaseCloseFaultTiming::OutcomeIndeterminateAfterEffect,
    ];

    for (boundary_index, boundary) in boundaries.into_iter().enumerate() {
        for (timing_index, timing) in timings.into_iter().enumerate() {
            let offset = (boundary_index * timings.len() + timing_index) as u128;
            let composition = TestComposition::new_create(800 + offset, 900 + offset)?;
            let mut world = InMemoryDatabaseOwnershipWorld::new();
            let slot = composition.slot(&mut world)?;
            drop(composition.create(&slot, None)?);
            let live = open_empty_live(
                &composition,
                &slot,
                InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty(),
                "memory-clean-fault",
            )?;
            let pending = live
                .prepare_close()
                .map_err(|_| io::Error::other("memory close preparation failed"))?;
            let target = pending.target_manifest();
            let fault = InMemoryDatabaseCloseFault::new(boundary, timing);

            let failure = pending
                .close_with_fault(fault)
                .err()
                .ok_or_else(|| io::Error::other("armed memory close fault did not fire"))?;

            let expected_state = match (boundary, timing) {
                (InMemoryDatabaseCloseBoundary::CandidatePublication, _) => {
                    DatabaseCleanManifestPublicationState::SourceSelected
                }
                (
                    InMemoryDatabaseCloseBoundary::ManifestPublication,
                    InMemoryDatabaseCloseFaultTiming::BeforeEffect,
                ) => DatabaseCleanManifestPublicationState::SourceSelected,
                (
                    InMemoryDatabaseCloseBoundary::ManifestPublication,
                    InMemoryDatabaseCloseFaultTiming::OutcomeIndeterminateBeforeEffect
                    | InMemoryDatabaseCloseFaultTiming::OutcomeIndeterminateAfterEffect,
                ) => DatabaseCleanManifestPublicationState::SelectionIndeterminate,
                (
                    InMemoryDatabaseCloseBoundary::ManifestPublication,
                    InMemoryDatabaseCloseFaultTiming::AfterEffect,
                )
                | (InMemoryDatabaseCloseBoundary::SelectedManifestVerification, _)
                | (
                    InMemoryDatabaseCloseBoundary::PublicationSynchronization,
                    InMemoryDatabaseCloseFaultTiming::BeforeEffect
                    | InMemoryDatabaseCloseFaultTiming::OutcomeIndeterminateBeforeEffect
                    | InMemoryDatabaseCloseFaultTiming::OutcomeIndeterminateAfterEffect,
                ) => DatabaseCleanManifestPublicationState::TargetSelectedDurabilityIndeterminate,
                (
                    InMemoryDatabaseCloseBoundary::PublicationSynchronization,
                    InMemoryDatabaseCloseFaultTiming::AfterEffect,
                ) => DatabaseCleanManifestPublicationState::TargetDurable,
            };
            assert_eq!(failure.state(), expected_state);
            assert_eq!(
                failure.source_identity(),
                composition.manifest.composition_identity()
            );
            assert_eq!(failure.target_identity(), target.composition_identity());
            assert!(slot.is_owned());

            let effect_applied = matches!(
                timing,
                InMemoryDatabaseCloseFaultTiming::AfterEffect
                    | InMemoryDatabaseCloseFaultTiming::OutcomeIndeterminateAfterEffect
            );
            let candidate_selected = !matches!(
                boundary,
                InMemoryDatabaseCloseBoundary::CandidatePublication
            ) || effect_applied;
            let target_selected =
                matches!(
                    boundary,
                    InMemoryDatabaseCloseBoundary::SelectedManifestVerification
                        | InMemoryDatabaseCloseBoundary::PublicationSynchronization
                ) || (matches!(boundary, InMemoryDatabaseCloseBoundary::ManifestPublication)
                    && effect_applied);
            let target_synchronized = matches!(
                boundary,
                InMemoryDatabaseCloseBoundary::PublicationSynchronization
            ) && effect_applied;
            assert_eq!(
                slot.close_candidate_manifest(),
                candidate_selected.then_some(target)
            );
            assert_eq!(
                slot.selected_manifest(),
                Some(if target_selected {
                    target
                } else {
                    composition.manifest
                })
            );
            assert_eq!(
                slot.synchronized_manifest(),
                target_synchronized.then_some(target)
            );

            let abandoned = failure.abandon();
            assert_eq!(abandoned.state(), expected_state);
            assert_eq!(abandoned.stage(), DatabaseLifecycleStage::Abandoned);
            assert!(!slot.is_owned());

            let selected_manifest = if target_selected {
                target
            } else {
                composition.manifest
            };
            drop(slot.try_acquire(
                composition.database_id,
                composition.manifest_object_id,
                selected_manifest,
                &composition.files,
            )?);
            assert!(!slot.is_owned());
        }
    }
    Ok(())
}

#[test]
fn published_memory_create_rejects_a_nonselected_manifest() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(81, 181)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    drop(composition.create(&slot, None)?);
    let unselected = composition.manifest.next_recovery_required()?;

    assert_eq!(
        slot.try_acquire(
            composition.database_id,
            composition.manifest_object_id,
            unselected,
            &composition.files,
        )
        .err(),
        Some(InMemoryDatabaseOwnershipError::SelectedManifestMismatch)
    );
    assert!(!slot.is_owned());
    Ok(())
}

#[test]
fn active_transaction_close_failure_is_terminal_until_explicit_abandon()
-> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(74, 174)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    drop(composition.create(&slot, None)?);
    let mut live = open_empty_live(
        &composition,
        &slot,
        InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty(),
        "memory-active-close",
    )?;
    let active = live.transaction_parts_mut().0.begin()?;
    let active_id = active.transaction_id();
    drop(active);

    let failure = live
        .prepare_close()
        .err()
        .ok_or_else(|| io::Error::other("active transaction unexpectedly closed cleanly"))?;
    assert_eq!(
        failure.identity(),
        composition.manifest.composition_identity()
    );
    match failure.cause() {
        DatabaseClosePreparationFailureCause::Transaction(transaction) => {
            assert!(matches!(
                transaction.error(),
                DurableTransactionPageStorageCleanClosePreparationError::BeforePublication(
                    DurableTransactionPageStorageCleanCloseBeforePublicationError::Evidence(
                        DurableTransactionPageStorageCleanCloseEvidenceError::CoordinatorLifecycleNotTerminal {
                            transaction,
                            status: TransactionLifecycleStatus::Active,
                        }
                    )
                ) if *transaction == active_id
            ));
        }
        _ => return Err(io::Error::other("active transaction returned wrong close cause").into()),
    }
    assert!(slot.is_owned());

    let abandoned = failure.abandon();
    assert_eq!(abandoned.stage(), DatabaseLifecycleStage::Abandoned);
    assert!(!slot.is_owned());
    Ok(())
}

#[test]
fn every_clean_close_candidate_fault_is_terminally_outcome_indeterminate()
-> Result<(), Box<dyn Error>> {
    let faults = [
        TransactionPageStorageCleanCloseCheckpointFaultPoint::BeforePublish,
        TransactionPageStorageCleanCloseCheckpointFaultPoint::AfterPublish,
        TransactionPageStorageCleanCloseCheckpointFaultPoint::BeforeLoad,
    ];
    for (index, fault) in faults.into_iter().enumerate() {
        let composition = TestComposition::new_create(75 + index as u128, 175 + index as u128)?;
        let mut world = InMemoryDatabaseOwnershipWorld::new();
        let slot = composition.slot(&mut world)?;
        drop(composition.create(&slot, None)?);
        let mut checkpoint =
            InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
        checkpoint.arm_clean_close_fault(fault)?;
        let live = open_empty_live(
            &composition,
            &slot,
            checkpoint,
            "memory-close-candidate-fault",
        )?;

        let failure = live
            .prepare_close()
            .err()
            .ok_or_else(|| io::Error::other("armed clean-close candidate fault did not fire"))?;
        match failure.cause() {
            DatabaseClosePreparationFailureCause::Transaction(transaction) => {
                assert!(transaction.error().outcome_is_indeterminate());
                match (fault, transaction.error()) {
                    (
                        TransactionPageStorageCleanCloseCheckpointFaultPoint::BeforePublish
                        | TransactionPageStorageCleanCloseCheckpointFaultPoint::AfterPublish,
                        DurableTransactionPageStorageCleanClosePreparationError::OutcomeIndeterminate(
                            DurableTransactionPageStorageCleanCloseOutcomeIndeterminateError::Publication(
                                publication,
                            ),
                        ),
                    ) => assert_eq!(
                        publication.cause(),
                        &InMemoryTransactionRestartCheckpointCompletenessBaselinePublicationError::CleanCloseInjectedFault(fault)
                    ),
                    (
                        TransactionPageStorageCleanCloseCheckpointFaultPoint::BeforeLoad,
                        DurableTransactionPageStorageCleanClosePreparationError::OutcomeIndeterminate(
                            DurableTransactionPageStorageCleanCloseOutcomeIndeterminateError::CheckpointReload(
                                source,
                            ),
                        ),
                    ) => assert_eq!(
                        source,
                        &InMemoryTransactionRestartCheckpointCompletenessBaselineSourceError::CleanCloseInjectedFault(fault)
                    ),
                    _ => {
                        return Err(
                            io::Error::other(
                                "clean-close fault returned wrong transaction cause",
                            )
                            .into(),
                        );
                    }
                }
            }
            _ => {
                return Err(io::Error::other(
                    "clean-close candidate fault returned wrong database cause",
                )
                .into());
            }
        }
        assert!(slot.is_owned());
        drop(failure.abandon());
        assert!(!slot.is_owned());
    }
    Ok(())
}

#[test]
fn lifecycle_generation_exhaustion_precedes_transaction_close_effect() -> Result<(), Box<dyn Error>>
{
    let mut composition = TestComposition::new_create(76, 176)?;
    let source = composition.manifest;
    composition.manifest = DatabaseManifest::recovery_required(
        DatabaseCompositionIdentity::new(
            composition.database_id,
            DatabaseLifecycleGeneration::new(u64::MAX)
                .ok_or_else(|| io::Error::other("maximum generation is zero"))?,
            composition.persistent_log_id,
            &source.composition_identity().ordered_files(),
        )?,
        source.storage_formats(),
        source.required_features(),
    );
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    let live = open_empty_live(
        &composition,
        &slot,
        InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty(),
        "memory-exhausted-close",
    )?;

    let failure = live
        .prepare_close()
        .err()
        .ok_or_else(|| io::Error::other("exhausted lifecycle generation advanced"))?;
    match failure.cause() {
        DatabaseClosePreparationFailureCause::Preflight(
            DatabaseClosePreparationPreflightError::LifecycleGeneration(source),
        ) => assert_eq!(source.current.get(), u64::MAX),
        _ => return Err(io::Error::other("generation exhaustion returned wrong cause").into()),
    }
    assert!(slot.is_owned());
    drop(failure.abandon());
    assert!(!slot.is_owned());
    Ok(())
}

#[test]
fn clean_source_manifest_is_rejected_before_recovery_required_authority()
-> Result<(), Box<dyn Error>> {
    let mut composition = TestComposition::new_create(78, 178)?;
    let certificate = DatabaseCleanCloseCertificate::new(
        composition
            .manifest
            .composition_identity()
            .lifecycle_generation(),
        None,
        1,
        1,
        1,
        0,
        0,
    )?;
    composition.manifest = composition.manifest.next_clean(certificate)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    let selected = composition.acquire(&slot)?;
    assert_eq!(selected.manifest(), composition.manifest);
    drop(selected);
    assert_eq!(
        composition
            .acquire_recovery_required(&slot, composition.manifest)
            .err(),
        Some(InMemoryDatabaseOwnershipError::StorageBinding(
            DatabaseRecoveryRequiredBindingRejection::ManifestLifecycle {
                actual: DatabaseManifestLifecycleState::Clean(certificate),
            }
        ))
    );
    assert!(!slot.is_owned());
    Ok(())
}

#[test]
fn live_abandon_relinquishes_ownership_without_close_preparation() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(77, 177)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    drop(composition.create(&slot, None)?);
    let live = open_empty_live(
        &composition,
        &slot,
        InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty(),
        "memory-abandon",
    )?;

    let abandoned = live.abandon();
    assert_eq!(abandoned.stage(), DatabaseLifecycleStage::Abandoned);
    assert_eq!(
        abandoned.identity(),
        composition.manifest.composition_identity()
    );
    assert!(!slot.is_owned());
    Ok(())
}

#[test]
fn rejected_checkpoint_never_bootstraps_or_releases_live() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(71, 171)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    drop(composition.create(&slot, None)?);

    let log = InMemoryCommitLog::<1>::with_persistent_lineage_id(composition.persistent_log_id);
    let store = InMemoryPageStore::new(&log);
    let mut checkpoint = InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty();
    checkpoint.arm_fault(RestartCheckpointCompletenessBaselineSourceFaultPoint::BeforeLoad)?;
    let storage = InMemoryDatabaseRecoveryStorage::new(log, store, checkpoint);
    let error = open_live_in_memory_database(InMemoryDatabaseLiveOpenRequest::new(
        &slot,
        composition.database_id,
        composition.manifest_object_id,
        composition.manifest,
        &composition.files,
        storage,
        compatibility_context("memory-rejected")?,
    ))
    .err()
    .ok_or_else(|| io::Error::other("checkpoint source fault released Live"))?;
    assert_eq!(
        error.recovery_phase(),
        Some(TransactionPageStorageRecoveryHandoffPhase::CheckpointSelected)
    );
    assert!(slot.is_owned());
    drop(error);
    assert!(!slot.is_owned());
    Ok(())
}

#[test]
fn foreign_concrete_wal_cannot_satisfy_modeled_database_recovery() -> Result<(), Box<dyn Error>> {
    let composition = TestComposition::new_create(72, 172)?;
    let mut world = InMemoryDatabaseOwnershipWorld::new();
    let slot = composition.slot(&mut world)?;
    drop(composition.create(&slot, None)?);

    let foreign_log = InMemoryCommitLog::<1>::with_persistent_lineage_id(persistent_log_id(9_172)?);
    let foreign_store = InMemoryPageStore::new(&foreign_log);
    let storage = InMemoryDatabaseRecoveryStorage::new(
        foreign_log,
        foreign_store,
        InMemoryTransactionRestartCheckpointCompletenessBaselineSource::empty(),
    );
    let error = open_live_in_memory_database(InMemoryDatabaseLiveOpenRequest::new(
        &slot,
        composition.database_id,
        composition.manifest_object_id,
        composition.manifest,
        &composition.files,
        storage,
        compatibility_context("memory-foreign")?,
    ))
    .err()
    .ok_or_else(|| io::Error::other("foreign concrete WAL released Live"))?;
    assert_eq!(error.recovery_phase(), None);
    assert!(slot.is_owned());
    drop(error);
    assert!(!slot.is_owned());
    Ok(())
}

fn open_empty_live(
    composition: &TestComposition,
    slot: &InMemoryDatabaseOwnershipSlot,
    checkpoint: InMemoryTransactionRestartCheckpointCompletenessBaselineSource,
    target_id: &str,
) -> Result<LiveInMemoryDatabase<1>, Box<dyn Error>> {
    let log = InMemoryCommitLog::<1>::with_persistent_lineage_id(composition.persistent_log_id);
    let store = InMemoryPageStore::new(&log);
    Ok(open_live_in_memory_database(
        InMemoryDatabaseLiveOpenRequest::new(
            slot,
            composition.database_id,
            composition.manifest_object_id,
            composition.manifest,
            &composition.files,
            InMemoryDatabaseRecoveryStorage::new(log, store, checkpoint),
            compatibility_context(target_id)?,
        ),
    )?)
}

struct TestComposition {
    database_id: DatabaseId,
    persistent_log_id: PersistentLogId,
    owner_object_id: InMemoryDatabaseObjectId,
    manifest_object_id: InMemoryDatabaseObjectId,
    manifest: DatabaseManifest,
    files: [InMemoryDatabaseFileObservation; 3],
}

impl TestComposition {
    fn new(database_value: u128, persistent_value: u128) -> Result<Self, Box<dyn Error>> {
        Self::new_with_formats(database_value, persistent_value, [3, 1, 1])
    }

    fn new_create(database_value: u128, persistent_value: u128) -> Result<Self, Box<dyn Error>> {
        Self::new_with_formats(database_value, persistent_value, [5, 2, 2])
    }

    fn new_with_formats(
        database_value: u128,
        persistent_value: u128,
        formats: [u16; 3],
    ) -> Result<Self, Box<dyn Error>> {
        let database_id = database_id(database_value)?;
        let persistent_log_id = persistent_log_id(persistent_value)?;
        let file_values = [
            1_000 + database_value,
            2_000 + database_value,
            3_000 + database_value,
        ];
        let manifest = manifest(database_id, persistent_log_id, file_values, formats)?;
        let files = [
            file_observation(
                DatabaseFileRole::Wal,
                file_values[0],
                3_000 + database_value,
                persistent_log_id,
                formats[0],
            )?,
            file_observation(
                DatabaseFileRole::PageStore,
                file_values[1],
                4_000 + database_value,
                persistent_log_id,
                formats[1],
            )?,
            file_observation(
                DatabaseFileRole::RestartCheckpoint,
                file_values[2],
                5_000 + database_value,
                persistent_log_id,
                formats[2],
            )?,
        ];
        Ok(Self {
            database_id,
            persistent_log_id,
            owner_object_id: object_id(1_000 + database_value)?,
            manifest_object_id: object_id(2_000 + database_value)?,
            manifest,
            files,
        })
    }

    fn slot(
        &self,
        world: &mut InMemoryDatabaseOwnershipWorld,
    ) -> Result<InMemoryDatabaseOwnershipSlot, InMemoryDatabaseOwnershipSlotError> {
        world.slot(self.database_id, self.owner_object_id)
    }

    fn acquire(
        &self,
        slot: &InMemoryDatabaseOwnershipSlot,
    ) -> Result<
        ntsql_storage_memory::InMemoryDatabaseOwnershipSelection,
        InMemoryDatabaseOwnershipError,
    > {
        slot.try_acquire(
            self.database_id,
            self.manifest_object_id,
            self.manifest,
            &self.files,
        )
    }

    fn acquire_recovery_required(
        &self,
        slot: &InMemoryDatabaseOwnershipSlot,
        manifest: DatabaseManifest,
    ) -> Result<
        ntsql_storage_memory::RecoveryRequiredInMemoryDatabase,
        InMemoryDatabaseOwnershipError,
    > {
        slot.try_acquire_recovery_required(
            self.database_id,
            self.manifest_object_id,
            manifest,
            &self.files,
        )
    }

    fn create(
        &self,
        slot: &InMemoryDatabaseOwnershipSlot,
        fault: Option<InMemoryDatabaseCreateFault>,
    ) -> Result<InMemoryDatabaseCreateOutcome, InMemoryDatabaseCreateError> {
        slot.try_create_recovery_required(
            self.manifest_object_id,
            self.manifest,
            &self.files,
            fault,
        )
    }
}

const fn expected_memory_fault_phase(
    boundary: InMemoryDatabaseCreateBoundary,
    timing: InMemoryDatabaseCreateFaultTiming,
) -> InMemoryDatabaseCreatePhase {
    let after = matches!(
        timing,
        InMemoryDatabaseCreateFaultTiming::AfterEffect
            | InMemoryDatabaseCreateFaultTiming::OutcomeIndeterminateAfterEffect
    );
    match (boundary, after) {
        (InMemoryDatabaseCreateBoundary::OwnerPublication, false) => {
            InMemoryDatabaseCreatePhase::Absent
        }
        (InMemoryDatabaseCreateBoundary::OwnerPublication, true)
        | (InMemoryDatabaseCreateBoundary::ManifestCandidatePublication, false) => {
            InMemoryDatabaseCreatePhase::Owner
        }
        (InMemoryDatabaseCreateBoundary::ManifestCandidatePublication, true)
        | (InMemoryDatabaseCreateBoundary::WalCandidatePublication, false) => {
            InMemoryDatabaseCreatePhase::ManifestCandidate
        }
        (InMemoryDatabaseCreateBoundary::WalCandidatePublication, true)
        | (InMemoryDatabaseCreateBoundary::PageStoreCandidatePublication, false) => {
            InMemoryDatabaseCreatePhase::WalCandidate
        }
        (InMemoryDatabaseCreateBoundary::PageStoreCandidatePublication, true)
        | (InMemoryDatabaseCreateBoundary::RestartCheckpointCandidatePublication, false) => {
            InMemoryDatabaseCreatePhase::PageStoreCandidate
        }
        (InMemoryDatabaseCreateBoundary::RestartCheckpointCandidatePublication, true)
        | (InMemoryDatabaseCreateBoundary::WalPublication, false) => {
            InMemoryDatabaseCreatePhase::RestartCheckpointCandidate
        }
        (InMemoryDatabaseCreateBoundary::WalPublication, true)
        | (InMemoryDatabaseCreateBoundary::PageStorePublication, false) => {
            InMemoryDatabaseCreatePhase::WalPublished
        }
        (InMemoryDatabaseCreateBoundary::PageStorePublication, true)
        | (InMemoryDatabaseCreateBoundary::RestartCheckpointPublication, false) => {
            InMemoryDatabaseCreatePhase::PageStorePublished
        }
        (InMemoryDatabaseCreateBoundary::RestartCheckpointPublication, true)
        | (InMemoryDatabaseCreateBoundary::ManifestPublication, false) => {
            InMemoryDatabaseCreatePhase::ChildrenPublished
        }
        (InMemoryDatabaseCreateBoundary::ManifestPublication, true) => {
            InMemoryDatabaseCreatePhase::Published
        }
    }
}

fn stable_roles() -> [DatabaseFileRole; 3] {
    [
        DatabaseFileRole::Wal,
        DatabaseFileRole::PageStore,
        DatabaseFileRole::RestartCheckpoint,
    ]
}

fn file_observation(
    role: DatabaseFileRole,
    file_value: u128,
    object_value: u128,
    persistent_log_id: PersistentLogId,
    format_value: u16,
) -> Result<InMemoryDatabaseFileObservation, io::Error> {
    Ok(InMemoryDatabaseFileObservation::new(
        role,
        database_file_id(file_value)?,
        object_id(object_value)?,
        persistent_log_id,
        format_version(format_value)?,
    ))
}

fn replace_file_id(
    file: InMemoryDatabaseFileObservation,
    file_id: DatabaseFileId,
) -> InMemoryDatabaseFileObservation {
    InMemoryDatabaseFileObservation::new(
        file.role(),
        file_id,
        file.object_id(),
        file.persistent_log_id(),
        file.format_version(),
    )
}

fn replace_object_id(
    file: InMemoryDatabaseFileObservation,
    object_id: InMemoryDatabaseObjectId,
) -> InMemoryDatabaseFileObservation {
    InMemoryDatabaseFileObservation::new(
        file.role(),
        file.file_id(),
        object_id,
        file.persistent_log_id(),
        file.format_version(),
    )
}

fn replace_log_id(
    file: InMemoryDatabaseFileObservation,
    persistent_log_id: PersistentLogId,
) -> InMemoryDatabaseFileObservation {
    InMemoryDatabaseFileObservation::new(
        file.role(),
        file.file_id(),
        file.object_id(),
        persistent_log_id,
        file.format_version(),
    )
}

fn replace_format(
    file: InMemoryDatabaseFileObservation,
    format: DatabaseStorageFormatVersion,
) -> InMemoryDatabaseFileObservation {
    InMemoryDatabaseFileObservation::new(
        file.role(),
        file.file_id(),
        file.object_id(),
        file.persistent_log_id(),
        format,
    )
}

fn manifest(
    database_id: DatabaseId,
    persistent_log_id: PersistentLogId,
    file_values: [u128; 3],
    formats: [u16; 3],
) -> Result<DatabaseManifest, Box<dyn Error>> {
    let files = [
        DatabaseFileIdentity::new(DatabaseFileRole::Wal, database_file_id(file_values[0])?),
        DatabaseFileIdentity::new(
            DatabaseFileRole::PageStore,
            database_file_id(file_values[1])?,
        ),
        DatabaseFileIdentity::new(
            DatabaseFileRole::RestartCheckpoint,
            database_file_id(file_values[2])?,
        ),
    ];
    Ok(DatabaseManifest::recovery_required(
        DatabaseCompositionIdentity::new(
            database_id,
            DatabaseLifecycleGeneration::new(1)
                .ok_or_else(|| io::Error::other("test generation is zero"))?,
            persistent_log_id,
            &files,
        )?,
        DatabaseStorageFormatRequirements::new(
            format_version(formats[0])?,
            format_version(formats[1])?,
            format_version(formats[2])?,
        ),
        DatabaseRequiredFeatures::NONE,
    ))
}

fn database_id(value: u128) -> Result<DatabaseId, io::Error> {
    DatabaseId::new(value).ok_or_else(|| io::Error::other("test database ID is zero"))
}

fn database_file_id(value: u128) -> Result<DatabaseFileId, io::Error> {
    DatabaseFileId::new(value).ok_or_else(|| io::Error::other("test file ID is zero"))
}

fn object_id(value: u128) -> Result<InMemoryDatabaseObjectId, io::Error> {
    InMemoryDatabaseObjectId::new(value).ok_or_else(|| io::Error::other("test object ID is zero"))
}

fn persistent_log_id(value: u128) -> Result<PersistentLogId, io::Error> {
    PersistentLogId::new(value).ok_or_else(|| io::Error::other("test persistent log ID is zero"))
}

fn format_version(value: u16) -> Result<DatabaseStorageFormatVersion, io::Error> {
    DatabaseStorageFormatVersion::new(value).ok_or_else(|| io::Error::other("test format is zero"))
}

fn compatibility_context(target_id: &str) -> Result<CompatibilityContext, Box<dyn Error>> {
    Ok(CompatibilityContext::try_new(CompatibilityProfile {
        target_id: target_id.to_owned(),
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
    })?)
}
