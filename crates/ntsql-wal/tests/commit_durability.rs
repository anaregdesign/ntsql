use std::{cell::Cell, error::Error, fmt};

use ntsql_wal::{CommitError, CommitLog, LogLineage, LogSequenceNumber, commit_durability};

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
    calls: Vec<Call>,
}

impl FakeCommitLog {
    fn succeeds_at(position: u64) -> Self {
        Self {
            lineage: LogLineage::new(),
            position: LogSequenceNumber::new(position),
            append_fails: false,
            flush_fails: false,
            calls: Vec::new(),
        }
    }
}

impl CommitLog<str> for FakeCommitLog {
    type Error = FakeError;

    fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    fn append_commit(&mut self, record: &str) -> Result<LogSequenceNumber, Self::Error> {
        self.calls.push(Call::Append(record.to_owned()));
        if self.append_fails {
            Err(FakeError::Append)
        } else {
            Ok(self.position)
        }
    }

    fn flush_through(&mut self, position: LogSequenceNumber) -> Result<(), Self::Error> {
        self.calls.push(Call::Flush(position));
        if self.flush_fails {
            Err(FakeError::Flush)
        } else {
            Ok(())
        }
    }
}

#[test]
fn acknowledgement_follows_append_and_exact_flush() -> Result<(), CommitError<FakeError>> {
    let mut log = FakeCommitLog::succeeds_at(42);

    let acknowledged_position = commit_durability(&mut log, "commit-record", |acknowledgement| {
        acknowledgement.position()
    })?;

    assert_eq!(acknowledged_position, LogSequenceNumber::new(42));
    assert_eq!(
        log.calls,
        [
            Call::Append("commit-record".to_owned()),
            Call::Flush(LogSequenceNumber::new(42)),
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
        acknowledgement.position()
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
fn flush_failure_preserves_unacknowledged_position_and_cause() {
    let mut log = FakeCommitLog {
        flush_fails: true,
        ..FakeCommitLog::succeeds_at(73)
    };
    let callback_called = Cell::new(false);

    let result = commit_durability(&mut log, "commit-record", |acknowledgement| {
        callback_called.set(true);
        acknowledgement.position()
    });

    assert_eq!(
        result,
        Err(CommitError::Flush {
            position: LogSequenceNumber::new(73),
            source: FakeError::Flush,
        })
    );
    assert_eq!(
        log.calls,
        [
            Call::Append("commit-record".to_owned()),
            Call::Flush(LogSequenceNumber::new(73)),
        ]
    );
    assert!(!callback_called.get());
}
