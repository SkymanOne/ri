//! Copying text to the system clipboard, and reading what the paste key
//! inserts.
//!
//! Port of `copyToClipboard` in `packages/coding-agent/src/utils/clipboard.ts`
//! and of the reads of `handleClipboardPaste` in pi `v1.0.0`, without pi's
//! native clipboard addon: platform commands first, then OSC 52 for remote
//! and headless sessions. On macOS a script reads the pasteboard as the
//! addon does.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
/// pi's limits for listing the clipboard's types and for reading an image.
const LIST_TIMEOUT: Duration = Duration::from_secs(1);
const IMAGE_TIMEOUT: Duration = Duration::from_secs(3);
/// The image types pasted as they are, in pi's order of preference, with
/// their file extensions.
const IMAGE_TYPES: [(&str, &str); 4] = [
    ("image/png", "png"),
    ("image/jpeg", "jpg"),
    ("image/webp", "webp"),
    ("image/gif", "gif"),
];
/// Reads the pasteboard named by its argument, else the general one, as
/// pi's native addon does: the paths of copied files, else an image as PNG,
/// else text.
const MACOS_READER: &str = r#"ObjC.import("AppKit");
function run(argv) {
	const board = argv.length ? $.NSPasteboard.pasteboardWithName(argv[0]) : $.NSPasteboard.generalPasteboard;
	const urls = board.readObjectsForClassesOptions($([$.NSURL]), $({ NSPasteboardURLReadingFileURLsOnlyKey: true }));
	const files = urls.isNil() ? [] : urls.js.map((url) => url.path.js);
	if (files.length) return JSON.stringify({ files });
	if (!board.availableTypeFromArray($([$.NSPasteboardTypePNG, $.NSPasteboardTypeTIFF])).isNil()) {
		let png = board.dataForType($.NSPasteboardTypePNG);
		if (png.isNil()) {
			const tiff = $.NSImage.alloc.initWithPasteboard(board).TIFFRepresentation;
			const bitmap = tiff.isNil() ? tiff : $.NSBitmapImageRep.imageRepWithData(tiff);
			png = bitmap.isNil() ? bitmap : bitmap.representationUsingTypeProperties($.NSBitmapImageFileTypePNG, $());
		}
		if (png.isNil()) return JSON.stringify({ error: "Clipboard does not contain an image" });
		return JSON.stringify({ png: png.base64EncodedStringWithOptions(0).js });
	}
	const text = board.stringForType($.NSPasteboardTypeString);
	return JSON.stringify(text.isNil() ? null : { text: text.js });
}"#;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn is_wsl() -> bool {
    env("WSL_DISTRO_NAME").is_some()
        || std::fs::read_to_string("/proc/version")
            .is_ok_and(|version| version.to_lowercase().contains("microsoft"))
}

/// pi's `runClipboardCommand`: runs `command` with `input` on stdin, and
/// its output when it exits successfully within `timeout`. A command given
/// input gets no output pipe, because clipboard writers can daemonize.
async fn run(
    command: &str,
    args: &[&str],
    input: Option<&str>,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let mut child = tokio::process::Command::new(command)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(if input.is_some() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let stdin = child.stdin.take();
    let output = tokio::time::timeout(timeout, async {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(input.unwrap_or_default().as_bytes()).await;
        }
        child.wait_with_output().await
    })
    .await;
    match output {
        Ok(Ok(output)) if output.status.success() => Some(output.stdout),
        _ => None,
    }
}

/// The OSC 52 sequence that sets the clipboard, or `None` when too long.
fn osc52(text: &str) -> Option<String> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    (encoded.len() <= MAX_OSC52_ENCODED_LENGTH).then(|| format!("\x1b]52;c;{encoded}\x07"))
}

/// Copies `text`. Terminal output (OSC 52) goes through `emit`.
pub async fn copy(text: &str, emit: impl Fn(&str)) -> Result<(), String> {
    let linux = cfg!(target_os = "linux");
    let mut commands: Vec<(&str, Vec<&str>)> = Vec::new();
    if cfg!(target_os = "macos") {
        commands.push(("pbcopy", vec![]));
    } else if cfg!(windows) {
        commands.push(("clip", vec![]));
    } else {
        if env("TERMUX_VERSION").is_some() {
            commands.push(("termux-clipboard-set", vec![]));
        }
        if env("WAYLAND_DISPLAY").is_some() {
            commands.push(("wl-copy", vec![]));
        }
        if env("DISPLAY").is_some() {
            commands.push(("xclip", vec!["-selection", "clipboard"]));
            commands.push(("xsel", vec!["--clipboard", "--input"]));
        }
    }
    let mut copied = false;
    for (command, args) in &commands {
        if run(command, args, Some(text), COMMAND_TIMEOUT)
            .await
            .is_some()
        {
            copied = true;
            break;
        }
    }
    let mut emitted = false;
    if !copied
        && linux
        && is_wsl()
        && env("WT_SESSION").is_some()
        && let Some(sequence) = osc52(text)
    {
        emit(&sequence);
        emitted = true;
        copied = true;
    }
    let remote = ["SSH_CONNECTION", "SSH_CLIENT", "MOSH_CONNECTION"]
        .iter()
        .any(|name| env(name).is_some());
    let headless = linux
        && env("DISPLAY").is_none()
        && env("WAYLAND_DISPLAY").is_none()
        && env("TERMUX_VERSION").is_none();
    let mut oversized = false;
    if !emitted && (remote || (!copied && headless)) {
        match osc52(text) {
            Some(sequence) => {
                emit(&sequence);
                copied = true;
            }
            None => oversized = true,
        }
    }
    if copied {
        return Ok(());
    }
    if oversized {
        return Err("Clipboard unavailable: text exceeds the OSC 52 size limit".into());
    }
    if linux {
        if env("TERMUX_VERSION").is_some() {
            return Err(
                "Clipboard unavailable: install the Termux:API app and `termux-api` package".into(),
            );
        }
        if env("WAYLAND_DISPLAY").is_some() {
            return Err(
                "Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access"
                    .into(),
            );
        }
        if env("DISPLAY").is_some() {
            return Err(
                "Clipboard unavailable: install `xclip` or `xsel`, or check X11 access".into(),
            );
        }
    }
    Err("Clipboard unavailable".into())
}

/// What the paste key inserts.
#[derive(Debug, PartialEq)]
pub enum Paste {
    /// The paths of copied files.
    Files(Vec<String>),
    /// The clipboard's text, or the path of its image saved to a file.
    Text(String),
}

/// pi's reads in `handleClipboardPaste`: the paths of copied files, else
/// the clipboard's image saved to a temporary file, else its text. The
/// error is the reason pi reports.
pub async fn paste() -> Result<Option<Paste>, String> {
    let paste = if cfg!(target_os = "macos") {
        paste_macos().await?
    } else if cfg!(target_os = "linux") {
        paste_linux().await?
    } else {
        None
    };
    match paste {
        Some(Paste::Files(paths)) if paths.iter().any(|path| path.contains(char::is_control)) => {
            Err("Clipboard file path contains control characters".into())
        }
        Some(Paste::Text(text)) if text.is_empty() => Ok(None),
        paste => Ok(paste),
    }
}

/// What [`MACOS_READER`] found.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Pasteboard {
    Files(Vec<String>),
    /// Base64.
    Png(String),
    Text(String),
    Error(String),
}

/// What the pasteboard named in `name`, else the general one, holds.
async fn read_macos(name: &[&str]) -> Option<Pasteboard> {
    let args = [&["-l", "JavaScript", "-e", MACOS_READER], name].concat();
    let output = run("osascript", &args, None, COMMAND_TIMEOUT).await?;
    serde_json::from_slice(&output).ok().flatten()
}

async fn paste_macos() -> Result<Option<Paste>, String> {
    Ok(match read_macos(&[]).await {
        None => None,
        Some(Pasteboard::Files(paths)) => Some(Paste::Files(paths)),
        Some(Pasteboard::Png(data)) => {
            let bytes = STANDARD.decode(data).map_err(|error| error.to_string())?;
            Some(Paste::Text(save_image(&bytes, "png").await?))
        }
        Some(Pasteboard::Text(text)) => Some(Paste::Text(text)),
        Some(Pasteboard::Error(error)) => return Err(error),
    })
}

/// pi's Linux reads: an image through wl-paste on Wayland and WSL, or
/// through xclip when wl-paste fails, else text.
async fn paste_linux() -> Result<Option<Paste>, String> {
    if env("TERMUX_VERSION").is_none()
        && let Some((bytes, extension)) = image_linux().await
    {
        return Ok(Some(Paste::Text(save_image(&bytes, extension).await?)));
    }
    Ok(text_linux().await.map(Paste::Text))
}

/// pi's `readClipboardImage` on Linux, without its native fallback.
async fn image_linux() -> Option<(Vec<u8>, &'static str)> {
    let wayland = env("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind == "wayland");
    let mut image = None;
    if wayland || is_wsl() {
        let read = [&["--type"][..], &["--no-newline"]];
        image = image_via("wl-paste", &["--list-types"], read).await;
    }
    if image.is_none() {
        let list = ["-selection", "clipboard", "-t", "TARGETS", "-o"];
        let read = [&["-selection", "clipboard", "-t"][..], &["-o"]];
        image = image_via("xclip", &list, read).await;
    }
    let (bytes, mime) = image.flatten()?;
    pasteable(bytes, &mime)
}

/// pi's `readClipboardImageVia{WlPaste,Xclip}`: `None` when `command`
/// fails, `Some(None)` when the clipboard has no image. `list` lists the
/// clipboard's types, and `read` reads the type named between its parts.
async fn image_via(
    command: &str,
    list: &[&str],
    read: [&[&str]; 2],
) -> Option<Option<(Vec<u8>, String)>> {
    let types = run(command, list, None, LIST_TIMEOUT).await?;
    let types = String::from_utf8_lossy(&types);
    let Some(mime) = image_type(&types) else {
        return Some(None);
    };
    let bytes = run(
        command,
        &[read[0], &[mime], read[1]].concat(),
        None,
        IMAGE_TIMEOUT,
    )
    .await?;
    Some((!bytes.is_empty()).then(|| (bytes, base_type(mime))))
}

/// pi's `baseMimeType`.
fn base_type(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_lowercase()
}

/// pi's `selectPreferredImageMimeType`: of `types`, one per line, the first
/// of [`IMAGE_TYPES`], else any image type.
fn image_type(types: &str) -> Option<&str> {
    let types: Vec<&str> = types
        .lines()
        .map(str::trim)
        .filter(|kind| !kind.is_empty())
        .collect();
    IMAGE_TYPES
        .iter()
        .find_map(|(preferred, _)| types.iter().find(|kind| base_type(kind) == *preferred))
        .or_else(|| {
            types
                .iter()
                .find(|kind| base_type(kind).starts_with("image/"))
        })
        .copied()
}

/// An image of type `mime` as pi pastes it, with its file extension: as it
/// is when of one of [`IMAGE_TYPES`], else converted to PNG. `None` when
/// the conversion fails.
fn pasteable(bytes: Vec<u8>, mime: &str) -> Option<(Vec<u8>, &'static str)> {
    let extension = |mime: &str| {
        IMAGE_TYPES
            .iter()
            .find(|(kind, _)| *kind == mime)
            .map(|(_, extension)| *extension)
    };
    if let Some(extension) = extension(mime) {
        return Some((bytes, extension));
    }
    let image = yapi_core::images::process(&bytes, mime, false, None)
        .ok()?
        .image;
    Some((
        STANDARD.decode(image.data).ok()?,
        extension(&image.mime_type)?,
    ))
}

/// pi's `readClipboardText` on Linux, without its native fallback: the
/// output of the first clipboard tool that runs.
async fn text_linux() -> Option<String> {
    let mut commands: Vec<(&str, &[&str])> = Vec::new();
    if env("TERMUX_VERSION").is_some() {
        commands.push(("termux-clipboard-get", &[]));
    }
    if env("WAYLAND_DISPLAY").is_some() {
        commands.push(("wl-paste", &["--no-newline", "--type", "text"]));
    }
    if env("DISPLAY").is_some() {
        commands.push(("xclip", &["-selection", "clipboard", "-out"]));
        commands.push(("xsel", &["--clipboard", "--output"]));
    }
    for (command, args) in commands {
        if let Some(bytes) = run(command, args, None, COMMAND_TIMEOUT).await {
            return Some(String::from_utf8_lossy(&bytes).into_owned());
        }
    }
    None
}

/// Saves a pasted image where pi does, as `yapi-clipboard-<uuid>.<extension>`
/// in the temporary directory, and returns its path.
async fn save_image(bytes: &[u8], extension: &str) -> Result<String, String> {
    let name = format!("yapi-clipboard-{}.{extension}", yapi_core::time::uuid_v4());
    let path = std::env::temp_dir().join(name);
    tokio::fs::write(&path, bytes)
        .await
        .map_err(|error| error.to_string())?;
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn commands_run_without_blocking_the_runtime() {
        let started = std::time::Instant::now();
        let tick = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            started.elapsed()
        });
        let script = ["-c", "cat >/dev/null; sleep 0.5"];
        assert!(
            run("sh", &script, Some("text"), COMMAND_TIMEOUT)
                .await
                .is_some()
        );
        assert!(tick.await.unwrap() < Duration::from_millis(400));
        let echo = run("sh", &["-c", "cat"], None, COMMAND_TIMEOUT).await;
        assert_eq!(echo.as_deref(), Some(&b""[..]));
        let output = run("sh", &["-c", "printf out"], None, COMMAND_TIMEOUT).await;
        assert_eq!(output.as_deref(), Some(&b"out"[..]));
        assert!(
            run("sh", &["-c", "exit 1"], None, COMMAND_TIMEOUT)
                .await
                .is_none()
        );
        let slow = run("sleep", &["5"], None, Duration::from_millis(50)).await;
        assert!(slow.is_none());
        assert!(
            run("yapi-test-missing-command", &[], None, COMMAND_TIMEOUT)
                .await
                .is_none()
        );
    }

    #[test]
    fn image_types_are_chosen_as_in_pi() {
        let types = "TARGETS\r\ntext/plain\nimage/bmp\n image/JPEG \nimage/png;q=1\n";
        assert_eq!(image_type(types), Some("image/png;q=1"));
        assert_eq!(image_type("image/gif\nimage/webp"), Some("image/webp"));
        assert_eq!(image_type("image/JPEG\nimage/gif"), Some("image/JPEG"));
        assert_eq!(image_type("text/plain\nimage/bmp"), Some("image/bmp"));
        assert_eq!(image_type("text/plain\nUTF8_STRING\n"), None);
    }

    #[tokio::test]
    async fn images_are_read_with_the_type_listed() {
        let list = ["-c", "printf 'text/plain\\nimage/png\\n'"];
        // `sh -c` names the type `$0`.
        let read = [&["-c", r#"test "$0" = image/png && printf png"#][..], &[]];
        let image = image_via("sh", &list, read).await;
        assert_eq!(image, Some(Some((b"png".to_vec(), "image/png".into()))));
        // An empty read is no image, and a failed one a failed tool.
        let empty = [&["-c", "true"][..], &[]];
        assert_eq!(image_via("sh", &list, empty).await, Some(None));
        assert_eq!(image_via("sh", &list, [&["-c", "exit 1"], &[]]).await, None);
        let text = ["-c", "echo text/plain"];
        assert_eq!(image_via("sh", &text, read).await, Some(None));
        assert_eq!(image_via("sh", &["-c", "exit 1"], read).await, None);
    }

    #[test]
    fn other_image_types_become_png() {
        assert_eq!(
            pasteable(b"jpeg".to_vec(), "image/jpeg"),
            Some((b"jpeg".to_vec(), "jpg"))
        );
        // pi's 1×1 red BMP.
        let mut bmp = b"BM".to_vec();
        for value in [58u32, 0, 54, 40, 1, 1] {
            bmp.extend(value.to_le_bytes());
        }
        bmp.extend([1, 0, 24, 0]);
        bmp.extend(
            [0u32, 4, 0, 0, 0, 0]
                .iter()
                .flat_map(|value| value.to_le_bytes()),
        );
        bmp.extend([0, 0, 0xff, 0]);
        let (png, extension) = pasteable(bmp, "image/bmp").unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        assert_eq!(extension, "png");
        assert_eq!(pasteable(b"not tiff".to_vec(), "image/tiff"), None);
    }

    /// The reader on a pasteboard of its own, filled by a script.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn the_macos_reader_finds_files_images_and_text() {
        const WRITER: &str = r#"ObjC.import("AppKit");
function run([name, kind, value]) {
	const board = $.NSPasteboard.pasteboardWithName(name);
	board.clearContents;
	if (kind === "release") board.releaseGlobally;
	else if (kind === "files") board.writeObjects($(value.split("\n").map((path) => $.NSURL.fileURLWithPath(path))));
	else if (kind === "text") board.setStringForType(value, $.NSPasteboardTypeString);
	else {
		const png = $.NSData.alloc.initWithBase64EncodedStringOptions(value, 0);
		const data = kind === "png" ? png : $.NSBitmapImageRep.imageRepWithData(png).TIFFRepresentation;
		board.setDataForType(data, kind === "png" ? $.NSPasteboardTypePNG : $.NSPasteboardTypeTIFF);
	}
}"#;
        const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";
        let name = format!("yapi-test-{}", std::process::id());
        let write = |kind: &str, value: &str| {
            let args = ["-l", "JavaScript", "-e", WRITER, &name, kind, value];
            let status = std::process::Command::new("osascript").args(args).status();
            assert!(status.unwrap().success());
        };
        let read = || async { read_macos(&[&name]).await };
        write("text", "");
        assert_eq!(read().await, Some(Pasteboard::Text(String::new())));
        write("files", "/etc/hosts\n/tmp/My Photos");
        let files = vec!["/etc/hosts".to_owned(), "/tmp/My Photos".to_owned()];
        assert_eq!(read().await, Some(Pasteboard::Files(files)));
        write("png", PNG);
        assert_eq!(read().await, Some(Pasteboard::Png(PNG.into())));
        write("tiff", PNG);
        let Some(Pasteboard::Png(png)) = read().await else {
            panic!("TIFF is read as PNG");
        };
        assert!(STANDARD.decode(png).unwrap().starts_with(b"\x89PNG"));
        write("text", "héllo\nworld");
        assert_eq!(read().await, Some(Pasteboard::Text("héllo\nworld".into())));
        write("release", "");
        assert_eq!(read().await, None);
    }
}
