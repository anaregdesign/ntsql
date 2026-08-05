//! I/O-free page durability invariants.
//!
//! Every type in this crate is ntsql-internal. The crate defines no SQL Server
//! page identity, page format, recovery semantics, checkpoint contract, or byte
//! representation claim.

use std::{error::Error, fmt, marker::PhantomData, num::NonZeroU64};

use ntsql_wal::{LogDurability, LogLineage, LogSequenceNumber};

/// Opaque ntsql-internal nonzero page number.
///
/// This value is adapter bookkeeping only. It defines no SQL Server page ID,
/// file offset, or persistent byte representation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PageNumber(NonZeroU64);

impl PageNumber {
    /// Wraps one nonzero adapter-owned page number.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the numeric page number for adapter bookkeeping.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// Opaque ntsql-internal page address scoped to one log lineage.
///
/// This is not SQL Server page identity. The lineage is cloned from the caller
/// so equality remains bound to one persistent ntsql lineage.
#[derive(Debug)]
pub struct PageAddress {
    lineage: LogLineage,
    number: PageNumber,
}

impl PageAddress {
    /// Creates one page address owned by the supplied lineage.
    #[must_use]
    pub fn new(lineage: &LogLineage, number: PageNumber) -> Self {
        Self {
            lineage: lineage.clone(),
            number,
        }
    }

    /// Returns the lineage that owns this internal address.
    #[must_use]
    pub const fn lineage(&self) -> &LogLineage {
        &self.lineage
    }

    /// Returns the adapter-owned page number.
    #[must_use]
    pub const fn number(&self) -> PageNumber {
        self.number
    }
}

impl PartialEq for PageAddress {
    fn eq(&self, other: &Self) -> bool {
        self.number == other.number && self.lineage.same_lineage(&other.lineage)
    }
}

impl Eq for PageAddress {}

/// Opaque ntsql-internal version assigned by one outer page adapter.
///
/// This value defines no SQL Server rowversion, page header, or wire contract.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct PageVersion(u64);

impl PageVersion {
    /// Wraps one adapter-assigned version.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric version for adapter bookkeeping.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Failure to build an ntsql-internal page image.
#[derive(Debug, Eq, PartialEq)]
pub enum PageImageError<const N: usize> {
    /// The page image width was zero, so no fixed-size page exists.
    ZeroLength {
        /// Original caller bytes retained without fallback allocation.
        bytes: [u8; N],
    },
}

impl<const N: usize> PageImageError<N> {
    /// Returns the retained caller bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; N] {
        match self {
            Self::ZeroLength { bytes } => bytes,
        }
    }

    /// Returns the retained caller bytes.
    #[must_use]
    pub fn into_bytes(self) -> [u8; N] {
        match self {
            Self::ZeroLength { bytes } => bytes,
        }
    }
}

impl<const N: usize> fmt::Display for PageImageError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroLength { .. } => formatter.write_str("page image length must be nonzero"),
        }
    }
}

impl<const N: usize> Error for PageImageError<N> {}

/// Fixed-size ntsql-internal page bytes.
///
/// This value is raw adapter-owned content only. It defines no SQL Server page
/// layout, checksum, header, or persistence encoding.
#[derive(Debug, Eq, PartialEq)]
pub struct PageImage<const N: usize> {
    bytes: [u8; N],
}

impl<const N: usize> PageImage<N> {
    /// Creates one fixed-size page image.
    pub fn new(bytes: [u8; N]) -> Result<Self, PageImageError<N>> {
        if N == 0 {
            return Err(PageImageError::ZeroLength { bytes });
        }
        Ok(Self { bytes })
    }

    /// Returns the borrowed image bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; N] {
        &self.bytes
    }

    /// Returns the owned image bytes.
    #[must_use]
    pub fn into_bytes(self) -> [u8; N] {
        self.bytes
    }
}

/// Ntsql-internal page image that has not yet received a WAL position.
#[derive(Debug, Eq, PartialEq)]
pub struct UnloggedPage<const N: usize> {
    address: PageAddress,
    version: PageVersion,
    image: PageImage<N>,
}

impl<const N: usize> UnloggedPage<N> {
    /// Creates one page write that still requires WAL append.
    #[must_use]
    pub const fn new(address: PageAddress, version: PageVersion, image: PageImage<N>) -> Self {
        Self {
            address,
            version,
            image,
        }
    }

    /// Returns the internal page address.
    #[must_use]
    pub const fn address(&self) -> &PageAddress {
        &self.address
    }

    /// Returns the adapter-assigned page version.
    #[must_use]
    pub const fn version(&self) -> PageVersion {
        self.version
    }

    /// Returns the unlogged image.
    #[must_use]
    pub const fn image(&self) -> &PageImage<N> {
        &self.image
    }

    /// Returns every owned input.
    #[must_use]
    pub fn into_parts(self) -> (PageAddress, PageVersion, PageImage<N>) {
        (self.address, self.version, self.image)
    }
}

/// WAL append port for one complete ntsql-internal page image.
pub trait PageLog<const N: usize>: LogDurability {
    /// Appends one page image and returns its exact assigned WAL position.
    ///
    /// Success means the record was appended, not made durable. The returned
    /// position must identify that record in this log's lineage. An error does
    /// not specify whether the physical append occurred.
    fn append_page(&mut self, page: &UnloggedPage<N>) -> Result<LogSequenceNumber, Self::Error>;
}

/// Terminal page state after a WAL append was attempted but valid append
/// evidence was not established.
///
/// The append may have changed physical state. This type deliberately offers no
/// conversion back to unlogged, dirty, clean, or directly retryable state.
///
/// ```compile_fail
/// use ntsql_page::IndeterminatePageLogAppend;
///
/// fn cannot_retry<const N: usize>(page: IndeterminatePageLogAppend<N>) {
///     let _ = page.into_unlogged_page();
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct IndeterminatePageLogAppend<const N: usize> {
    address: PageAddress,
    version: PageVersion,
    image: PageImage<N>,
    observed_position: Option<LogSequenceNumber>,
}

impl<const N: usize> IndeterminatePageLogAppend<N> {
    fn from_unlogged(page: UnloggedPage<N>, observed_position: Option<LogSequenceNumber>) -> Self {
        let UnloggedPage {
            address,
            version,
            image,
        } = page;
        Self {
            address,
            version,
            image,
            observed_position,
        }
    }

    /// Returns the internal page address.
    #[must_use]
    pub const fn address(&self) -> &PageAddress {
        &self.address
    }

    /// Returns the adapter-assigned version.
    #[must_use]
    pub const fn version(&self) -> PageVersion {
        self.version
    }

    /// Returns the page image whose append outcome is unresolved.
    #[must_use]
    pub const fn image(&self) -> &PageImage<N> {
        &self.image
    }

    /// Returns an adapter-reported position when append returned success but
    /// its evidence was invalid.
    #[must_use]
    pub const fn observed_position(&self) -> Option<&LogSequenceNumber> {
        self.observed_position.as_ref()
    }
}

/// Pre-append rejection reason for an unlogged page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StagePageWriteRejectionReason {
    /// The page and supplied WAL belong to different lineages.
    ForeignLog,
}

/// Page staging rejected before the WAL append port was called.
#[derive(Debug, Eq, PartialEq)]
pub struct StagePageWriteRejection<const N: usize> {
    page: UnloggedPage<N>,
    reason: StagePageWriteRejectionReason,
}

impl<const N: usize> StagePageWriteRejection<N> {
    /// Returns the unchanged unlogged page.
    #[must_use]
    pub const fn page(&self) -> &UnloggedPage<N> {
        &self.page
    }

    /// Returns the exact rejection reason.
    #[must_use]
    pub const fn reason(&self) -> StagePageWriteRejectionReason {
        self.reason
    }

    /// Returns the unchanged unlogged page for a corrected composition.
    #[must_use]
    pub fn into_page(self) -> UnloggedPage<N> {
        self.page
    }
}

impl<const N: usize> fmt::Display for StagePageWriteRejection<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            StagePageWriteRejectionReason::ForeignLog => write!(
                formatter,
                "page {} belongs to another WAL lineage",
                self.page.address().number().get()
            ),
        }
    }
}

impl<const N: usize> Error for StagePageWriteRejection<N> {}

/// WAL append failure paired with terminal page state.
#[derive(Debug, Eq, PartialEq)]
pub struct StagePageWriteAppendError<E, const N: usize> {
    page: IndeterminatePageLogAppend<N>,
    source: E,
}

impl<E, const N: usize> StagePageWriteAppendError<E, N> {
    /// Returns the terminal page state.
    #[must_use]
    pub const fn page(&self) -> &IndeterminatePageLogAppend<N> {
        &self.page
    }

    /// Returns the exact WAL append failure.
    #[must_use]
    pub const fn cause(&self) -> &E {
        &self.source
    }

    /// Returns the terminal state and exact WAL cause.
    #[must_use]
    pub fn into_parts(self) -> (IndeterminatePageLogAppend<N>, E) {
        (self.page, self.source)
    }
}

impl<E: fmt::Display, const N: usize> fmt::Display for StagePageWriteAppendError<E, N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "page {} WAL append failed: {}",
            self.page.address().number().get(),
            self.source
        )
    }
}

impl<E, const N: usize> Error for StagePageWriteAppendError<E, N>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Invalid evidence returned after a page WAL append reported success.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StagePageWriteEvidenceErrorReason {
    /// The returned position belongs to another lineage.
    ForeignPosition,
    /// The WAL lineage changed while append was in progress.
    LogLineageChanged,
}

/// Invalid post-append evidence paired with terminal page state.
#[derive(Debug, Eq, PartialEq)]
pub struct StagePageWriteEvidenceError<const N: usize> {
    page: IndeterminatePageLogAppend<N>,
    reason: StagePageWriteEvidenceErrorReason,
}

impl<const N: usize> StagePageWriteEvidenceError<N> {
    /// Returns the terminal page state.
    #[must_use]
    pub const fn page(&self) -> &IndeterminatePageLogAppend<N> {
        &self.page
    }

    /// Returns the exact evidence failure.
    #[must_use]
    pub const fn reason(&self) -> StagePageWriteEvidenceErrorReason {
        self.reason
    }
}

impl<const N: usize> fmt::Display for StagePageWriteEvidenceError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Some(position) = self.page.observed_position() else {
            return write!(
                formatter,
                "page {} WAL append evidence has no observed position",
                self.page.address().number().get()
            );
        };
        match self.reason {
            StagePageWriteEvidenceErrorReason::ForeignPosition => write!(
                formatter,
                "page {} WAL append returned foreign position {}",
                self.page.address().number().get(),
                position.get()
            ),
            StagePageWriteEvidenceErrorReason::LogLineageChanged => write!(
                formatter,
                "page {} WAL lineage changed after append at position {}",
                self.page.address().number().get(),
                position.get()
            ),
        }
    }
}

impl<const N: usize> Error for StagePageWriteEvidenceError<N> {}

/// Failure before or after the page-WAL append effect boundary.
#[derive(Debug, Eq, PartialEq)]
pub enum StagePageWriteError<E, const N: usize> {
    /// Composition was rejected before append.
    Rejected(StagePageWriteRejection<N>),
    /// Append returned an adapter failure after it was invoked.
    Append(StagePageWriteAppendError<E, N>),
    /// Append returned success with invalid lineage evidence.
    InvalidEvidence(StagePageWriteEvidenceError<N>),
}

impl<E: fmt::Display, const N: usize> fmt::Display for StagePageWriteError<E, N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(error) => error.fmt(formatter),
            Self::Append(error) => error.fmt(formatter),
            Self::InvalidEvidence(error) => error.fmt(formatter),
        }
    }
}

impl<E, const N: usize> Error for StagePageWriteError<E, N>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected(error) => Some(error),
            Self::Append(error) => Some(error),
            Self::InvalidEvidence(error) => Some(error),
        }
    }
}

/// Reason dirty-page construction was rejected before any page state existed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirtyPageConstructionErrorReason {
    /// The required WAL position belongs to another lineage than the page
    /// address.
    ForeignRequiredPosition,
}

/// Failed ntsql-internal dirty-page construction that retains every moved part.
#[derive(Debug, Eq, PartialEq)]
pub struct DirtyPageConstructionError<const N: usize> {
    reason: DirtyPageConstructionErrorReason,
    address: PageAddress,
    version: PageVersion,
    image: PageImage<N>,
    required_position: LogSequenceNumber,
}

impl<const N: usize> DirtyPageConstructionError<N> {
    /// Returns the exact rejection reason.
    #[must_use]
    pub const fn reason(&self) -> DirtyPageConstructionErrorReason {
        self.reason
    }

    /// Returns the retained construction inputs unchanged.
    #[must_use]
    pub fn into_parts(self) -> (PageAddress, PageVersion, PageImage<N>, LogSequenceNumber) {
        (
            self.address,
            self.version,
            self.image,
            self.required_position,
        )
    }
}

impl<const N: usize> fmt::Display for DirtyPageConstructionError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            DirtyPageConstructionErrorReason::ForeignRequiredPosition => write!(
                formatter,
                "page {} and required WAL position {} belong to different lineages",
                self.address.number().get(),
                self.required_position.get()
            ),
        }
    }
}

impl<const N: usize> Error for DirtyPageConstructionError<N> {}

/// Ntsql-internal page state that still requires one exact WAL durability fence
/// before the page store may report success.
///
/// ```compile_fail
/// use ntsql_page::{DirtyPage, PageAddress, PageImage, PageVersion};
/// use ntsql_wal::LogSequenceNumber;
///
/// fn cannot_construct<const N: usize>(
///     address: PageAddress,
///     version: PageVersion,
///     image: PageImage<N>,
///     position: LogSequenceNumber,
/// ) {
///     let _ = DirtyPage::new(address, version, image, position);
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct DirtyPage<const N: usize> {
    address: PageAddress,
    version: PageVersion,
    image: PageImage<N>,
    required_position: LogSequenceNumber,
}

impl<const N: usize> DirtyPage<N> {
    fn new(
        address: PageAddress,
        version: PageVersion,
        image: PageImage<N>,
        required_position: LogSequenceNumber,
    ) -> Result<Self, DirtyPageConstructionError<N>> {
        if !address.lineage().same_lineage(required_position.lineage()) {
            return Err(DirtyPageConstructionError {
                reason: DirtyPageConstructionErrorReason::ForeignRequiredPosition,
                address,
                version,
                image,
                required_position,
            });
        }
        Ok(Self {
            address,
            version,
            image,
            required_position,
        })
    }

    /// Returns the internal page address.
    #[must_use]
    pub const fn address(&self) -> &PageAddress {
        &self.address
    }

    /// Returns the adapter-assigned version.
    #[must_use]
    pub const fn version(&self) -> PageVersion {
        self.version
    }

    /// Returns the borrowed image bytes.
    #[must_use]
    pub const fn image(&self) -> &PageImage<N> {
        &self.image
    }

    /// Returns the exact WAL position that must already be durable before store
    /// success is valid.
    #[must_use]
    pub const fn required_position(&self) -> &LogSequenceNumber {
        &self.required_position
    }
}

/// Appends one full page image and stages it as dirty only after validating the
/// exact returned position and unchanged WAL lineage.
///
/// A foreign WAL is rejected before append and returns the unchanged unlogged
/// page. Once append is invoked, any error or invalid evidence is terminal
/// because the physical append effect is unspecified.
pub fn stage_page_write<Log, const N: usize>(
    log: &mut Log,
    page: UnloggedPage<N>,
) -> Result<DirtyPage<N>, StagePageWriteError<Log::Error, N>>
where
    Log: PageLog<N>,
{
    if !page.address().lineage().same_lineage(log.lineage()) {
        return Err(StagePageWriteError::Rejected(StagePageWriteRejection {
            page,
            reason: StagePageWriteRejectionReason::ForeignLog,
        }));
    }
    let expected_lineage = log.lineage().clone();
    let position = match log.append_page(&page) {
        Ok(position) => position,
        Err(source) => {
            return Err(StagePageWriteError::Append(StagePageWriteAppendError {
                page: IndeterminatePageLogAppend::from_unlogged(page, None),
                source,
            }));
        }
    };
    let reason = if !position.lineage().same_lineage(&expected_lineage) {
        Some(StagePageWriteEvidenceErrorReason::ForeignPosition)
    } else if !expected_lineage.same_lineage(log.lineage()) {
        Some(StagePageWriteEvidenceErrorReason::LogLineageChanged)
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(StagePageWriteError::InvalidEvidence(
            StagePageWriteEvidenceError {
                page: IndeterminatePageLogAppend::from_unlogged(page, Some(position)),
                reason,
            },
        ));
    }
    let (address, version, image) = page.into_parts();
    match DirtyPage::new(address, version, image, position) {
        Ok(dirty) => Ok(dirty),
        Err(error) => {
            let (address, version, image, position) = error.into_parts();
            Err(StagePageWriteError::InvalidEvidence(
                StagePageWriteEvidenceError {
                    page: IndeterminatePageLogAppend::from_unlogged(
                        UnloggedPage::new(address, version, image),
                        Some(position),
                    ),
                    reason: StagePageWriteEvidenceErrorReason::ForeignPosition,
                },
            ))
        }
    }
}

/// Ntsql-internal page state whose required WAL position and durable page write
/// both reported success.
///
/// This type defines no checkpoint, flush ordering, SQL Server buffer-manager,
/// or on-disk page-format claim.
///
/// ```compile_fail
/// use ntsql_page::{CleanPage, PageAddress, PageImage, PageNumber, PageVersion};
/// use ntsql_wal::LogLineage;
///
/// fn cannot_construct() {
///     let lineage = LogLineage::new();
///     let number = match PageNumber::new(1) {
///         Some(number) => number,
///         None => return,
///     };
///     let image = match PageImage::new([0_u8; 1]) {
///         Ok(image) => image,
///         Err(_) => return,
///     };
///     let _forged = CleanPage {
///         address: PageAddress::new(&lineage, number),
///         version: PageVersion::new(0),
///         image,
///         required_position: lineage.position(1),
///     };
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct CleanPage<const N: usize> {
    address: PageAddress,
    version: PageVersion,
    image: PageImage<N>,
    required_position: LogSequenceNumber,
}

impl<const N: usize> CleanPage<N> {
    fn from_dirty(dirty: DirtyPage<N>) -> Self {
        let DirtyPage {
            address,
            version,
            image,
            required_position,
        } = dirty;
        Self {
            address,
            version,
            image,
            required_position,
        }
    }

    /// Returns the internal page address.
    #[must_use]
    pub const fn address(&self) -> &PageAddress {
        &self.address
    }

    /// Returns the adapter-assigned version.
    #[must_use]
    pub const fn version(&self) -> PageVersion {
        self.version
    }

    /// Returns the borrowed clean image.
    #[must_use]
    pub const fn image(&self) -> &PageImage<N> {
        &self.image
    }

    /// Returns the exact WAL position that was established before durable page
    /// completion was reported.
    #[must_use]
    pub const fn required_position(&self) -> &LogSequenceNumber {
        &self.required_position
    }

    /// Returns the owned clean-page parts.
    #[must_use]
    pub fn into_parts(self) -> (PageAddress, PageVersion, PageImage<N>, LogSequenceNumber) {
        (
            self.address,
            self.version,
            self.image,
            self.required_position,
        )
    }
}

type PageWriteAttemptBrand<'attempt> = (&'attempt (), fn(&'attempt ()) -> &'attempt ());

/// Proof that one exact WAL position was already durable for the current page
/// write attempt.
///
/// The generative attempt brand prevents safe code from widening or stashing the
/// permit beyond the write attempt that created it.
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
/// use ntsql_wal::LogLineage;
///
/// fn cannot_forge() {
///     let lineage = LogLineage::new();
///     let _forged = PageWritePermit {
///         durable_position: lineage.position(1),
///     };
/// }
/// ```
///
/// ```compile_fail
/// use ntsql_page::PageWritePermit;
///
/// fn cannot_widen<'attempt>(permit: PageWritePermit<'attempt>) -> PageWritePermit<'static> {
///     permit
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
#[must_use]
pub struct PageWritePermit<'attempt> {
    durable_position: LogSequenceNumber,
    attempt_brand: PhantomData<PageWriteAttemptBrand<'attempt>>,
}

impl PageWritePermit<'_> {
    /// Returns the exact WAL position already confirmed durable for this write.
    #[must_use]
    pub const fn durable_position(&self) -> &LogSequenceNumber {
        &self.durable_position
    }
}

/// Persistence port that durably stores one page image for one lineage.
///
/// `Ok(())` must mean the durable page write completed, not that work was only
/// queued or scheduled.
///
/// ```compile_fail
/// use ntsql_page::{DirtyPage, PageStore};
///
/// fn cannot_call_without_permit<const N: usize, Store>(store: &mut Store, page: &DirtyPage<N>)
/// where
///     Store: PageStore<N>,
/// {
///     let _ = store.write_page(page);
/// }
/// ```
pub trait PageStore<const N: usize> {
    /// Adapter-specific write failure.
    type Error;

    /// Returns the lineage this store writes.
    fn lineage(&self) -> &LogLineage;

    /// Stores one page only after receiving the matching durable-write permit.
    fn write_page(
        &mut self,
        page: &DirtyPage<N>,
        permit: PageWritePermit<'_>,
    ) -> Result<(), Self::Error>;
}

/// Rejection reason returned before any log or store port was called.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FlushDirtyPageRejectionReason {
    /// The provided log belongs to another lineage.
    ForeignLog,
    /// The provided page store belongs to another lineage.
    ForeignStore,
}

/// Failed dirty-page flush rejected before touching either injected port.
#[derive(Debug, Eq, PartialEq)]
pub struct FlushDirtyPageRejection<const N: usize> {
    page: DirtyPage<N>,
    reason: FlushDirtyPageRejectionReason,
}

impl<const N: usize> FlushDirtyPageRejection<N> {
    /// Returns the retained dirty page.
    #[must_use]
    pub const fn page(&self) -> &DirtyPage<N> {
        &self.page
    }

    /// Returns the exact rejection reason.
    #[must_use]
    pub const fn reason(&self) -> FlushDirtyPageRejectionReason {
        self.reason
    }

    /// Returns the retained dirty page.
    #[must_use]
    pub fn into_page(self) -> DirtyPage<N> {
        self.page
    }

    /// Returns the retained dirty page and exact rejection reason.
    #[must_use]
    pub fn into_parts(self) -> (DirtyPage<N>, FlushDirtyPageRejectionReason) {
        (self.page, self.reason)
    }
}

impl<const N: usize> fmt::Display for FlushDirtyPageRejection<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            FlushDirtyPageRejectionReason::ForeignLog => write!(
                formatter,
                "page {} belongs to another log lineage",
                self.page.address().number().get()
            ),
            FlushDirtyPageRejectionReason::ForeignStore => write!(
                formatter,
                "page {} belongs to another page-store lineage",
                self.page.address().number().get()
            ),
        }
    }
}

impl<const N: usize> Error for FlushDirtyPageRejection<N> {}

/// WAL flush failure that retains the unchanged dirty page because the page
/// store was never called.
#[derive(Debug, Eq, PartialEq)]
pub struct FlushDirtyPageLogError<E, const N: usize> {
    page: DirtyPage<N>,
    source: E,
}

impl<E, const N: usize> FlushDirtyPageLogError<E, N> {
    /// Returns the retained dirty page.
    #[must_use]
    pub const fn page(&self) -> &DirtyPage<N> {
        &self.page
    }

    /// Returns the exact WAL failure.
    #[must_use]
    pub const fn cause(&self) -> &E {
        &self.source
    }

    /// Returns the dirty page for an idempotent retry and the exact WAL failure.
    #[must_use]
    pub fn into_parts(self) -> (DirtyPage<N>, E) {
        (self.page, self.source)
    }
}

impl<E: fmt::Display, const N: usize> fmt::Display for FlushDirtyPageLogError<E, N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "page {} WAL flush through {} failed: {}",
            self.page.address().number().get(),
            self.page.required_position().get(),
            self.source
        )
    }
}

impl<E, const N: usize> Error for FlushDirtyPageLogError<E, N>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Terminal ntsql-internal page state after WAL durability succeeded but the
/// page-store write failed.
///
/// This value deliberately offers no conversion back to dirty or clean page
/// state and no retry entrypoint.
///
/// ```compile_fail
/// use ntsql_page::IndeterminatePageWrite;
///
/// fn cannot_retry<const N: usize>(page: IndeterminatePageWrite<N>) {
///     let _ = page.into_dirty_page();
/// }
/// ```
#[derive(Debug, Eq, PartialEq)]
pub struct IndeterminatePageWrite<const N: usize> {
    address: PageAddress,
    version: PageVersion,
    image: PageImage<N>,
    required_position: LogSequenceNumber,
}

impl<const N: usize> IndeterminatePageWrite<N> {
    fn from_dirty(dirty: DirtyPage<N>) -> Self {
        let DirtyPage {
            address,
            version,
            image,
            required_position,
        } = dirty;
        Self {
            address,
            version,
            image,
            required_position,
        }
    }

    /// Returns the internal page address.
    #[must_use]
    pub const fn address(&self) -> &PageAddress {
        &self.address
    }

    /// Returns the adapter-assigned version.
    #[must_use]
    pub const fn version(&self) -> PageVersion {
        self.version
    }

    /// Returns the borrowed image.
    #[must_use]
    pub const fn image(&self) -> &PageImage<N> {
        &self.image
    }

    /// Returns the exact WAL position that had already been flushed before the
    /// store failure.
    #[must_use]
    pub const fn required_position(&self) -> &LogSequenceNumber {
        &self.required_position
    }
}

/// Store failure paired with terminal indeterminate page state.
#[derive(Debug, Eq, PartialEq)]
pub struct FlushDirtyPageStoreError<E, const N: usize> {
    page: IndeterminatePageWrite<N>,
    source: E,
}

impl<E, const N: usize> FlushDirtyPageStoreError<E, N> {
    /// Returns the terminal indeterminate page state.
    #[must_use]
    pub const fn page(&self) -> &IndeterminatePageWrite<N> {
        &self.page
    }

    /// Returns the exact store failure.
    #[must_use]
    pub const fn cause(&self) -> &E {
        &self.source
    }

    /// Returns the terminal page state and exact store failure.
    #[must_use]
    pub fn into_parts(self) -> (IndeterminatePageWrite<N>, E) {
        (self.page, self.source)
    }
}

impl<E: fmt::Display, const N: usize> fmt::Display for FlushDirtyPageStoreError<E, N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "page {} durable write after WAL position {} failed: {}",
            self.page.address().number().get(),
            self.page.required_position().get(),
            self.source
        )
    }
}

impl<E, const N: usize> Error for FlushDirtyPageStoreError<E, N>
where
    E: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Failed dirty-page flush before or after the store indeterminacy boundary.
#[derive(Debug, Eq, PartialEq)]
pub enum FlushDirtyPageError<LogError, StoreError, const N: usize> {
    /// The page, log, and store did not share one lineage, so no port was
    /// called.
    Rejected(FlushDirtyPageRejection<N>),
    /// The WAL flush failed before the page store was called.
    LogFlush(FlushDirtyPageLogError<LogError, N>),
    /// The WAL flush succeeded but the page-store result is terminally
    /// indeterminate.
    StoreWrite(FlushDirtyPageStoreError<StoreError, N>),
}

impl<LogError: fmt::Display, StoreError: fmt::Display, const N: usize> fmt::Display
    for FlushDirtyPageError<LogError, StoreError, N>
{
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected(error) => error.fmt(formatter),
            Self::LogFlush(error) => error.fmt(formatter),
            Self::StoreWrite(error) => error.fmt(formatter),
        }
    }
}

impl<LogError, StoreError, const N: usize> Error for FlushDirtyPageError<LogError, StoreError, N>
where
    LogError: Error + 'static,
    StoreError: Error + 'static,
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Rejected(error) => Some(error),
            Self::LogFlush(error) => Some(error),
            Self::StoreWrite(error) => Some(error),
        }
    }
}

/// Flushes the exact required WAL position before asking the page store to
/// report durable page completion.
///
/// Rejections for foreign log or store lineages happen before either port is
/// called. A WAL failure retains the dirty page for a safe retry because the
/// page store was not invoked. A store failure is terminally indeterminate.
pub fn flush_dirty_page<Log, Store, const N: usize>(
    log: &mut Log,
    store: &mut Store,
    dirty: DirtyPage<N>,
) -> Result<CleanPage<N>, FlushDirtyPageError<Log::Error, Store::Error, N>>
where
    Log: LogDurability,
    Store: PageStore<N>,
{
    if !dirty.address().lineage().same_lineage(log.lineage()) {
        return Err(FlushDirtyPageError::Rejected(FlushDirtyPageRejection {
            page: dirty,
            reason: FlushDirtyPageRejectionReason::ForeignLog,
        }));
    }
    if !dirty.address().lineage().same_lineage(store.lineage()) {
        return Err(FlushDirtyPageError::Rejected(FlushDirtyPageRejection {
            page: dirty,
            reason: FlushDirtyPageRejectionReason::ForeignStore,
        }));
    }
    if let Err(source) = log.flush_through(dirty.required_position()) {
        return Err(FlushDirtyPageError::LogFlush(FlushDirtyPageLogError {
            page: dirty,
            source,
        }));
    }

    let permit = PageWritePermit {
        durable_position: dirty.required_position().clone(),
        attempt_brand: PhantomData,
    };
    if let Err(source) = store.write_page(&dirty, permit) {
        return Err(FlushDirtyPageError::StoreWrite(FlushDirtyPageStoreError {
            page: IndeterminatePageWrite::from_dirty(dirty),
            source,
        }));
    }

    Ok(CleanPage::from_dirty(dirty))
}

/// Why an adapter-neutral page-recovery observation could not be constructed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PageRecoveryObservationErrorReason {
    /// Durable page WAL positions and stored required positions must be nonzero.
    ZeroPosition,
}

/// Failed construction of one adapter-neutral page-recovery observation.
#[derive(Debug, Eq, PartialEq)]
pub struct PageRecoveryObservationError<const N: usize> {
    page_number: PageNumber,
    page_version: PageVersion,
    image: PageImage<N>,
    position: LogSequenceNumber,
    reason: PageRecoveryObservationErrorReason,
}

impl<const N: usize> PageRecoveryObservationError<N> {
    /// Returns the retained page number.
    #[must_use]
    pub const fn page_number(&self) -> PageNumber {
        self.page_number
    }

    /// Returns the retained page version.
    #[must_use]
    pub const fn page_version(&self) -> PageVersion {
        self.page_version
    }

    /// Returns the retained page image.
    #[must_use]
    pub const fn image(&self) -> &PageImage<N> {
        &self.image
    }

    /// Returns the retained lineage-bound position.
    #[must_use]
    pub const fn position(&self) -> &LogSequenceNumber {
        &self.position
    }

    /// Returns the exact construction failure.
    #[must_use]
    pub const fn reason(&self) -> PageRecoveryObservationErrorReason {
        self.reason
    }

    /// Returns every retained input.
    #[must_use]
    pub fn into_parts(
        self,
    ) -> (
        PageNumber,
        PageVersion,
        PageImage<N>,
        LogSequenceNumber,
        PageRecoveryObservationErrorReason,
    ) {
        (
            self.page_number,
            self.page_version,
            self.image,
            self.position,
            self.reason,
        )
    }
}

impl<const N: usize> fmt::Display for PageRecoveryObservationError<N> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.reason {
            PageRecoveryObservationErrorReason::ZeroPosition => write!(
                formatter,
                "page {} recovery observation has zero as its WAL position",
                self.page_number.get()
            ),
        }
    }
}

impl<const N: usize> Error for PageRecoveryObservationError<N> {}

#[derive(Debug, Eq, PartialEq)]
struct PageRecoveryObservation<const N: usize> {
    page_number: PageNumber,
    page_version: PageVersion,
    image: PageImage<N>,
    position: LogSequenceNumber,
}

impl<const N: usize> PageRecoveryObservation<N> {
    fn new(
        page_number: PageNumber,
        page_version: PageVersion,
        image: PageImage<N>,
        position: LogSequenceNumber,
    ) -> Result<Self, PageRecoveryObservationError<N>> {
        if position.get() == 0 {
            return Err(PageRecoveryObservationError {
                page_number,
                page_version,
                image,
                position,
                reason: PageRecoveryObservationErrorReason::ZeroPosition,
            });
        }
        Ok(Self {
            page_number,
            page_version,
            image,
            position,
        })
    }
}

/// Adapter-neutral observation of one complete durable full-image page WAL
/// record.
///
/// Construction validates only the repository-owned value shape. Calling code
/// remains responsible for supplying records from an authoritative durable
/// prefix.
#[derive(Debug, Eq, PartialEq)]
pub struct DurablePageWalObservation<const N: usize> {
    observation: PageRecoveryObservation<N>,
}

impl<const N: usize> DurablePageWalObservation<N> {
    /// Constructs one observed durable full-image page WAL record.
    pub fn new(
        page_number: PageNumber,
        page_version: PageVersion,
        image: PageImage<N>,
        position: LogSequenceNumber,
    ) -> Result<Self, PageRecoveryObservationError<N>> {
        PageRecoveryObservation::new(page_number, page_version, image, position)
            .map(|observation| Self { observation })
    }

    /// Returns the observed page number.
    #[must_use]
    pub const fn page_number(&self) -> PageNumber {
        self.observation.page_number
    }

    /// Returns the observed page version.
    #[must_use]
    pub const fn page_version(&self) -> PageVersion {
        self.observation.page_version
    }

    /// Returns the observed full page image.
    #[must_use]
    pub const fn image(&self) -> &PageImage<N> {
        &self.observation.image
    }

    /// Returns the observed durable WAL position.
    #[must_use]
    pub const fn position(&self) -> &LogSequenceNumber {
        &self.observation.position
    }
}

/// Adapter-neutral observation of one current durable page-store snapshot.
///
/// The required position is evidence supplied by an adapter, not a replay
/// permit or proof that the matching WAL record exists.
#[derive(Debug, Eq, PartialEq)]
pub struct StoredPageSnapshotObservation<const N: usize> {
    observation: PageRecoveryObservation<N>,
}

impl<const N: usize> StoredPageSnapshotObservation<N> {
    /// Constructs one observed durable page-store snapshot.
    pub fn new(
        page_number: PageNumber,
        page_version: PageVersion,
        image: PageImage<N>,
        required_position: LogSequenceNumber,
    ) -> Result<Self, PageRecoveryObservationError<N>> {
        PageRecoveryObservation::new(page_number, page_version, image, required_position)
            .map(|observation| Self { observation })
    }

    /// Returns the observed page number.
    #[must_use]
    pub const fn page_number(&self) -> PageNumber {
        self.observation.page_number
    }

    /// Returns the observed page version.
    #[must_use]
    pub const fn page_version(&self) -> PageVersion {
        self.observation.page_version
    }

    /// Returns the observed full page image.
    #[must_use]
    pub const fn image(&self) -> &PageImage<N> {
        &self.observation.image
    }

    /// Returns the exact WAL position required by this stored snapshot.
    #[must_use]
    pub const fn required_position(&self) -> &LogSequenceNumber {
        &self.observation.position
    }
}

/// Allocation-free physical comparison of durable page WAL and page-store
/// observations.
///
/// This value deliberately cannot become a page-write permit:
///
/// ```compile_fail
/// use ntsql_page::{DurablePageReconciliation, PageWritePermit};
///
/// fn cannot_authorize_write(
///     comparison: DurablePageReconciliation,
/// ) -> PageWritePermit<'static> {
///     comparison.into()
/// }
/// ```
///
/// It also cannot become a dirty page or replay command:
///
/// ```compile_fail
/// use ntsql_page::{DirtyPage, DurablePageReconciliation};
///
/// fn cannot_create_dirty<const N: usize>(
///     comparison: DurablePageReconciliation,
/// ) -> DirtyPage<N> {
///     comparison.into()
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurablePageReconciliation {
    /// Neither a stored snapshot nor a durable page WAL record was observed.
    NoDurableState,
    /// The stored snapshot exactly matches the latest durable page WAL record.
    ExactCurrent {
        /// Exact durable position shared by WAL and store observations.
        durable_position: LogSequenceNumber,
    },
    /// The stored snapshot is backed by WAL but a later durable full image
    /// exists.
    StoreBehind {
        /// Durable WAL position backing the current stored snapshot.
        stored_position: LogSequenceNumber,
        /// Highest observed durable page WAL position for this page.
        latest_durable_position: LogSequenceNumber,
    },
    /// Durable page WAL state exists but no stored snapshot was observed.
    StoreMissing {
        /// Highest observed durable page WAL position for this page.
        latest_durable_position: LogSequenceNumber,
    },
}

/// Contradiction that prevents authoritative physical page reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DurablePageReconciliationError {
    /// The supplied snapshot belongs to another page.
    UnexpectedSnapshotPage {
        expected: PageNumber,
        actual: PageNumber,
    },
    /// The supplied snapshot position belongs to another WAL lineage.
    ForeignSnapshotLineage {
        page_number: PageNumber,
        position: LogSequenceNumber,
    },
    /// A supplied WAL observation belongs to another page.
    UnexpectedWalPage {
        expected: PageNumber,
        actual: PageNumber,
        position: LogSequenceNumber,
    },
    /// A supplied WAL observation belongs to another lineage.
    ForeignWalLineage {
        page_number: PageNumber,
        position: LogSequenceNumber,
    },
    /// Two adjacent observations reuse one position with identical payload.
    DuplicateWalPosition {
        page_number: PageNumber,
        position: LogSequenceNumber,
    },
    /// Two adjacent observations reuse one position with different payload.
    ContradictoryWalPosition {
        page_number: PageNumber,
        position: LogSequenceNumber,
    },
    /// WAL observations were not supplied in strictly increasing durable order.
    NonAdvancingWalPosition {
        page_number: PageNumber,
        previous: LogSequenceNumber,
        actual: LogSequenceNumber,
    },
    /// The snapshot's required position has no matching durable page WAL
    /// observation.
    SnapshotPositionUnbacked {
        page_number: PageNumber,
        position: LogSequenceNumber,
    },
    /// Snapshot metadata or bytes contradict the WAL observation at the same
    /// position.
    SnapshotPayloadContradiction {
        page_number: PageNumber,
        position: LogSequenceNumber,
    },
}

impl fmt::Display for DurablePageReconciliationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedSnapshotPage { expected, actual } => write!(
                formatter,
                "expected page {} but snapshot describes page {}",
                expected.get(),
                actual.get()
            ),
            Self::ForeignSnapshotLineage {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} snapshot position {} belongs to another WAL lineage",
                page_number.get(),
                position.get()
            ),
            Self::UnexpectedWalPage {
                expected,
                actual,
                position,
            } => write!(
                formatter,
                "expected page {} but WAL position {} describes page {}",
                expected.get(),
                position.get(),
                actual.get()
            ),
            Self::ForeignWalLineage {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} WAL position {} belongs to another lineage",
                page_number.get(),
                position.get()
            ),
            Self::DuplicateWalPosition {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} repeats WAL position {} with identical payload",
                page_number.get(),
                position.get()
            ),
            Self::ContradictoryWalPosition {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} repeats WAL position {} with contradictory payload",
                page_number.get(),
                position.get()
            ),
            Self::NonAdvancingWalPosition {
                page_number,
                previous,
                actual,
            } => write!(
                formatter,
                "page {} WAL position {} does not advance beyond {}",
                page_number.get(),
                actual.get(),
                previous.get()
            ),
            Self::SnapshotPositionUnbacked {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} snapshot position {} has no matching durable page WAL record",
                page_number.get(),
                position.get()
            ),
            Self::SnapshotPayloadContradiction {
                page_number,
                position,
            } => write!(
                formatter,
                "page {} snapshot contradicts durable WAL payload at position {}",
                page_number.get(),
                position.get()
            ),
        }
    }
}

impl Error for DurablePageReconciliationError {}

/// Reconciles one page's ordered durable WAL observations with its optional
/// current stored snapshot without authorizing any physical mutation.
///
/// `wal_observations` must contain every durable full-image WAL observation for
/// `page_number` in strictly increasing log order. Gaps are valid because
/// transaction records and writes for other pages may occupy intervening
/// positions.
pub fn reconcile_durable_page<'observation, const N: usize, Observations>(
    expected_lineage: &LogLineage,
    page_number: PageNumber,
    snapshot: Option<&StoredPageSnapshotObservation<N>>,
    wal_observations: Observations,
) -> Result<DurablePageReconciliation, DurablePageReconciliationError>
where
    Observations: IntoIterator<Item = &'observation DurablePageWalObservation<N>>,
{
    if let Some(snapshot) = snapshot {
        if snapshot.page_number() != page_number {
            return Err(DurablePageReconciliationError::UnexpectedSnapshotPage {
                expected: page_number,
                actual: snapshot.page_number(),
            });
        }
        if !expected_lineage.same_lineage(snapshot.required_position().lineage()) {
            return Err(DurablePageReconciliationError::ForeignSnapshotLineage {
                page_number,
                position: snapshot.required_position().clone(),
            });
        }
    }

    let mut previous: Option<&DurablePageWalObservation<N>> = None;
    let mut latest_position = None;
    let mut snapshot_backed = false;
    for observation in wal_observations {
        if observation.page_number() != page_number {
            return Err(DurablePageReconciliationError::UnexpectedWalPage {
                expected: page_number,
                actual: observation.page_number(),
                position: observation.position().clone(),
            });
        }
        if !expected_lineage.same_lineage(observation.position().lineage()) {
            return Err(DurablePageReconciliationError::ForeignWalLineage {
                page_number,
                position: observation.position().clone(),
            });
        }

        if let Some(previous) = previous {
            if observation.position().get() == previous.position().get() {
                let reason = if observation.page_version() == previous.page_version()
                    && observation.image().bytes() == previous.image().bytes()
                {
                    DurablePageReconciliationError::DuplicateWalPosition {
                        page_number,
                        position: observation.position().clone(),
                    }
                } else {
                    DurablePageReconciliationError::ContradictoryWalPosition {
                        page_number,
                        position: observation.position().clone(),
                    }
                };
                return Err(reason);
            }
            if observation.position().get() < previous.position().get() {
                return Err(DurablePageReconciliationError::NonAdvancingWalPosition {
                    page_number,
                    previous: previous.position().clone(),
                    actual: observation.position().clone(),
                });
            }
        }

        if let Some(snapshot) = snapshot
            && observation.position().get() == snapshot.required_position().get()
        {
            if observation.page_version() != snapshot.page_version()
                || observation.image().bytes() != snapshot.image().bytes()
            {
                return Err(
                    DurablePageReconciliationError::SnapshotPayloadContradiction {
                        page_number,
                        position: snapshot.required_position().clone(),
                    },
                );
            }
            snapshot_backed = true;
        }

        latest_position = Some(observation.position().get());
        previous = Some(observation);
    }

    let Some(latest_position) = latest_position else {
        return match snapshot {
            Some(snapshot) => Err(DurablePageReconciliationError::SnapshotPositionUnbacked {
                page_number,
                position: snapshot.required_position().clone(),
            }),
            None => Ok(DurablePageReconciliation::NoDurableState),
        };
    };

    let latest_durable_position = expected_lineage.position(latest_position);
    let Some(snapshot) = snapshot else {
        return Ok(DurablePageReconciliation::StoreMissing {
            latest_durable_position,
        });
    };
    if !snapshot_backed {
        return Err(DurablePageReconciliationError::SnapshotPositionUnbacked {
            page_number,
            position: snapshot.required_position().clone(),
        });
    }
    if snapshot.required_position().get() == latest_position {
        Ok(DurablePageReconciliation::ExactCurrent {
            durable_position: latest_durable_position,
        })
    } else {
        Ok(DurablePageReconciliation::StoreBehind {
            stored_position: snapshot.required_position().clone(),
            latest_durable_position,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, error::Error, fmt, num::NonZeroU64};

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum FakeError {
        LogAppend,
        LogFlush,
        StoreWrite,
    }

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::LogAppend => formatter.write_str("log append failed"),
                Self::LogFlush => formatter.write_str("log flush failed"),
                Self::StoreWrite => formatter.write_str("store write failed"),
            }
        }
    }

    impl Error for FakeError {}

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum EventKind {
        Unused,
        Append,
        Flush,
        Write,
    }

    struct CallTrace {
        len: Cell<usize>,
        kinds: [Cell<EventKind>; 4],
        positions: [Cell<u64>; 4],
        matches_expected: [Cell<bool>; 4],
        page_numbers: [Cell<u64>; 4],
    }

    impl CallTrace {
        const fn new() -> Self {
            Self {
                len: Cell::new(0),
                kinds: [
                    Cell::new(EventKind::Unused),
                    Cell::new(EventKind::Unused),
                    Cell::new(EventKind::Unused),
                    Cell::new(EventKind::Unused),
                ],
                positions: [Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0)],
                matches_expected: [
                    Cell::new(false),
                    Cell::new(false),
                    Cell::new(false),
                    Cell::new(false),
                ],
                page_numbers: [Cell::new(0), Cell::new(0), Cell::new(0), Cell::new(0)],
            }
        }

        fn push(&self, kind: EventKind, position: u64, matches_expected: bool, page_number: u64) {
            let index = self.len.get();
            assert!(index < self.kinds.len());
            self.kinds[index].set(kind);
            self.positions[index].set(position);
            self.matches_expected[index].set(matches_expected);
            self.page_numbers[index].set(page_number);
            self.len.set(index + 1);
        }

        fn len(&self) -> usize {
            self.len.get()
        }

        fn kind(&self, index: usize) -> EventKind {
            self.kinds[index].get()
        }

        fn position(&self, index: usize) -> u64 {
            self.positions[index].get()
        }

        fn matches_expected(&self, index: usize) -> bool {
            self.matches_expected[index].get()
        }

        fn page_number(&self, index: usize) -> u64 {
            self.page_numbers[index].get()
        }
    }

    struct FakeLog<'trace> {
        lineage: LogLineage,
        expected_position: LogSequenceNumber,
        append_position: LogSequenceNumber,
        trace: &'trace CallTrace,
        fail_append: bool,
        fail_flush: bool,
        lineage_after_append: Option<LogLineage>,
    }

    impl<'trace> FakeLog<'trace> {
        fn new(
            lineage: LogLineage,
            expected_position: LogSequenceNumber,
            trace: &'trace CallTrace,
        ) -> Self {
            Self {
                lineage,
                append_position: expected_position.clone(),
                expected_position,
                trace,
                fail_append: false,
                fail_flush: false,
                lineage_after_append: None,
            }
        }
    }

    impl LogDurability for FakeLog<'_> {
        type Error = FakeError;

        fn lineage(&self) -> &LogLineage {
            &self.lineage
        }

        fn flush_through(&mut self, position: &LogSequenceNumber) -> Result<(), Self::Error> {
            self.trace.push(
                EventKind::Flush,
                position.get(),
                position == &self.expected_position,
                0,
            );
            if self.fail_flush {
                Err(FakeError::LogFlush)
            } else {
                Ok(())
            }
        }
    }

    impl<const N: usize> PageLog<N> for FakeLog<'_> {
        fn append_page(
            &mut self,
            page: &UnloggedPage<N>,
        ) -> Result<LogSequenceNumber, Self::Error> {
            self.trace.push(
                EventKind::Append,
                self.append_position.get(),
                page.address().lineage().same_lineage(&self.lineage),
                page.address().number().get(),
            );
            if let Some(lineage) = self.lineage_after_append.take() {
                self.lineage = lineage;
            }
            if self.fail_append {
                Err(FakeError::LogAppend)
            } else {
                Ok(self.append_position.clone())
            }
        }
    }

    struct FakeStore<'trace> {
        lineage: LogLineage,
        expected_position: LogSequenceNumber,
        trace: &'trace CallTrace,
        fail_write: bool,
    }

    impl<'trace> FakeStore<'trace> {
        fn new(
            lineage: LogLineage,
            expected_position: LogSequenceNumber,
            trace: &'trace CallTrace,
        ) -> Self {
            Self {
                lineage,
                expected_position,
                trace,
                fail_write: false,
            }
        }
    }

    impl<const N: usize> PageStore<N> for FakeStore<'_> {
        type Error = FakeError;

        fn lineage(&self) -> &LogLineage {
            &self.lineage
        }

        fn write_page(
            &mut self,
            page: &DirtyPage<N>,
            permit: PageWritePermit<'_>,
        ) -> Result<(), Self::Error> {
            self.trace.push(
                EventKind::Write,
                permit.durable_position().get(),
                permit.durable_position() == page.required_position()
                    && permit.durable_position() == &self.expected_position,
                page.address().number().get(),
            );
            if self.fail_write {
                Err(FakeError::StoreWrite)
            } else {
                Ok(())
            }
        }
    }

    fn page_number(value: u64) -> PageNumber {
        let number = PageNumber::new(value);
        assert!(number.is_some());
        let Some(number) = number else {
            return PageNumber(NonZeroU64::MIN);
        };
        number
    }

    fn page_image<const N: usize>(bytes: [u8; N]) -> PageImage<N> {
        let image = PageImage::new(bytes);
        assert!(image.is_ok());
        let Ok(image) = image else {
            return PageImage { bytes };
        };
        image
    }

    fn dirty_page<const N: usize>(
        lineage: &LogLineage,
        number: u64,
        version: u64,
        bytes: [u8; N],
        required_position: LogSequenceNumber,
    ) -> DirtyPage<N> {
        let dirty = DirtyPage::new(
            PageAddress::new(lineage, page_number(number)),
            PageVersion::new(version),
            page_image(bytes),
            required_position,
        );
        assert!(dirty.is_ok());
        let Ok(dirty) = dirty else {
            return DirtyPage {
                address: PageAddress::new(lineage, page_number(number)),
                version: PageVersion::new(version),
                image: page_image(bytes),
                required_position: lineage.position(1),
            };
        };
        dirty
    }

    fn unlogged_page<const N: usize>(
        lineage: &LogLineage,
        number: u64,
        version: u64,
        bytes: [u8; N],
    ) -> UnloggedPage<N> {
        UnloggedPage::new(
            PageAddress::new(lineage, page_number(number)),
            PageVersion::new(version),
            page_image(bytes),
        )
    }

    fn wal_observation<const N: usize>(
        lineage: &LogLineage,
        number: u64,
        version: u64,
        bytes: [u8; N],
        position: u64,
    ) -> DurablePageWalObservation<N> {
        let observation = DurablePageWalObservation::new(
            page_number(number),
            PageVersion::new(version),
            page_image(bytes),
            lineage.position(position),
        );
        assert!(observation.is_ok());
        let Ok(observation) = observation else {
            return DurablePageWalObservation {
                observation: PageRecoveryObservation {
                    page_number: page_number(number),
                    page_version: PageVersion::new(version),
                    image: page_image(bytes),
                    position: lineage.position(position),
                },
            };
        };
        observation
    }

    fn stored_observation<const N: usize>(
        lineage: &LogLineage,
        number: u64,
        version: u64,
        bytes: [u8; N],
        position: u64,
    ) -> StoredPageSnapshotObservation<N> {
        let observation = StoredPageSnapshotObservation::new(
            page_number(number),
            PageVersion::new(version),
            page_image(bytes),
            lineage.position(position),
        );
        assert!(observation.is_ok());
        let Ok(observation) = observation else {
            return StoredPageSnapshotObservation {
                observation: PageRecoveryObservation {
                    page_number: page_number(number),
                    page_version: PageVersion::new(version),
                    image: page_image(bytes),
                    position: lineage.position(position),
                },
            };
        };
        observation
    }

    #[test]
    fn stages_flushes_and_writes_one_exact_page_image() {
        let trace = CallTrace::new();
        let lineage = LogLineage::new();
        let required_position = lineage.position(7);
        let bytes = [41_u8, 42, 43, 44];
        let page = unlogged_page(&lineage, 21, 12, bytes);
        let mut log = FakeLog::new(lineage.clone(), required_position.clone(), &trace);
        let mut store = FakeStore::new(lineage.clone(), required_position.clone(), &trace);

        let dirty = stage_page_write(&mut log, page);
        assert!(dirty.is_ok());
        let Ok(dirty) = dirty else {
            return;
        };
        let clean = flush_dirty_page(&mut log, &mut store, dirty);

        assert!(clean.is_ok());
        let Ok(clean) = clean else {
            return;
        };
        assert_eq!(trace.len(), 3);
        assert_eq!(trace.kind(0), EventKind::Append);
        assert_eq!(trace.kind(1), EventKind::Flush);
        assert_eq!(trace.kind(2), EventKind::Write);
        assert_eq!(trace.position(0), 7);
        assert_eq!(trace.position(1), 7);
        assert_eq!(trace.position(2), 7);
        assert!(trace.matches_expected(0));
        assert!(trace.matches_expected(1));
        assert!(trace.matches_expected(2));
        assert_eq!(trace.page_number(0), 21);
        assert_eq!(trace.page_number(2), 21);
        assert_eq!(
            clean.address(),
            &PageAddress::new(&lineage, page_number(21))
        );
        assert_eq!(clean.version(), PageVersion::new(12));
        assert_eq!(clean.image().bytes(), &bytes);
        assert_eq!(clean.required_position(), &required_position);
    }

    #[test]
    fn foreign_log_rejects_unlogged_page_before_append() {
        let trace = CallTrace::new();
        let lineage = LogLineage::new();
        let foreign_lineage = LogLineage::new();
        let page = unlogged_page(&lineage, 22, 13, [45_u8, 46]);
        let mut log = FakeLog::new(foreign_lineage.clone(), foreign_lineage.position(9), &trace);

        let error = stage_page_write(&mut log, page);

        assert!(error.is_err());
        let Err(error) = error else {
            return;
        };
        assert_eq!(trace.len(), 0);
        let StagePageWriteError::Rejected(error) = error else {
            return;
        };
        assert_eq!(error.reason(), StagePageWriteRejectionReason::ForeignLog);
        let page = error.into_page();
        assert_eq!(page.address(), &PageAddress::new(&lineage, page_number(22)));
        assert_eq!(page.version(), PageVersion::new(13));
        assert_eq!(page.image().bytes(), &[45_u8, 46]);
    }

    #[test]
    fn append_failure_is_terminal_and_preserves_cause() {
        let trace = CallTrace::new();
        let lineage = LogLineage::new();
        let page = unlogged_page(&lineage, 23, 14, [47_u8, 48]);
        let mut log = FakeLog::new(lineage.clone(), lineage.position(11), &trace);
        log.fail_append = true;

        let error = stage_page_write(&mut log, page);

        assert!(error.is_err());
        let Err(error) = error else {
            return;
        };
        assert_eq!(trace.len(), 1);
        assert_eq!(trace.kind(0), EventKind::Append);
        let StagePageWriteError::Append(error) = error else {
            return;
        };
        assert_eq!(error.cause(), &FakeError::LogAppend);
        let (page, source) = error.into_parts();
        assert_eq!(source, FakeError::LogAppend);
        assert_eq!(page.address().number().get(), 23);
        assert_eq!(page.version(), PageVersion::new(14));
        assert_eq!(page.image().bytes(), &[47_u8, 48]);
        assert_eq!(page.observed_position(), None);
    }

    #[test]
    fn foreign_append_position_is_terminal() {
        let trace = CallTrace::new();
        let lineage = LogLineage::new();
        let foreign_lineage = LogLineage::new();
        let page = unlogged_page(&lineage, 24, 15, [49_u8, 50]);
        let mut log = FakeLog::new(lineage.clone(), lineage.position(13), &trace);
        log.append_position = foreign_lineage.position(17);

        let error = stage_page_write(&mut log, page);

        assert!(error.is_err());
        let Err(error) = error else {
            return;
        };
        assert_eq!(trace.len(), 1);
        let StagePageWriteError::InvalidEvidence(error) = error else {
            return;
        };
        assert_eq!(
            error.reason(),
            StagePageWriteEvidenceErrorReason::ForeignPosition
        );
        assert_eq!(
            error.page().observed_position(),
            Some(&foreign_lineage.position(17))
        );
        assert_eq!(error.page().address().number().get(), 24);
    }

    #[test]
    fn append_time_lineage_rotation_is_terminal() {
        let trace = CallTrace::new();
        let lineage = LogLineage::new();
        let replacement = LogLineage::new();
        let returned_position = lineage.position(19);
        let page = unlogged_page(&lineage, 25, 16, [51_u8, 52]);
        let mut log = FakeLog::new(lineage, returned_position.clone(), &trace);
        log.lineage_after_append = Some(replacement);

        let error = stage_page_write(&mut log, page);

        assert!(error.is_err());
        let Err(error) = error else {
            return;
        };
        assert_eq!(trace.len(), 1);
        let StagePageWriteError::InvalidEvidence(error) = error else {
            return;
        };
        assert_eq!(
            error.reason(),
            StagePageWriteEvidenceErrorReason::LogLineageChanged
        );
        assert_eq!(error.page().observed_position(), Some(&returned_position));
        assert_eq!(error.page().address().number().get(), 25);
    }

    #[test]
    fn flushes_before_writing_with_exact_position_and_permit() {
        let trace = CallTrace::new();
        let lineage = LogLineage::new();
        let required_position = lineage.position(11);
        let bytes = [1_u8, 2, 3, 4];
        let dirty = dirty_page(&lineage, 7, 3, bytes, required_position.clone());
        let mut log = FakeLog::new(lineage.clone(), required_position.clone(), &trace);
        let mut store = FakeStore::new(lineage.clone(), required_position.clone(), &trace);

        let clean = flush_dirty_page(&mut log, &mut store, dirty);

        assert!(clean.is_ok());
        let Ok(clean) = clean else {
            return;
        };
        assert_eq!(trace.len(), 2);
        assert_eq!(trace.kind(0), EventKind::Flush);
        assert_eq!(trace.position(0), 11);
        assert!(trace.matches_expected(0));
        assert_eq!(trace.kind(1), EventKind::Write);
        assert_eq!(trace.position(1), 11);
        assert!(trace.matches_expected(1));
        assert_eq!(trace.page_number(1), 7);
        assert_eq!(clean.address(), &PageAddress::new(&lineage, page_number(7)));
        assert_eq!(clean.version(), PageVersion::new(3));
        assert_eq!(clean.image().bytes(), &bytes);
        assert_eq!(clean.required_position(), &required_position);
    }

    #[test]
    fn foreign_log_rejection_happens_before_any_port_call() {
        let trace = CallTrace::new();
        let lineage = LogLineage::new();
        let foreign_lineage = LogLineage::new();
        let required_position = lineage.position(13);
        let bytes = [5_u8, 6, 7, 8];
        let dirty = dirty_page(&lineage, 9, 4, bytes, required_position.clone());
        let mut log = FakeLog::new(foreign_lineage, required_position.clone(), &trace);
        let mut store = FakeStore::new(lineage.clone(), required_position, &trace);

        let error = flush_dirty_page(&mut log, &mut store, dirty);

        assert!(error.is_err());
        let Err(error) = error else {
            return;
        };
        assert_eq!(trace.len(), 0);
        assert!(matches!(error, FlushDirtyPageError::Rejected(_)));
        let rejection = match error {
            FlushDirtyPageError::Rejected(rejection) => rejection,
            FlushDirtyPageError::LogFlush(_) | FlushDirtyPageError::StoreWrite(_) => return,
        };
        assert_eq!(
            rejection.reason(),
            FlushDirtyPageRejectionReason::ForeignLog
        );
        assert_eq!(
            rejection.page().address(),
            &PageAddress::new(&lineage, page_number(9))
        );
        assert_eq!(rejection.page().version(), PageVersion::new(4));
        assert_eq!(rejection.page().image().bytes(), &bytes);
        assert_eq!(rejection.page().required_position().get(), 13);
    }

    #[test]
    fn foreign_store_rejection_happens_before_any_port_call() {
        let trace = CallTrace::new();
        let lineage = LogLineage::new();
        let foreign_lineage = LogLineage::new();
        let required_position = lineage.position(17);
        let bytes = [9_u8, 10, 11, 12];
        let dirty = dirty_page(&lineage, 10, 5, bytes, required_position.clone());
        let mut log = FakeLog::new(lineage.clone(), required_position.clone(), &trace);
        let mut store = FakeStore::new(foreign_lineage, required_position, &trace);

        let error = flush_dirty_page(&mut log, &mut store, dirty);

        assert!(error.is_err());
        let Err(error) = error else {
            return;
        };
        assert_eq!(trace.len(), 0);
        assert!(matches!(error, FlushDirtyPageError::Rejected(_)));
        let rejection = match error {
            FlushDirtyPageError::Rejected(rejection) => rejection,
            FlushDirtyPageError::LogFlush(_) | FlushDirtyPageError::StoreWrite(_) => return,
        };
        assert_eq!(
            rejection.reason(),
            FlushDirtyPageRejectionReason::ForeignStore
        );
        assert_eq!(
            rejection.page().address(),
            &PageAddress::new(&lineage, page_number(10))
        );
        assert_eq!(rejection.page().version(), PageVersion::new(5));
        assert_eq!(rejection.page().image().bytes(), &bytes);
        assert_eq!(rejection.page().required_position().get(), 17);
    }

    #[test]
    fn wal_failure_preserves_dirty_page_for_retry_without_store_call() {
        let failed_trace = CallTrace::new();
        let retry_trace = CallTrace::new();
        let lineage = LogLineage::new();
        let required_position = lineage.position(19);
        let bytes = [13_u8, 14, 15, 16];
        let dirty = dirty_page(&lineage, 11, 6, bytes, required_position.clone());
        let mut log = FakeLog::new(lineage.clone(), required_position.clone(), &failed_trace);
        log.fail_flush = true;
        let mut store = FakeStore::new(lineage.clone(), required_position.clone(), &failed_trace);

        let error = flush_dirty_page(&mut log, &mut store, dirty);

        assert!(error.is_err());
        let Err(error) = error else {
            return;
        };
        assert!(matches!(error, FlushDirtyPageError::LogFlush(_)));
        let dirty = match error {
            FlushDirtyPageError::LogFlush(error) => {
                assert_eq!(failed_trace.len(), 1);
                assert_eq!(failed_trace.kind(0), EventKind::Flush);
                assert_eq!(failed_trace.position(0), 19);
                assert!(failed_trace.matches_expected(0));
                assert_eq!(error.cause(), &FakeError::LogFlush);
                let (dirty, source) = error.into_parts();
                assert_eq!(source, FakeError::LogFlush);
                dirty
            }
            FlushDirtyPageError::Rejected(_) | FlushDirtyPageError::StoreWrite(_) => return,
        };

        let mut retry_log = FakeLog::new(lineage.clone(), required_position.clone(), &retry_trace);
        let mut retry_store = FakeStore::new(lineage, required_position.clone(), &retry_trace);
        let clean = flush_dirty_page(&mut retry_log, &mut retry_store, dirty);
        assert!(clean.is_ok());
        assert_eq!(retry_trace.len(), 2);
        assert_eq!(retry_trace.kind(0), EventKind::Flush);
        assert_eq!(retry_trace.kind(1), EventKind::Write);
        assert_eq!(retry_trace.position(1), 19);
        assert!(retry_trace.matches_expected(1));
        let Ok(clean) = clean else {
            return;
        };
        assert_eq!(clean.address().number().get(), 11);
        assert_eq!(clean.version(), PageVersion::new(6));
        assert_eq!(clean.image().bytes(), &bytes);
        assert_eq!(clean.required_position(), &required_position);
    }

    #[test]
    fn store_failure_becomes_terminal_indeterminate_after_wal_flush() {
        let trace = CallTrace::new();
        let lineage = LogLineage::new();
        let required_position = lineage.position(23);
        let bytes = [17_u8, 18, 19, 20];
        let dirty = dirty_page(&lineage, 12, 7, bytes, required_position.clone());
        let mut log = FakeLog::new(lineage.clone(), required_position.clone(), &trace);
        let mut store = FakeStore::new(lineage, required_position.clone(), &trace);
        store.fail_write = true;

        let error = flush_dirty_page(&mut log, &mut store, dirty);

        assert!(error.is_err());
        let Err(error) = error else {
            return;
        };
        assert!(matches!(error, FlushDirtyPageError::StoreWrite(_)));
        let error = match error {
            FlushDirtyPageError::StoreWrite(error) => error,
            FlushDirtyPageError::Rejected(_) | FlushDirtyPageError::LogFlush(_) => return,
        };
        assert_eq!(trace.len(), 2);
        assert_eq!(trace.kind(0), EventKind::Flush);
        assert_eq!(trace.kind(1), EventKind::Write);
        assert_eq!(trace.position(0), 23);
        assert_eq!(trace.position(1), 23);
        assert!(trace.matches_expected(0));
        assert!(trace.matches_expected(1));
        assert_eq!(trace.page_number(1), 12);
        assert_eq!(error.cause(), &FakeError::StoreWrite);
        let (page, source) = error.into_parts();
        assert_eq!(source, FakeError::StoreWrite);
        assert_eq!(page.address().number().get(), 12);
        assert_eq!(page.version(), PageVersion::new(7));
        assert_eq!(page.image().bytes(), &bytes);
        assert_eq!(page.required_position(), &required_position);
    }

    #[test]
    fn zero_length_page_image_rejection_retains_bytes() {
        let error = PageImage::<0>::new([]);
        assert!(error.is_err());
        let Err(error) = error else {
            return;
        };
        assert_eq!(error.bytes(), &[]);
        assert_eq!(error.into_bytes(), []);
    }

    #[test]
    fn dirty_page_construction_mismatch_retains_all_inputs() {
        let lineage = LogLineage::new();
        let foreign_lineage = LogLineage::new();
        let bytes = [21_u8, 22, 23, 24];
        let address = PageAddress::new(&lineage, page_number(13));
        let image = page_image(bytes);
        let required_position = foreign_lineage.position(29);

        let error = DirtyPage::new(
            address,
            PageVersion::new(8),
            image,
            required_position.clone(),
        );

        assert!(error.is_err());
        let Err(error) = error else {
            return;
        };
        assert_eq!(
            error.reason(),
            DirtyPageConstructionErrorReason::ForeignRequiredPosition
        );
        let (address, version, image, position) = error.into_parts();
        assert_eq!(address, PageAddress::new(&lineage, page_number(13)));
        assert_eq!(version, PageVersion::new(8));
        assert_eq!(image.bytes(), &bytes);
        assert_eq!(position, required_position);
    }

    #[test]
    fn same_page_number_from_different_lineages_is_not_equal() {
        let first = LogLineage::new();
        let second = LogLineage::new();
        let number = page_number(14);

        assert_ne!(
            PageAddress::new(&first, number),
            PageAddress::new(&second, number)
        );
    }

    #[test]
    fn successful_clean_state_retains_all_fields() {
        let trace = CallTrace::new();
        let lineage = LogLineage::new();
        let required_position = lineage.position(31);
        let bytes = [25_u8, 26, 27, 28];
        let dirty = dirty_page(&lineage, 15, 9, bytes, required_position.clone());
        let mut log = FakeLog::new(lineage.clone(), required_position.clone(), &trace);
        let mut store = FakeStore::new(lineage.clone(), required_position.clone(), &trace);

        let clean = flush_dirty_page(&mut log, &mut store, dirty);

        assert!(clean.is_ok());
        let Ok(clean) = clean else {
            return;
        };
        let (address, version, image, position) = clean.into_parts();
        assert_eq!(address, PageAddress::new(&lineage, page_number(15)));
        assert_eq!(version, PageVersion::new(9));
        assert_eq!(image.bytes(), &bytes);
        assert_eq!(position, required_position);
    }

    #[test]
    fn recovery_observation_zero_position_retains_every_input() {
        let lineage = LogLineage::new();
        let number = page_number(31);
        let version = PageVersion::new(7);
        let bytes = [1_u8, 2];
        let position = lineage.position(0);

        let error =
            DurablePageWalObservation::new(number, version, page_image(bytes), position.clone());

        assert!(error.is_err());
        let Err(error) = error else {
            return;
        };
        assert_eq!(
            error.reason(),
            PageRecoveryObservationErrorReason::ZeroPosition
        );
        assert_eq!(error.page_number(), number);
        assert_eq!(error.page_version(), version);
        assert_eq!(error.image().bytes(), &bytes);
        assert_eq!(error.position(), &position);
        let (actual_number, actual_version, image, actual_position, reason) = error.into_parts();
        assert_eq!(actual_number, number);
        assert_eq!(actual_version, version);
        assert_eq!(image.bytes(), &bytes);
        assert_eq!(actual_position, position);
        assert_eq!(reason, PageRecoveryObservationErrorReason::ZeroPosition);
    }

    #[test]
    fn durable_page_reconciliation_reports_exact_current() {
        let lineage = LogLineage::new();
        let number = page_number(32);
        let records = [
            wal_observation(&lineage, 32, 1, [1_u8, 2], 3),
            wal_observation(&lineage, 32, 2, [3_u8, 4], 7),
        ];
        let snapshot = stored_observation(&lineage, 32, 2, [3_u8, 4], 7);

        let result = reconcile_durable_page(&lineage, number, Some(&snapshot), records.iter());

        assert_eq!(
            result,
            Ok(DurablePageReconciliation::ExactCurrent {
                durable_position: lineage.position(7),
            })
        );
    }

    #[test]
    fn durable_page_reconciliation_reports_store_behind_without_ordering_versions() {
        let lineage = LogLineage::new();
        let number = page_number(33);
        let records = [
            wal_observation(&lineage, 33, 99, [5_u8, 6], 5),
            wal_observation(&lineage, 33, 1, [7_u8, 8], 9),
        ];
        let snapshot = stored_observation(&lineage, 33, 99, [5_u8, 6], 5);

        let result = reconcile_durable_page(&lineage, number, Some(&snapshot), records.iter());

        assert_eq!(
            result,
            Ok(DurablePageReconciliation::StoreBehind {
                stored_position: lineage.position(5),
                latest_durable_position: lineage.position(9),
            })
        );
    }

    #[test]
    fn durable_page_reconciliation_reports_missing_and_no_state() {
        let lineage = LogLineage::new();
        let number = page_number(34);
        let records = [
            wal_observation(&lineage, 34, 1, [9_u8, 10], 2),
            wal_observation(&lineage, 34, 2, [11_u8, 12], 8),
        ];

        let missing = reconcile_durable_page(&lineage, number, None, records.iter());
        let empty = reconcile_durable_page(
            &lineage,
            number,
            None,
            std::iter::empty::<&DurablePageWalObservation<2>>(),
        );

        assert_eq!(
            missing,
            Ok(DurablePageReconciliation::StoreMissing {
                latest_durable_position: lineage.position(8),
            })
        );
        assert_eq!(empty, Ok(DurablePageReconciliation::NoDurableState));
    }

    #[test]
    fn snapshot_position_must_be_backed_by_a_durable_page_record() {
        let lineage = LogLineage::new();
        let number = page_number(35);
        let records = [
            wal_observation(&lineage, 35, 1, [13_u8, 14], 3),
            wal_observation(&lineage, 35, 2, [15_u8, 16], 9),
        ];
        let within_range = stored_observation(&lineage, 35, 7, [17_u8, 18], 8);
        let beyond_range = stored_observation(&lineage, 35, 8, [19_u8, 20], 12);

        let within_result =
            reconcile_durable_page(&lineage, number, Some(&within_range), records.iter());
        let beyond_result =
            reconcile_durable_page(&lineage, number, Some(&beyond_range), records.iter());

        assert_eq!(
            within_result,
            Err(DurablePageReconciliationError::SnapshotPositionUnbacked {
                page_number: number,
                position: lineage.position(8),
            })
        );
        assert_eq!(
            beyond_result,
            Err(DurablePageReconciliationError::SnapshotPositionUnbacked {
                page_number: number,
                position: lineage.position(12),
            })
        );
    }

    #[test]
    fn reconciliation_rejects_snapshot_payload_contradiction_at_backing_position() {
        let lineage = LogLineage::new();
        let number = page_number(36);
        let records = [wal_observation(&lineage, 36, 4, [19_u8, 20], 6)];
        let wrong_version = stored_observation(&lineage, 36, 5, [19_u8, 20], 6);
        let wrong_bytes = stored_observation(&lineage, 36, 4, [21_u8, 22], 6);

        let version_result =
            reconcile_durable_page(&lineage, number, Some(&wrong_version), records.iter());
        let bytes_result =
            reconcile_durable_page(&lineage, number, Some(&wrong_bytes), records.iter());
        let expected = Err(
            DurablePageReconciliationError::SnapshotPayloadContradiction {
                page_number: number,
                position: lineage.position(6),
            },
        );

        assert_eq!(version_result, expected);
        assert_eq!(
            bytes_result,
            Err(
                DurablePageReconciliationError::SnapshotPayloadContradiction {
                    page_number: number,
                    position: lineage.position(6),
                }
            )
        );
    }

    #[test]
    fn reconciliation_rejects_foreign_lineages_before_numeric_ordering() {
        let lineage = LogLineage::new();
        let foreign = LogLineage::new();
        let number = page_number(37);
        let foreign_snapshot = stored_observation(&foreign, 37, 1, [23_u8, 24], 2);
        let valid = wal_observation(&lineage, 37, 1, [23_u8, 24], 10);
        let foreign_record = wal_observation(&foreign, 37, 2, [25_u8, 26], 1);
        let records = [valid, foreign_record];

        let snapshot_result = reconcile_durable_page(
            &lineage,
            number,
            Some(&foreign_snapshot),
            std::iter::empty::<&DurablePageWalObservation<2>>(),
        );
        let wal_result = reconcile_durable_page(&lineage, number, None, records.iter());

        assert_eq!(
            snapshot_result,
            Err(DurablePageReconciliationError::ForeignSnapshotLineage {
                page_number: number,
                position: foreign.position(2),
            })
        );
        assert_eq!(
            wal_result,
            Err(DurablePageReconciliationError::ForeignWalLineage {
                page_number: number,
                position: foreign.position(1),
            })
        );
    }

    #[test]
    fn reconciliation_rejects_observations_for_another_page() {
        let lineage = LogLineage::new();
        let expected = page_number(38);
        let snapshot = stored_observation(&lineage, 39, 1, [27_u8, 28], 2);
        let records = [wal_observation(&lineage, 40, 1, [29_u8, 30], 3)];

        let snapshot_result = reconcile_durable_page(
            &lineage,
            expected,
            Some(&snapshot),
            std::iter::empty::<&DurablePageWalObservation<2>>(),
        );
        let wal_result = reconcile_durable_page(&lineage, expected, None, records.iter());

        assert_eq!(
            snapshot_result,
            Err(DurablePageReconciliationError::UnexpectedSnapshotPage {
                expected,
                actual: page_number(39),
            })
        );
        assert_eq!(
            wal_result,
            Err(DurablePageReconciliationError::UnexpectedWalPage {
                expected,
                actual: page_number(40),
                position: lineage.position(3),
            })
        );
    }

    #[test]
    fn reconciliation_rejects_duplicate_and_contradictory_wal_positions() {
        let lineage = LogLineage::new();
        let number = page_number(41);
        let identical = [
            wal_observation(&lineage, 41, 1, [31_u8, 32], 4),
            wal_observation(&lineage, 41, 1, [31_u8, 32], 4),
        ];
        let contradictory = [
            wal_observation(&lineage, 41, 1, [31_u8, 32], 4),
            wal_observation(&lineage, 41, 2, [33_u8, 34], 4),
        ];

        let duplicate_result = reconcile_durable_page(&lineage, number, None, identical.iter());
        let contradiction_result =
            reconcile_durable_page(&lineage, number, None, contradictory.iter());

        assert_eq!(
            duplicate_result,
            Err(DurablePageReconciliationError::DuplicateWalPosition {
                page_number: number,
                position: lineage.position(4),
            })
        );
        assert_eq!(
            contradiction_result,
            Err(DurablePageReconciliationError::ContradictoryWalPosition {
                page_number: number,
                position: lineage.position(4),
            })
        );
    }

    #[test]
    fn reconciliation_rejects_nonadvancing_wal_order() {
        let lineage = LogLineage::new();
        let number = page_number(42);
        let records = [
            wal_observation(&lineage, 42, 1, [35_u8, 36], 12),
            wal_observation(&lineage, 42, 2, [37_u8, 38], 7),
        ];

        let result = reconcile_durable_page(&lineage, number, None, records.iter());

        assert_eq!(
            result,
            Err(DurablePageReconciliationError::NonAdvancingWalPosition {
                page_number: number,
                previous: lineage.position(12),
                actual: lineage.position(7),
            })
        );
    }
}
