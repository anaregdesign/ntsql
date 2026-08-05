use std::{error::Error, fmt, io};

use ntsql_transaction::{ActiveTransaction, TransactionCommitRecord, TransactionId};
use ntsql_wal::{CommitError, CommitLog, LogSequenceNumber};

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
    Append(TransactionId),
    Flush(LogSequenceNumber),
}

struct FakeCommitLog {
    position: LogSequenceNumber,
    append_fails: bool,
    flush_fails: bool,
    calls: Vec<Call>,
}

impl FakeCommitLog {
    fn succeeds_at(position: u64) -> Self {
        Self {
            position: LogSequenceNumber::new(position),
            append_fails: false,
            flush_fails: false,
            calls: Vec::new(),
        }
    }
}

impl CommitLog<TransactionCommitRecord> for FakeCommitLog {
    type Error = FakeError;

    fn append_commit(
        &mut self,
        record: &TransactionCommitRecord,
    ) -> Result<LogSequenceNumber, Self::Error> {
        self.calls.push(Call::Append(record.transaction_id()));
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
fn durable_commit_consumes_active_state_and_preserves_identity() -> Result<(), Box<dyn Error>> {
    let mut log = FakeCommitLog::succeeds_at(41);
    let transaction_id = TransactionId::new(7);
    let transaction = ActiveTransaction::new(transaction_id);

    let committed = transaction.commit(&mut log)?;

    assert_eq!(committed.transaction_id(), transaction_id);
    assert_eq!(committed.log_position(), LogSequenceNumber::new(41));
    assert_eq!(
        log.calls,
        [
            Call::Append(transaction_id),
            Call::Flush(LogSequenceNumber::new(41)),
        ]
    );
    Ok(())
}

#[test]
fn append_failure_consumes_active_state_into_indeterminate() -> Result<(), Box<dyn Error>> {
    let mut log = FakeCommitLog {
        append_fails: true,
        ..FakeCommitLog::succeeds_at(41)
    };
    let transaction_id = TransactionId::new(7);

    let error = ActiveTransaction::new(transaction_id)
        .commit(&mut log)
        .err()
        .ok_or_else(|| invalid_data("append failure unexpectedly committed"))?;
    let (indeterminate, cause) = error.into_parts();

    assert_eq!(indeterminate.transaction_id(), transaction_id);
    assert_eq!(
        cause,
        CommitError::Append {
            source: FakeError::Append,
        }
    );
    assert_eq!(log.calls, [Call::Append(transaction_id)]);
    Ok(())
}

#[test]
fn flush_failure_consumes_active_state_into_indeterminate() -> Result<(), Box<dyn Error>> {
    let mut log = FakeCommitLog {
        flush_fails: true,
        ..FakeCommitLog::succeeds_at(83)
    };
    let transaction_id = TransactionId::new(11);

    let error = ActiveTransaction::new(transaction_id)
        .commit(&mut log)
        .err()
        .ok_or_else(|| invalid_data("flush failure unexpectedly committed"))?;
    let (indeterminate, cause) = error.into_parts();

    assert_eq!(indeterminate.transaction_id(), transaction_id);
    assert_eq!(
        cause,
        CommitError::Flush {
            position: LogSequenceNumber::new(83),
            source: FakeError::Flush,
        }
    );
    assert_eq!(
        log.calls,
        [
            Call::Append(transaction_id),
            Call::Flush(LogSequenceNumber::new(83)),
        ]
    );
    Ok(())
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
