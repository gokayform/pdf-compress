//! Command line interface for the pure-Rust PDF compressor.
//!
//! The binary intentionally has no argument-parser dependency.  Keeping the
//! small parser here makes the executable usable in the same minimal builds as
//! the library and lets us validate every value before doing any I/O.

#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing
    )
)]

use pdf_compress::{compress, Options, Preset};
use std::error::Error as StdError;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

const HELP: &str = "\
Pure-Rust PDF compressor\n\
\n\
Usage:\n\
  pdf-compress [OPTIONS] <INPUT> <OUTPUT>\n\
\n\
The default preset is lossless.  OUTPUT is never replaced unless --force is\n\
given.\n\
\n\
Options:\n\
  -p, --preset <PRESET>                 lossless, screen, ebook, printer, or prepress\n\
      --jpeg-quality <1..100>           JPEG quality for a lossy preset\n\
      --dpi <DPI>                       Target image resolution for a lossy preset\n\
      --max-input-bytes <BYTES>         Reject input larger than this limit\n\
      --max-decoded-stream-bytes <BYTES>\n\
                                         Reject streams exceeding this decoded-size limit\n\
      --keep-if-larger                  Keep compressed bytes even when they grow\n\
      --force                           Replace an existing OUTPUT atomically\n\
  -h, --help                            Show this help\n\
  -V, --version                         Show the version\n\
\
Examples:\n\
  pdf-compress input.pdf output.pdf\n\
  pdf-compress --preset ebook --jpeg-quality 72 --dpi 150 input.pdf ebook.pdf\n\
  pdf-compress --preset printer --max-input-bytes 104857600 input.pdf output.pdf\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresetName {
    Lossless,
    Screen,
    Ebook,
    Printer,
    Prepress,
}

impl PresetName {
    fn parse(value: &str) -> Result<Self, String> {
        match value.to_ascii_lowercase().as_str() {
            "lossless" => Ok(Self::Lossless),
            "screen" => Ok(Self::Screen),
            "ebook" => Ok(Self::Ebook),
            "printer" => Ok(Self::Printer),
            "prepress" => Ok(Self::Prepress),
            _ => Err(format!(
                "invalid preset {value:?}; expected lossless, screen, ebook, printer, or prepress"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Lossless => "lossless",
            Self::Screen => "screen",
            Self::Ebook => "ebook",
            Self::Printer => "printer",
            Self::Prepress => "prepress",
        }
    }

    fn is_lossless(self) -> bool {
        matches!(self, Self::Lossless)
    }

    fn into_library(self) -> Preset {
        match self {
            Self::Lossless => Preset::Lossless,
            Self::Screen => Preset::Screen,
            Self::Ebook => Preset::Ebook,
            Self::Printer => Preset::Printer,
            Self::Prepress => Preset::Prepress,
        }
    }
}

#[derive(Debug)]
struct Command {
    input: PathBuf,
    output: PathBuf,
    preset: PresetName,
    jpeg_quality: Option<u8>,
    target_dpi: Option<u32>,
    max_input_bytes: Option<usize>,
    max_decoded_stream_bytes: Option<usize>,
    keep_if_larger: bool,
    force: bool,
}

#[derive(Debug)]
enum ParseOutcome {
    Help,
    Version,
    Run(Command),
}

#[derive(Debug)]
enum CliError {
    Message(String),
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Compression(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Message(message) => formatter.write_str(message),
            Self::Io {
                action,
                path,
                source,
            } => write!(formatter, "could not {action} {}: {source}", path.display()),
            Self::Compression(message) => write!(formatter, "compression failed: {message}"),
        }
    }
}

impl StdError for CliError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Message(_) | Self::Compression(_) => None,
        }
    }
}

// `print!`/`eprintln!` panic when the stream is closed (for example a broken
// pipe); the CLI must exit with its own status instead.
macro_rules! out {
    ($($argument:tt)*) => {{
        let _ = writeln!(io::stdout(), $($argument)*);
    }};
}

macro_rules! err {
    ($($argument:tt)*) => {{
        let _ = writeln!(io::stderr(), $($argument)*);
    }};
}

fn main() {
    match parse_args(std::env::args_os()) {
        Ok(ParseOutcome::Help) => {
            let _ = io::stdout().write_all(HELP.as_bytes());
        }
        Ok(ParseOutcome::Version) => {
            out!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        }
        Ok(ParseOutcome::Run(command)) => {
            if let Err(error) = execute(command) {
                err!("error: {error}");
                process::exit(1);
            }
        }
        Err(error) => {
            err!("error: {error}\n\n{HELP}");
            process::exit(2);
        }
    }
}

fn parse_args<I>(args: I) -> Result<ParseOutcome, String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut arguments = args.into_iter();
    let _program = arguments.next();
    let mut positional = Vec::new();
    let mut options_done = false;
    let mut preset = PresetName::Lossless;
    let mut preset_seen = false;
    let mut jpeg_quality = None;
    let mut jpeg_quality_seen = false;
    let mut target_dpi = None;
    let mut target_dpi_seen = false;
    let mut max_input_bytes = None;
    let mut max_input_bytes_seen = false;
    let mut max_decoded_stream_bytes = None;
    let mut max_decoded_stream_bytes_seen = false;
    let mut keep_if_larger = false;
    let mut keep_if_larger_seen = false;
    let mut force = false;
    let mut force_seen = false;

    while let Some(argument) = arguments.next() {
        if !options_done && argument == OsStr::new("--") {
            options_done = true;
            continue;
        }

        let text = argument.to_str();
        if !options_done {
            if let Some("--help" | "-h") = text {
                return Ok(ParseOutcome::Help);
            }
            if let Some("--version" | "-V") = text {
                return Ok(ParseOutcome::Version);
            }
        }

        if !options_done {
            if let Some(value) = text.and_then(|value| value.strip_prefix("--preset=")) {
                if preset_seen {
                    return Err("--preset may only be supplied once".into());
                }
                preset = PresetName::parse(value)?;
                preset_seen = true;
                continue;
            }
            if let Some(value) = text.and_then(|value| value.strip_prefix("--jpeg-quality=")) {
                if jpeg_quality_seen {
                    return Err("--jpeg-quality may only be supplied once".into());
                }
                jpeg_quality = Some(parse_quality(value)?);
                jpeg_quality_seen = true;
                continue;
            }
            if let Some(value) = text.and_then(|value| value.strip_prefix("--dpi=")) {
                if target_dpi_seen {
                    return Err("--dpi may only be supplied once".into());
                }
                target_dpi = Some(parse_dpi(value)?);
                target_dpi_seen = true;
                continue;
            }
            if let Some(value) = text.and_then(|value| value.strip_prefix("--max-input-bytes=")) {
                if max_input_bytes_seen {
                    return Err("--max-input-bytes may only be supplied once".into());
                }
                max_input_bytes = Some(parse_byte_limit(value, "--max-input-bytes")?);
                max_input_bytes_seen = true;
                continue;
            }
            if let Some(value) =
                text.and_then(|value| value.strip_prefix("--max-decoded-stream-bytes="))
            {
                if max_decoded_stream_bytes_seen {
                    return Err("--max-decoded-stream-bytes may only be supplied once".into());
                }
                max_decoded_stream_bytes =
                    Some(parse_byte_limit(value, "--max-decoded-stream-bytes")?);
                max_decoded_stream_bytes_seen = true;
                continue;
            }

            match text {
                Some("--preset") | Some("-p") => {
                    if preset_seen {
                        return Err("--preset may only be supplied once".into());
                    }
                    let value = next_value(&mut arguments, "--preset")?;
                    preset = PresetName::parse(&value)?;
                    preset_seen = true;
                    continue;
                }
                Some("--jpeg-quality") => {
                    if jpeg_quality_seen {
                        return Err("--jpeg-quality may only be supplied once".into());
                    }
                    let value = next_value(&mut arguments, "--jpeg-quality")?;
                    jpeg_quality = Some(parse_quality(&value)?);
                    jpeg_quality_seen = true;
                    continue;
                }
                Some("--dpi") => {
                    if target_dpi_seen {
                        return Err("--dpi may only be supplied once".into());
                    }
                    let value = next_value(&mut arguments, "--dpi")?;
                    target_dpi = Some(parse_dpi(&value)?);
                    target_dpi_seen = true;
                    continue;
                }
                Some("--max-input-bytes") => {
                    if max_input_bytes_seen {
                        return Err("--max-input-bytes may only be supplied once".into());
                    }
                    let value = next_value(&mut arguments, "--max-input-bytes")?;
                    max_input_bytes = Some(parse_byte_limit(&value, "--max-input-bytes")?);
                    max_input_bytes_seen = true;
                    continue;
                }
                Some("--max-decoded-stream-bytes") => {
                    if max_decoded_stream_bytes_seen {
                        return Err("--max-decoded-stream-bytes may only be supplied once".into());
                    }
                    let value = next_value(&mut arguments, "--max-decoded-stream-bytes")?;
                    max_decoded_stream_bytes =
                        Some(parse_byte_limit(&value, "--max-decoded-stream-bytes")?);
                    max_decoded_stream_bytes_seen = true;
                    continue;
                }
                Some("--keep-if-larger") => {
                    if keep_if_larger_seen {
                        return Err("--keep-if-larger may only be supplied once".into());
                    }
                    keep_if_larger = true;
                    keep_if_larger_seen = true;
                    continue;
                }
                Some("--force") => {
                    if force_seen {
                        return Err("--force may only be supplied once".into());
                    }
                    force = true;
                    force_seen = true;
                    continue;
                }
                Some(value) if value.starts_with('-') => {
                    return Err(format!("unknown option {value:?}"));
                }
                _ => {}
            }
        }

        positional.push(PathBuf::from(argument));
    }

    if positional.len() != 2 {
        return Err(format!(
            "expected exactly two positional paths (INPUT and OUTPUT), got {}",
            positional.len()
        ));
    }

    let input = positional.remove(0);
    let output = positional.remove(0);
    if input.as_os_str().is_empty() || output.as_os_str().is_empty() {
        return Err("INPUT and OUTPUT must not be empty paths".into());
    }

    Ok(ParseOutcome::Run(Command {
        input,
        output,
        preset,
        jpeg_quality,
        target_dpi,
        max_input_bytes,
        max_decoded_stream_bytes,
        keep_if_larger,
        force,
    }))
}

fn next_value<I>(arguments: &mut I, option: &str) -> Result<String, String>
where
    I: Iterator<Item = OsString>,
{
    let value = arguments
        .next()
        .ok_or_else(|| format!("missing value for {option}"))?;
    value
        .into_string()
        .map_err(|_| format!("value for {option} must be valid UTF-8"))
}

fn parse_quality(value: &str) -> Result<u8, String> {
    let quality = value.parse::<u8>().map_err(|_| {
        format!("invalid JPEG quality {value:?}; expected an integer from 1 to 100")
    })?;
    if !(1..=100).contains(&quality) {
        return Err(format!(
            "invalid JPEG quality {quality}; expected an integer from 1 to 100"
        ));
    }
    Ok(quality)
}

fn parse_dpi(value: &str) -> Result<u32, String> {
    let dpi = value
        .parse::<u32>()
        .map_err(|_| format!("invalid DPI {value:?}; expected a positive integer"))?;
    if dpi == 0 {
        return Err("DPI must be greater than zero".into());
    }
    Ok(dpi)
}

fn parse_byte_limit(value: &str, option: &str) -> Result<usize, String> {
    let bytes = value.parse::<u64>().map_err(|_| {
        format!("invalid value for {option}: {value:?}; expected a positive byte count")
    })?;
    if bytes == 0 {
        return Err(format!("{option} must be greater than zero"));
    }
    usize::try_from(bytes).map_err(|_| format!("{option} is too large for this platform"))
}

fn execute(command: Command) -> Result<(), CliError> {
    let options = build_options(&command).map_err(CliError::Message)?;

    if !command.force {
        ensure_destination_is_new(&command.output)?;
        if paths_refer_to_same_file(&command.input, &command.output) {
            return Err(CliError::Message(
                "INPUT and OUTPUT refer to the same file; use --force for in-place replacement"
                    .into(),
            ));
        }
    }

    let input_metadata = fs::metadata(&command.input)
        .map_err(|source| io_error("read metadata for", &command.input, source))?;
    if !input_metadata.is_file() {
        return Err(CliError::Message(format!(
            "INPUT {} is not a regular file",
            command.input.display()
        )));
    }
    if input_metadata.len() > options.max_input_bytes as u64 {
        return Err(CliError::Message(format!(
            "INPUT {} is {} bytes, exceeding --max-input-bytes ({})",
            command.input.display(),
            input_metadata.len(),
            options.max_input_bytes
        )));
    }

    let input =
        fs::read(&command.input).map_err(|source| io_error("read", &command.input, source))?;
    if input.len() > options.max_input_bytes {
        return Err(CliError::Message(format!(
            "INPUT grew beyond --max-input-bytes while it was being read ({})",
            options.max_input_bytes
        )));
    }

    let result =
        compress(&input, &options).map_err(|error| CliError::Compression(error.to_string()))?;
    let output_size = result.bytes.len();
    let warnings = result.report.warnings;
    write_output(&command.output, &result.bytes, command.force)?;

    for warning in warnings {
        err!("warning: {warning}");
    }
    if result.report.compatibility_repairs > 0 {
        err!(
            "retained {} lossless compatibility repair(s)",
            result.report.compatibility_repairs
        );
    }
    err!(
        "wrote {} ({} → {} bytes; preset {})",
        command.output.display(),
        input.len(),
        output_size,
        command.preset.as_str()
    );
    Ok(())
}

fn build_options(command: &Command) -> Result<Options, String> {
    if command.preset.is_lossless()
        && (command.jpeg_quality.is_some() || command.target_dpi.is_some())
    {
        return Err(
            "--jpeg-quality and --dpi require a lossy preset (screen, ebook, printer, or prepress)"
                .into(),
        );
    }

    let mut options = Options::for_preset(command.preset.into_library());
    if let Some(quality) = command.jpeg_quality {
        options.jpeg_quality = quality;
    }
    if let Some(dpi) = command.target_dpi {
        options.target_dpi = Some(dpi);
    }
    if let Some(limit) = command.max_input_bytes {
        options.max_input_bytes = limit;
    }
    if let Some(limit) = command.max_decoded_stream_bytes {
        options.max_decoded_stream_bytes = limit;
    }
    if command.keep_if_larger {
        options.keep_if_larger = true;
    }
    Ok(options)
}

fn ensure_destination_is_new(path: &Path) -> Result<(), CliError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(CliError::Message(format!(
            "OUTPUT {} already exists; pass --force to replace it",
            path.display()
        ))),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(io_error("inspect OUTPUT", path, source)),
    }
}

fn paths_refer_to_same_file(input: &Path, output: &Path) -> bool {
    if input == output {
        return true;
    }
    match (fs::canonicalize(input), fs::canonicalize(output)) {
        (Ok(input), Ok(output)) => input == output,
        _ => false,
    }
}

fn write_output(path: &Path, bytes: &[u8], force: bool) -> Result<(), CliError> {
    if !force {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .map_err(|source| io_error("create OUTPUT", path, source))?;
        let result = write_and_sync(&mut file, bytes);
        drop(file);
        if let Err(source) = result {
            let _ = fs::remove_file(path);
            return Err(io_error("write OUTPUT", path, source));
        }
        return Ok(());
    }

    // Renaming over a symlink would replace the link rather than the file it
    // points at, so the replacement is staged next to the resolved target.
    let target = replacement_target(path)?;
    let parent = output_parent(&target);
    let (temporary_path, mut temporary_file) = create_temporary_file(parent)
        .map_err(|source| io_error("create temporary OUTPUT", parent, source))?;
    let write_result = write_and_sync(&mut temporary_file, bytes);
    drop(temporary_file);
    if let Err(source) = write_result {
        let _ = fs::remove_file(&temporary_path);
        return Err(io_error("write OUTPUT", path, source));
    }

    if let Ok(existing) = fs::metadata(&target) {
        if let Err(source) = fs::set_permissions(&temporary_path, existing.permissions()) {
            let _ = fs::remove_file(&temporary_path);
            return Err(io_error(
                "copy permissions to temporary OUTPUT",
                path,
                source,
            ));
        }
    }

    if let Err(source) = fs::rename(&temporary_path, &target) {
        let _ = fs::remove_file(&temporary_path);
        return Err(io_error("replace OUTPUT", path, source));
    }
    Ok(())
}

fn replacement_target(path: &Path) -> Result<PathBuf, CliError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            fs::canonicalize(path).map_err(|source| io_error("resolve OUTPUT", path, source))
        }
        Ok(_) => Ok(path.to_path_buf()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(source) => Err(io_error("inspect OUTPUT", path, source)),
    }
}

fn write_and_sync(file: &mut File, bytes: &[u8]) -> io::Result<()> {
    file.write_all(bytes)?;
    file.sync_all()
}

fn output_parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn create_temporary_file(parent: &Path) -> io::Result<(PathBuf, File)> {
    let process_id = process::id();
    for _ in 0..128 {
        let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".pdf-compress-{process_id}-{counter}.tmp"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(source),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not choose a unique temporary path",
    ))
}

fn io_error(action: &'static str, path: &Path, source: io::Error) -> CliError {
    CliError::Io {
        action,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arguments(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn defaults_to_lossless_and_accepts_paths() {
        let parsed = parse_args(arguments(&["pdf-compress", "in.pdf", "out.pdf"]))
            .expect("arguments should parse");
        let ParseOutcome::Run(command) = parsed else {
            panic!("expected a command");
        };
        assert_eq!(command.preset, PresetName::Lossless);
        assert_eq!(command.input, PathBuf::from("in.pdf"));
        assert_eq!(command.output, PathBuf::from("out.pdf"));
        assert!(!command.force);
    }

    #[test]
    fn parses_preset_overrides_and_limits() {
        let parsed = parse_args(arguments(&[
            "pdf-compress",
            "--preset",
            "ebook",
            "--jpeg-quality=73",
            "--dpi",
            "150",
            "--max-input-bytes",
            "100000",
            "--max-decoded-stream-bytes=200000",
            "--keep-if-larger",
            "--force",
            "in.pdf",
            "out.pdf",
        ]))
        .expect("arguments should parse");
        let ParseOutcome::Run(command) = parsed else {
            panic!("expected a command");
        };
        assert_eq!(command.preset, PresetName::Ebook);
        assert_eq!(command.jpeg_quality, Some(73));
        assert_eq!(command.target_dpi, Some(150));
        assert_eq!(command.max_input_bytes, Some(100_000));
        assert_eq!(command.max_decoded_stream_bytes, Some(200_000));
        assert!(command.keep_if_larger);
        assert!(command.force);
    }

    #[test]
    fn rejects_invalid_ranges_and_unknown_options() {
        for (args, expected) in [
            (
                &["pdf-compress", "--jpeg-quality", "0", "a", "b"][..],
                "JPEG quality",
            ),
            (&["pdf-compress", "--dpi", "0", "a", "b"][..], "DPI"),
            (
                &["pdf-compress", "--max-input-bytes", "0", "a", "b"][..],
                "--max-input-bytes",
            ),
            (&["pdf-compress", "--wat", "a", "b"][..], "unknown option"),
        ] {
            let error = parse_args(arguments(args)).expect_err("arguments should fail");
            assert!(error.contains(expected), "{error:?}");
        }
    }

    #[test]
    fn does_not_overwrite_existing_output_without_force() {
        let root = std::env::temp_dir().join(format!(
            "pdf-compress-cli-test-{}-{}",
            process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).expect("create test directory");
        let output = root.join("output.pdf");
        fs::write(&output, b"old").expect("create existing output");

        let error = write_output(&output, b"new", false).expect_err("existing output must fail");
        assert!(error.to_string().contains("create OUTPUT"));
        assert_eq!(fs::read(&output).expect("read existing output"), b"old");

        fs::remove_file(&output).expect("remove test output");
        fs::remove_dir(&root).expect("remove test directory");
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "pdf-compress-cli-test-{}-{}",
                process::id(),
                TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).expect("create test directory");
            Self(root)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn command(input: PathBuf, output: PathBuf, force: bool) -> Command {
        Command {
            input,
            output,
            preset: PresetName::Lossless,
            jpeg_quality: None,
            target_dpi: None,
            max_input_bytes: None,
            max_decoded_stream_bytes: None,
            keep_if_larger: false,
            force,
        }
    }

    #[test]
    fn compression_errors_use_display_formatting() {
        let dir = TestDir::new();
        let input = dir.path("input.pdf");
        fs::write(&input, b"this is not a pdf").expect("write input");

        let error = execute(command(input, dir.path("output.pdf"), false))
            .expect_err("garbage input must fail");
        let message = error.to_string();
        assert!(message.starts_with("compression failed: "), "{message}");
        assert!(!message.contains("InvalidInput("), "{message}");
        assert!(!message.contains("Pdf("), "{message}");
    }

    #[cfg(unix)]
    #[test]
    fn force_replaces_symlink_target_and_keeps_permissions() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = TestDir::new();
        let target = dir.path("target.pdf");
        let link = dir.path("link.pdf");
        fs::write(&target, b"old").expect("write target");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o640))
            .expect("set target permissions");
        symlink(&target, &link).expect("create symlink");

        write_output(&link, b"new", true).expect("forced write through symlink");

        assert!(fs::symlink_metadata(&link)
            .expect("link metadata")
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).expect("read target"), b"new");
        assert_eq!(fs::read(&link).expect("read through link"), b"new");
        let mode = fs::metadata(&target)
            .expect("target metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
        let leftovers: Vec<_> = fs::read_dir(&dir.0)
            .expect("list directory")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn force_preserves_permissions_of_regular_output() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new();
        let output = dir.path("output.pdf");
        fs::write(&output, b"old").expect("write output");
        fs::set_permissions(&output, fs::Permissions::from_mode(0o600))
            .expect("set output permissions");

        write_output(&output, b"new", true).expect("forced write");

        assert_eq!(fs::read(&output).expect("read output"), b"new");
        let mode = fs::metadata(&output)
            .expect("output metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn force_through_dangling_symlink_fails_without_creating_target() {
        use std::os::unix::fs::symlink;

        let dir = TestDir::new();
        let link = dir.path("link.pdf");
        symlink(dir.path("missing.pdf"), &link).expect("create symlink");

        write_output(&link, b"new", true).expect_err("dangling symlink must fail");

        assert!(!dir.path("missing.pdf").exists());
        assert!(fs::symlink_metadata(&link)
            .expect("link metadata")
            .file_type()
            .is_symlink());
    }

    #[test]
    fn force_supports_in_place_replacement() {
        let dir = TestDir::new();
        let path = dir.path("in-place.pdf");
        fs::write(&path, b"old").expect("write file");

        assert!(paths_refer_to_same_file(&path, &path));
        write_output(&path, b"new", true).expect("in-place write");

        assert_eq!(fs::read(&path).expect("read file"), b"new");
    }
}
