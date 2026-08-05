//! I/O-free compatibility policy shared by ntsql engine components.

use std::{collections::BTreeSet, error::Error, fmt, marker::PhantomData};

/// One externally observable aspect required in every conformance record.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ObservationDimension {
    /// Input acceptance and parse or bind behavior.
    Syntax,
    /// Client-visible protocol behavior.
    Wire,
    /// Typed result values and ordering.
    Result,
    /// Result-set and column metadata.
    Metadata,
    /// Errors, warnings, and connection diagnostics.
    Diagnostic,
    /// Transaction state and persistent side effects.
    TransactionalSideEffect,
    /// Configuration, lifecycle, and administration behavior.
    Operational,
}

/// Canonical order of all required conformance dimensions.
pub const OBSERVATION_DIMENSIONS: [ObservationDimension; 7] = [
    ObservationDimension::Syntax,
    ObservationDimension::Wire,
    ObservationDimension::Result,
    ObservationDimension::Metadata,
    ObservationDimension::Diagnostic,
    ObservationDimension::TransactionalSideEffect,
    ObservationDimension::Operational,
];

/// Stable identifier of one exact compatibility target.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TargetId(Box<str>);

impl TargetId {
    /// Returns the contract identifier as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TargetId {
    type Error = CompatibilityContextError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if is_contract_identifier(&value) {
            Ok(Self(value.into_boxed_str()))
        } else {
            Err(CompatibilityContextError::InvalidTargetId)
        }
    }
}

/// Exact four-component Database Engine product version.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProductVersion(Box<str>);

impl ProductVersion {
    /// Returns the exact dotted version representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ProductVersion {
    type Error = CompatibilityContextError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if has_four_numeric_components(&value) {
            Ok(Self(value.into_boxed_str()))
        } else {
            Err(CompatibilityContextError::InvalidProductVersion)
        }
    }
}

/// Database compatibility level selected for a context.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CompatibilityLevel(u16);

impl CompatibilityLevel {
    /// Creates a compatibility-level value recorded by a validated target.
    #[must_use]
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    /// Returns the numeric compatibility level.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Behavior selectors used to construct an immutable compatibility context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatibilityProfile {
    /// Exact target identifier.
    pub target_id: String,
    /// Product release label.
    pub product_release: String,
    /// Exact servicing update label.
    pub servicing_update: String,
    /// Exact four-component product version.
    pub product_version: String,
    /// Engine edition label.
    pub edition: String,
    /// Operating-system descriptor.
    pub operating_system: String,
    /// Processor architecture descriptor.
    pub architecture: String,
    /// Database compatibility level.
    pub compatibility_level: u16,
    /// Server and database collation.
    pub collation: String,
    /// Session language.
    pub language: String,
    /// Session language identifier.
    pub lcid: u32,
    /// Host and session timezone policy.
    pub timezone: String,
    /// Explicit session defaults applied before a request.
    pub session_defaults: Vec<String>,
}

/// Immutable collection of all behavior selectors for one exact target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatibilityContext {
    target_id: TargetId,
    product_release: Box<str>,
    servicing_update: Box<str>,
    product_version: ProductVersion,
    edition: Box<str>,
    operating_system: Box<str>,
    architecture: Box<str>,
    compatibility_level: CompatibilityLevel,
    collation: Box<str>,
    language: Box<str>,
    lcid: u32,
    timezone: Box<str>,
    session_defaults: Box<[String]>,
}

type ScopeBrandMarker<'scope> = (&'scope (), fn(&'scope ()) -> &'scope ());

/// Generative identity for one request's exact compatibility context.
///
/// Values carrying different private scope brands cannot be combined through a
/// public API that requires one brand. The scope borrows rather than copies the
/// selected context and can be created only by
/// [`CompatibilityContext::with_scope`].
#[derive(Clone, Copy, Debug)]
pub struct CompatibilityScope<'context, 'scope> {
    context: &'context CompatibilityContext,
    scope_brand: PhantomData<ScopeBrandMarker<'scope>>,
}

impl<'context, 'scope> CompatibilityScope<'context, 'scope> {
    /// Returns the exact immutable context selected for this request scope.
    #[must_use]
    pub const fn context(self) -> &'context CompatibilityContext {
        self.context
    }
}

impl CompatibilityContext {
    /// Validates and freezes one complete set of behavior selectors.
    pub fn try_new(profile: CompatibilityProfile) -> Result<Self, CompatibilityContextError> {
        require_nonempty("product_release", &profile.product_release)?;
        require_nonempty("servicing_update", &profile.servicing_update)?;
        require_nonempty("edition", &profile.edition)?;
        require_nonempty("operating_system", &profile.operating_system)?;
        require_nonempty("architecture", &profile.architecture)?;
        require_nonempty("collation", &profile.collation)?;
        require_nonempty("language", &profile.language)?;
        require_nonempty("timezone", &profile.timezone)?;
        validate_session_defaults(&profile.session_defaults)?;

        Ok(Self {
            target_id: TargetId::try_from(profile.target_id)?,
            product_release: profile.product_release.into_boxed_str(),
            servicing_update: profile.servicing_update.into_boxed_str(),
            product_version: ProductVersion::try_from(profile.product_version)?,
            edition: profile.edition.into_boxed_str(),
            operating_system: profile.operating_system.into_boxed_str(),
            architecture: profile.architecture.into_boxed_str(),
            compatibility_level: CompatibilityLevel::new(profile.compatibility_level),
            collation: profile.collation.into_boxed_str(),
            language: profile.language.into_boxed_str(),
            lcid: profile.lcid,
            timezone: profile.timezone.into_boxed_str(),
            session_defaults: profile.session_defaults.into_boxed_slice(),
        })
    }

    /// Runs an operation with a fresh, non-escaping request scope brand.
    ///
    /// Staged request values can carry the scope and require the same private
    /// brand in later APIs. Independently opened scopes therefore cannot be
    /// mixed accidentally:
    ///
    /// ```compile_fail
    /// use ntsql_compatibility::{CompatibilityContext, CompatibilityScope};
    ///
    /// fn require_same_scope<'context, 'scope>(
    ///     _left: CompatibilityScope<'context, 'scope>,
    ///     _right: CompatibilityScope<'context, 'scope>,
    /// ) {}
    ///
    /// fn cannot_mix<'context>(
    ///     left: &'context CompatibilityContext,
    ///     right: &'context CompatibilityContext,
    /// ) {
    ///     left.with_scope(|left_scope| {
    ///         right.with_scope(|right_scope| {
    ///             require_same_scope(left_scope, right_scope);
    ///         });
    ///     });
    /// }
    /// ```
    ///
    /// The fresh brand also cannot escape its callback:
    ///
    /// ```compile_fail
    /// use ntsql_compatibility::CompatibilityContext;
    ///
    /// fn cannot_escape(context: &CompatibilityContext) {
    ///     let _scope = context.with_scope(|scope| scope);
    /// }
    /// ```
    ///
    /// Erasing the concrete return type cannot make the brand `'static`:
    ///
    /// ```compile_fail
    /// use ntsql_compatibility::CompatibilityContext;
    ///
    /// fn cannot_erase(
    ///     context: &'static CompatibilityContext,
    /// ) -> Box<dyn FnOnce()> {
    ///     context.with_scope(|scope| -> Box<dyn FnOnce()> {
    ///         Box::new(move || {
    ///             let _context = scope.context();
    ///         })
    ///     })
    /// }
    /// ```
    pub fn with_scope<'context, R, F>(&'context self, operation: F) -> R
    where
        F: for<'scope> FnOnce(CompatibilityScope<'context, 'scope>) -> R,
    {
        operation(CompatibilityScope {
            context: self,
            scope_brand: PhantomData,
        })
    }

    /// Returns the exact compatibility target identifier.
    #[must_use]
    pub fn target_id(&self) -> &TargetId {
        &self.target_id
    }

    /// Returns the product release label.
    #[must_use]
    pub fn product_release(&self) -> &str {
        &self.product_release
    }

    /// Returns the exact servicing update label.
    #[must_use]
    pub fn servicing_update(&self) -> &str {
        &self.servicing_update
    }

    /// Returns the exact product version.
    #[must_use]
    pub fn product_version(&self) -> &ProductVersion {
        &self.product_version
    }

    /// Returns the Engine edition label.
    #[must_use]
    pub fn edition(&self) -> &str {
        &self.edition
    }

    /// Returns the operating-system descriptor.
    #[must_use]
    pub fn operating_system(&self) -> &str {
        &self.operating_system
    }

    /// Returns the processor architecture descriptor.
    #[must_use]
    pub fn architecture(&self) -> &str {
        &self.architecture
    }

    /// Returns the database compatibility level.
    #[must_use]
    pub const fn compatibility_level(&self) -> CompatibilityLevel {
        self.compatibility_level
    }

    /// Returns the configured server and database collation.
    #[must_use]
    pub fn collation(&self) -> &str {
        &self.collation
    }

    /// Returns the configured session language.
    #[must_use]
    pub fn language(&self) -> &str {
        &self.language
    }

    /// Returns the configured language identifier.
    #[must_use]
    pub const fn lcid(&self) -> u32 {
        self.lcid
    }

    /// Returns the configured timezone policy.
    #[must_use]
    pub fn timezone(&self) -> &str {
        &self.timezone
    }

    /// Returns the explicit session defaults in application order.
    #[must_use]
    pub fn session_defaults(&self) -> &[String] {
        &self.session_defaults
    }
}

/// Reason a compatibility profile could not become a context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompatibilityContextError {
    /// Target identifiers must follow the public contract grammar.
    InvalidTargetId,
    /// Product versions must have four numeric components.
    InvalidProductVersion,
    /// A required behavior selector was empty.
    EmptySelector(&'static str),
    /// At least one explicit session default is required.
    MissingSessionDefaults,
    /// An explicit session default was empty.
    EmptySessionDefault,
    /// The same session default occurred more than once.
    DuplicateSessionDefault(String),
}

impl fmt::Display for CompatibilityContextError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTargetId => formatter.write_str("target identifier is malformed"),
            Self::InvalidProductVersion => {
                formatter.write_str("product version must have four numeric components")
            }
            Self::EmptySelector(field) => write!(formatter, "behavior selector {field} is empty"),
            Self::MissingSessionDefaults => {
                formatter.write_str("at least one session default is required")
            }
            Self::EmptySessionDefault => formatter.write_str("session default is empty"),
            Self::DuplicateSessionDefault(setting) => {
                write!(formatter, "session default is duplicated: {setting}")
            }
        }
    }
}

impl Error for CompatibilityContextError {}

fn require_nonempty(field: &'static str, value: &str) -> Result<(), CompatibilityContextError> {
    if value.is_empty() {
        Err(CompatibilityContextError::EmptySelector(field))
    } else {
        Ok(())
    }
}

fn validate_session_defaults(defaults: &[String]) -> Result<(), CompatibilityContextError> {
    if defaults.is_empty() {
        return Err(CompatibilityContextError::MissingSessionDefaults);
    }

    let mut unique = BTreeSet::new();
    for default in defaults {
        if default.is_empty() {
            return Err(CompatibilityContextError::EmptySessionDefault);
        }
        if !unique.insert(default.as_str()) {
            return Err(CompatibilityContextError::DuplicateSessionDefault(
                default.clone(),
            ));
        }
    }
    Ok(())
}

fn is_contract_identifier(value: &str) -> bool {
    value.len() <= 128
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
}

fn has_four_numeric_components(value: &str) -> bool {
    let mut components = value.split('.');
    let valid = components.by_ref().take(4).all(|component| {
        !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
    });
    valid && components.next().is_none() && value.bytes().filter(|byte| *byte == b'.').count() == 3
}
