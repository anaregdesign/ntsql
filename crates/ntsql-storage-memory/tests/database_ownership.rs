use std::{error::Error, io};

use ntsql_database::{
    DatabaseCompositionIdentity, DatabaseFileId, DatabaseFileIdentity, DatabaseFileRole,
    DatabaseId, DatabaseLifecycleGeneration, DatabaseLifecycleStage, DatabaseManifest,
    DatabaseRequiredFeatures, DatabaseStorageFormatRequirements, DatabaseStorageFormatVersion,
};
use ntsql_storage_memory::{
    InMemoryDatabaseFileObservation, InMemoryDatabaseObjectId, InMemoryDatabaseObjectRole,
    InMemoryDatabaseOwnershipError, InMemoryDatabaseOwnershipSlot,
    InMemoryDatabaseOwnershipSlotError, InMemoryDatabaseOwnershipWorld,
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
        let database_id = database_id(database_value)?;
        let persistent_log_id = persistent_log_id(persistent_value)?;
        let file_values = [
            1_000 + database_value,
            2_000 + database_value,
            3_000 + database_value,
        ];
        let formats = [3, 1, 1];
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
