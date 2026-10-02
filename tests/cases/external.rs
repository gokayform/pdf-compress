/// Explicit external test: absent tools are errors rather than skipped successes.
#[test]
#[ignore = "requires installed Ghostscript, Poppler, and qpdf; run with --ignored"]
fn strict_qpdf_ghostscript_and_poppler_lossless_reference() {
    let output = std::env::temp_dir().join(format!(
        "pdf-compress-reference-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let result = crate::support::reference::run(
        [
            std::ffi::OsString::from("--output"),
            output.clone().into_os_string(),
        ]
        .into_iter(),
    );
    assert!(
        result.is_ok(),
        "{result:?}\nArtifacts: {}",
        output.display()
    );
    std::fs::remove_dir_all(output).unwrap();
}
