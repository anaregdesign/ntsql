//! I/O-free write-ahead durability invariants.

use std::{error::Error, fmt, marker::PhantomData};

/// Opaque ntsql-internal position assigned by a commit-log adapter.
///
/// This value defines no SQL Server, wire, or persistent byte representation.
/// It is meaningful only for the log that assigned it; values from independent
/// logs are not globally ordered or interchangeable.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogSequenceNumber(u64);

impl LogSequenceNumber {
    /// Preserves a position assigned by the commit-log implementation.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the opaque numeric position for adapter bookkeeping.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Persistence port needed to establish one commit durability fence.
///
/// The record type is owned by the caller. A concrete outer adapter decides how
/// to encode and persist it; this domain crate owns only call ordering.
pub trait CommitLog<Record: ?Sized> {
    /// Adapter-specific failure retained by [`CommitError`].
    type Error;

    /// Appends one commit record and returns its exact assigned position.
    fn append_commit(&mut self, record: &Record) -> Result<LogSequenceNumber, Self::Error>;

    /// Makes the log durable through at least `position`.
    ///
    /// `Ok(())` must mean durable completion, not queued or scheduled work.
    fn flush_through(&mut self, position: LogSequenceNumber) -> Result<(), Self::Error>;
}

/// Internal failure to establish commit durability.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommitError<E> {
    /// Appending the commit record did not report success.
    ///
    /// The adapter's physical state is unspecified; callers must not assume a
    /// retry is safe.
    Append {
        /// Unmodified adapter failure.
        source: E,
    },
    /// The commit record was appended but not confirmed durable.
    Flush {
        /// Appended position that remains unacknowledged.
        position: LogSequenceNumber,
        /// Unmodified adapter failure.
        source: E,
    },
}

impl<E: fmt::Display> fmt::Display for CommitError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Append { source } => write!(formatter, "commit log append failed: {source}"),
            Self::Flush { position, source } => write!(
                formatter,
                "commit log flush through {} failed: {source}",
                position.get()
            ),
        }
    }
}

impl<E> Error for CommitError<E>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Append { source } | Self::Flush { source, .. } => Some(source),
        }
    }
}

type CommitAttemptBrand<'attempt> = (&'attempt (), fn(&'attempt ()) -> &'attempt ());

/// Proof that the commit-log port confirmed durability through one position.
///
/// Fields and construction are private, and the generative attempt brand cannot
/// escape [`commit_durability`]. Safe downstream code therefore cannot forge,
/// retain, clone, or substitute an acknowledgement across sequential attempts.
///
/// ```compile_fail
/// use ntsql_wal::{CommitAcknowledgement, LogSequenceNumber};
///
/// let _forged = CommitAcknowledgement {
///     position: LogSequenceNumber::new(1),
/// };
/// ```
///
/// ```compile_fail
/// use ntsql_wal::CommitAcknowledgement;
///
/// fn cannot_clone(acknowledgement: CommitAcknowledgement<'_>) {
///     let _copy = acknowledgement.clone();
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
#[must_use]
pub struct CommitAcknowledgement<'attempt> {
    position: LogSequenceNumber,
    attempt_brand: PhantomData<CommitAttemptBrand<'attempt>>,
}

impl CommitAcknowledgement<'_> {
    /// Returns the exact position confirmed by the durability fence.
    #[must_use]
    pub const fn position(&self) -> LogSequenceNumber {
        self.position
    }
}

/// Appends one commit record and flushes through its exact assigned position.
///
/// The callback receives a fresh, non-escaping acknowledgement only after both
/// port operations report success. No acknowledgement exists on either failure
/// path. A successful result means only that the injected port reported the
/// requested durability; concrete persistence correctness remains the adapter's
/// responsibility.
///
/// ```compile_fail
/// use ntsql_wal::{CommitLog, commit_durability};
///
/// fn cannot_escape<L, Record: ?Sized>(log: &mut L, record: &Record)
/// where
///     L: CommitLog<Record>,
/// {
///     let _escaped = commit_durability(log, record, |acknowledgement| acknowledgement);
/// }
/// ```
pub fn commit_durability<L, Record, Output, OnDurable>(
    log: &mut L,
    record: &Record,
    on_durable: OnDurable,
) -> Result<Output, CommitError<L::Error>>
where
    L: CommitLog<Record>,
    Record: ?Sized,
    OnDurable: for<'attempt> FnOnce(CommitAcknowledgement<'attempt>) -> Output,
{
    let position = log
        .append_commit(record)
        .map_err(|source| CommitError::Append { source })?;
    log.flush_through(position)
        .map_err(|source| CommitError::Flush { position, source })?;
    Ok(on_durable(CommitAcknowledgement {
        position,
        attempt_brand: PhantomData,
    }))
}
