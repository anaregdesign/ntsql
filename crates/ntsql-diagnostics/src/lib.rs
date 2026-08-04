//! I/O-free vocabulary for diagnostics exposed to database clients.
//!
//! Internal causes, backtraces, logging context, and transport failures belong
//! to the component that observed them. This crate carries only fields that an
//! engine component intends to expose to a client.

/// Client-visible diagnostic number.
///
/// The full unsigned width is preserved without assigning target-specific
/// meaning or imposing an unverified range.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DiagnosticNumber(u32);

impl DiagnosticNumber {
    /// Creates a diagnostic number without interpreting it.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the client-visible numeric value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// Client-visible diagnostic severity.
///
/// The full unsigned width is preserved until an approved compatibility
/// specification defines any target-specific interpretation.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DiagnosticSeverity(u8);

impl DiagnosticSeverity {
    /// Creates a severity without interpreting it.
    #[must_use]
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Returns the client-visible numeric value.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Client-visible diagnostic state.
///
/// State remains distinct from severity even though both use the same numeric
/// width on the client boundary.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DiagnosticState(u8);

impl DiagnosticState {
    /// Creates a state without interpreting it.
    #[must_use]
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Returns the client-visible numeric value.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Stable client-facing fields of one engine diagnostic.
///
/// This value deliberately does not implement [`std::error::Error`] and does
/// not contain an internal source chain. A future protocol adapter may encode
/// these fields, but protocol tokens are not part of this domain type.
///
/// ```compile_fail
/// use ntsql_diagnostics::ClientDiagnostic;
///
/// fn require_internal_error<T: std::error::Error>() {}
/// require_internal_error::<ClientDiagnostic>();
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientDiagnostic {
    number: DiagnosticNumber,
    severity: DiagnosticSeverity,
    state: DiagnosticState,
    message: Box<str>,
}

impl ClientDiagnostic {
    /// Creates a diagnostic while preserving every client-visible field.
    ///
    /// Numeric fields use distinct types so they cannot be interchanged.
    ///
    /// ```compile_fail
    /// use ntsql_diagnostics::{
    ///     ClientDiagnostic, DiagnosticNumber, DiagnosticSeverity, DiagnosticState,
    /// };
    ///
    /// let number = DiagnosticNumber::new(1);
    /// let severity = DiagnosticSeverity::new(2);
    /// let state = DiagnosticState::new(3);
    /// let _ = ClientDiagnostic::new(severity, number, state, "message");
    /// ```
    #[must_use]
    pub fn new(
        number: DiagnosticNumber,
        severity: DiagnosticSeverity,
        state: DiagnosticState,
        message: impl Into<Box<str>>,
    ) -> Self {
        Self {
            number,
            severity,
            state,
            message: message.into(),
        }
    }

    /// Returns the client-visible diagnostic number.
    #[must_use]
    pub const fn number(&self) -> DiagnosticNumber {
        self.number
    }

    /// Returns the client-visible severity.
    #[must_use]
    pub const fn severity(&self) -> DiagnosticSeverity {
        self.severity
    }

    /// Returns the client-visible state.
    #[must_use]
    pub const fn state(&self) -> DiagnosticState {
        self.state
    }

    /// Returns the exact client-visible message text.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}
