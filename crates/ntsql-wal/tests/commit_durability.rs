use std::{cell::Cell, error::Error, fmt};

use ntsql_wal::{
    CommitError, CommitLog, LogDurability, LogLineage, LogSequenceNumber, PersistentLogId,
    commit_durability,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FakeError {
    Append,
    Flush,
}

impl fmt::Display for FakeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Append => formatter.write_str("append failure"),
            Self::Flush => formatter.write_str("flush failure"),
        }
    }
}

impl Error for FakeError {}

#[derive(Debug, Eq, PartialEq)]
enum Call {
    Append(String),
    Flush(LogSequenceNumber),
}

struct FakeCommitLog {
    lineage: LogLineage,
    position: LogSequenceNumber,
    append_fails: bool,
    flush_fails: bool,
    lineage_after_append: Option<LogLineage>,
    calls: Vec<Call>,
}

impl FakeCommitLog {
    fn succeeds_at(position: u64) -> Self {
        let lineage = LogLineage::new();
        Self {
            position: lineage.position(position),
            lineage,
            append_fails: false,
            flush_fails: false,
            lineage_after_append: None,
            calls: Vec::new(),
        }
    }
}

impl LogDurability for FakeCommitLog {
    type Error = FakeError;

    fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    fn flush_through(&mut self, position: &LogSequenceNumber) -> Result<(), Self::Error> {
        self.calls.push(Call::Flush(position.clone()));
        if self.flush_fails {
            Err(FakeError::Flush)
        } else {
            Ok(())
        }
    }
}

impl CommitLog<str> for FakeCommitLog {
    fn append_commit(&mut self, record: &str) -> Result<LogSequenceNumber, Self::Error> {
        self.calls.push(Call::Append(record.to_owned()));
        if let Some(lineage) = self.lineage_after_append.take() {
            self.lineage = lineage;
        }
        if self.append_fails {
            Err(FakeError::Append)
        } else {
            Ok(self.position.clone())
        }
    }
}

#[test]
fn acknowledgement_follows_append_and_exact_flush() -> Result<(), CommitError<FakeError>> {
    let mut log = FakeCommitLog::succeeds_at(42);
    let expected_position = log.position.clone();

    let acknowledged_position = commit_durability(&mut log, "commit-record", |acknowledgement| {
        acknowledgement.position().clone()
    })?;

    assert_eq!(acknowledged_position, expected_position);
    assert_eq!(
        log.calls,
        [
            Call::Append("commit-record".to_owned()),
            Call::Flush(expected_position),
        ]
    );
    Ok(())
}

#[test]
fn append_failure_prevents_flush_and_acknowledgement() {
    let mut log = FakeCommitLog {
        append_fails: true,
        ..FakeCommitLog::succeeds_at(42)
    };
    let callback_called = Cell::new(false);

    let result = commit_durability(&mut log, "commit-record", |acknowledgement| {
        callback_called.set(true);
        acknowledgement.position().clone()
    });

    assert_eq!(
        result,
        Err(CommitError::Append {
            source: FakeError::Append,
        })
    );
    assert_eq!(log.calls, [Call::Append("commit-record".to_owned())]);
    assert!(!callback_called.get());
}

#[test]
fn foreign_append_position_prevents_flush_and_acknowledgement() {
    let mut log = FakeCommitLog::succeeds_at(42);
    let foreign_position = LogLineage::new().position(42);
    assert_eq!(log.position.get(), foreign_position.get());
    assert_ne!(log.position, foreign_position);
    log.position = foreign_position.clone();
    let callback_called = Cell::new(false);

    let result = commit_durability(&mut log, "commit-record", |_| {
        callback_called.set(true);
    });

    assert_eq!(
        result,
        Err(CommitError::ForeignAppendPosition {
            position: foreign_position
        })
    );
    assert_eq!(log.calls, [Call::Append("commit-record".to_owned())]);
    assert!(!callback_called.get());
}

#[test]
fn lineage_rotation_during_append_prevents_flush_and_acknowledgement() {
    let mut log = FakeCommitLog::succeeds_at(42);
    let original_position = log.position.clone();
    log.lineage_after_append = Some(LogLineage::new());
    let callback_called = Cell::new(false);

    let result = commit_durability(&mut log, "commit-record", |_| {
        callback_called.set(true);
    });

    assert_eq!(
        result,
        Err(CommitError::ForeignAppendPosition {
            position: original_position
        })
    );
    assert_eq!(log.calls, [Call::Append("commit-record".to_owned())]);
    assert!(!callback_called.get());
}

#[test]
fn flush_failure_preserves_unacknowledged_position_and_cause() {
    let mut log = FakeCommitLog {
        flush_fails: true,
        ..FakeCommitLog::succeeds_at(73)
    };
    let expected_position = log.position.clone();
    let callback_called = Cell::new(false);

    let result = commit_durability(&mut log, "commit-record", |acknowledgement| {
        callback_called.set(true);
        acknowledgement.position().clone()
    });

    assert_eq!(
        result,
        Err(CommitError::Flush {
            position: expected_position.clone(),
            source: FakeError::Flush,
        })
    );
    assert_eq!(
        log.calls,
        [
            Call::Append("commit-record".to_owned()),
            Call::Flush(expected_position),
        ]
    );
    assert!(!callback_called.get());
}

#[test]
fn persistent_id_reconstructs_lineage_and_position_identity() -> Result<(), Box<dyn Error>> {
    assert_eq!(PersistentLogId::new(0), None);
    let id = PersistentLogId::new(7)
        .ok_or_else(|| std::io::Error::other("nonzero persistent ID was rejected"))?;
    let other_id = PersistentLogId::new(8)
        .ok_or_else(|| std::io::Error::other("nonzero persistent ID was rejected"))?;
    let first = LogLineage::persistent(id);
    let reopened = LogLineage::persistent(id);
    let other = LogLineage::persistent(other_id);
    let ephemeral = LogLineage::new();

    assert_eq!(first.persistent_id(), Some(id));
    assert!(first.same_lineage(&reopened));
    assert!(!first.same_lineage(&other));
    assert!(!first.same_lineage(&ephemeral));
    assert_eq!(first.position(41), reopened.position(41));
    assert_ne!(first.position(41), other.position(41));
    Ok(())
}
