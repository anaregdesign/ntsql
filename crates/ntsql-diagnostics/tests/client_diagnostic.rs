use ntsql_diagnostics::{ClientDiagnostic, DiagnosticNumber, DiagnosticSeverity, DiagnosticState};

#[test]
fn preserves_every_client_visible_field() {
    let diagnostic = ClientDiagnostic::new(
        DiagnosticNumber::new(u32::MAX),
        DiagnosticSeverity::new(u8::MAX),
        DiagnosticState::new(u8::MAX - 1),
        "  exact message\r\n",
    );

    assert_eq!(diagnostic.number().get(), u32::MAX);
    assert_eq!(diagnostic.severity().get(), u8::MAX);
    assert_eq!(diagnostic.state().get(), u8::MAX - 1);
    assert_eq!(diagnostic.message(), "  exact message\r\n");
}

#[test]
fn preserves_empty_and_whitespace_only_messages() {
    for message in ["", " \t\r\n"] {
        let diagnostic = ClientDiagnostic::new(
            DiagnosticNumber::new(0),
            DiagnosticSeverity::new(0),
            DiagnosticState::new(0),
            message,
        );

        assert_eq!(diagnostic.message(), message);
    }
}
