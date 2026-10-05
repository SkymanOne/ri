//! Filesystem calls for the `node:fs` shim, over WASI. The host's preopens
//! decide what is reachable.
//!
//! Every call takes JSON arguments and returns JSON: the result, or
//! `{"error": {"code", "message", "syscall", "path"}}` with Node's codes and
//! messages, which the shim throws as Node errors.

use std::io::Write as _;
use std::path::Path;
use std::time::UNIX_EPOCH;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

fn code(kind: std::io::ErrorKind) -> (&'static str, &'static str) {
    use std::io::ErrorKind::*;
    match kind {
        NotFound => ("ENOENT", "no such file or directory"),
        PermissionDenied => ("EACCES", "permission denied"),
        AlreadyExists => ("EEXIST", "file already exists"),
        NotADirectory => ("ENOTDIR", "not a directory"),
        IsADirectory => ("EISDIR", "illegal operation on a directory"),
        DirectoryNotEmpty => ("ENOTEMPTY", "directory not empty"),
        InvalidInput => ("EINVAL", "invalid argument"),
        ReadOnlyFilesystem => ("EROFS", "read-only file system"),
        _ => ("EIO", "i/o error"),
    }
}

fn error(err: &std::io::Error, syscall: &str, path: &str) -> Value {
    let (code, text) = code(err.kind());
    json!({"error": {
        "code": code,
        "message": format!("{code}: {text}, {syscall} '{path}'"),
        "syscall": syscall,
        "path": path,
    }})
}

fn millis(time: std::io::Result<std::time::SystemTime>) -> f64 {
    time.ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |elapsed| elapsed.as_secs_f64() * 1000.0)
}

/// What a directory entry is: `symlink`, `dir`, `file` or `other`.
fn kind(file_type: std::fs::FileType) -> &'static str {
    if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "dir"
    } else if file_type.is_file() {
        "file"
    } else {
        "other"
    }
}

fn stat(metadata: &std::fs::Metadata) -> Value {
    let kind = kind(metadata.file_type());
    let mode = match kind {
        "dir" => 0o040755,
        "symlink" => 0o120777,
        _ if metadata.permissions().readonly() => 0o100444,
        _ => 0o100644,
    };
    json!({
        "type": kind,
        "size": metadata.len(),
        "mode": mode,
        "mtimeMs": millis(metadata.modified()),
        "atimeMs": millis(metadata.accessed()),
        "ctimeMs": millis(metadata.modified()),
        "birthtimeMs": millis(metadata.created()),
    })
}

/// Node's `realpath` for absolute `path`. wasi-libc resolves every ancestor
/// and fails above a preopened directory, so links are resolved here one
/// component at a time. Ancestors that cannot be read count as directories.
fn realpath(path: &Path) -> std::io::Result<std::path::PathBuf> {
    use std::path::{Component, PathBuf};
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return Ok(resolved);
    }
    let mut pending: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components().rev() {
        if let Component::Normal(name) = component {
            pending.push(name.to_owned());
        } else if component == Component::ParentDir {
            pending.push("..".into());
        }
    }
    let mut resolved = PathBuf::from("/");
    let mut links = 0;
    while let Some(name) = pending.pop() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        let candidate = resolved.join(&name);
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                links += 1;
                if links > 40 {
                    return Err(std::io::Error::other("too many symbolic links"));
                }
                let target = std::fs::read_link(&candidate)?;
                if target.is_absolute() {
                    resolved = PathBuf::from("/");
                }
                for component in target.components().rev() {
                    match component {
                        Component::Normal(name) => pending.push(name.to_owned()),
                        Component::ParentDir => pending.push("..".into()),
                        _ => {}
                    }
                }
            }
            _ => resolved = candidate,
        }
    }
    std::fs::metadata(&resolved)?;
    Ok(resolved)
}

fn text(args: &Value, key: &str) -> String {
    args[key].as_str().unwrap_or_default().to_owned()
}

fn run(op: &str, args: &Value) -> Value {
    let path = text(args, "path");
    let result: Result<Value, (std::io::Error, &str)> = match op {
        "readFile" => std::fs::read(&path)
            .map(|bytes| {
                if args["encoding"].is_null() {
                    json!({"base64": STANDARD.encode(bytes)})
                } else {
                    json!({"text": String::from_utf8_lossy(&bytes)})
                }
            })
            .map_err(|err| (err, "open")),
        "writeFile" => {
            let bytes = match args["base64"].as_str() {
                Some(data) => STANDARD.decode(data).unwrap_or_default(),
                None => text(args, "text").into_bytes(),
            };
            let mut options = std::fs::OpenOptions::new();
            options.create(true);
            if args["append"].as_bool() == Some(true) {
                options.append(true);
            } else {
                options.write(true).truncate(true);
            }
            options
                .open(&path)
                .and_then(|mut file| file.write_all(&bytes))
                .map(|()| Value::Null)
                .map_err(|err| (err, "open"))
        }
        "exists" => Ok(Value::Bool(Path::new(&path).exists())),
        "access" => std::fs::metadata(&path)
            .map(|_| Value::Null)
            .map_err(|err| (err, "access")),
        "stat" => std::fs::metadata(&path)
            .map(|metadata| stat(&metadata))
            .map_err(|err| (err, "stat")),
        "lstat" => std::fs::symlink_metadata(&path)
            .map(|metadata| stat(&metadata))
            .map_err(|err| (err, "lstat")),
        "readdir" => std::fs::read_dir(&path)
            .map(|entries| {
                let mut out: Vec<Value> = entries
                    .flatten()
                    .map(|entry| {
                        let kind = entry.file_type().map_or("other", kind);
                        json!({"name": entry.file_name().to_string_lossy(), "type": kind})
                    })
                    .collect();
                out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
                Value::Array(out)
            })
            .map_err(|err| (err, "scandir")),
        "mkdir" => {
            let result = if args["recursive"].as_bool() == Some(true) {
                std::fs::create_dir_all(&path)
            } else {
                std::fs::create_dir(&path)
            };
            result.map(|()| Value::Null).map_err(|err| (err, "mkdir"))
        }
        "rm" => {
            let force = args["force"].as_bool() == Some(true);
            let recursive = args["recursive"].as_bool() == Some(true);
            let result = match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() && recursive => std::fs::remove_dir_all(&path),
                Ok(metadata) if metadata.is_dir() => std::fs::remove_dir(&path),
                Ok(_) => std::fs::remove_file(&path),
                Err(err) if force && err.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(err) => Err(err),
            };
            result.map(|()| Value::Null).map_err(|err| (err, "rm"))
        }
        "rmdir" => std::fs::remove_dir(&path)
            .map(|()| Value::Null)
            .map_err(|err| (err, "rmdir")),
        "unlink" => std::fs::remove_file(&path)
            .map(|()| Value::Null)
            .map_err(|err| (err, "unlink")),
        "rename" => std::fs::rename(&path, text(args, "to"))
            .map(|()| Value::Null)
            .map_err(|err| (err, "rename")),
        "copyFile" => std::fs::copy(&path, text(args, "to"))
            .map(|_| Value::Null)
            .map_err(|err| (err, "copyfile")),
        "realpath" => realpath(Path::new(&path))
            .map(|resolved| Value::String(resolved.to_string_lossy().into_owned()))
            .map_err(|err| (err, "realpath")),
        "readlink" => std::fs::read_link(&path)
            .map(|target| Value::String(target.to_string_lossy().into_owned()))
            .map_err(|err| (err, "readlink")),
        _ => Ok(
            json!({"error": {"code": "ENOSYS", "message": format!("ENOSYS: function not implemented, {op}")}}),
        ),
    };
    result.unwrap_or_else(|(err, syscall)| error(&err, syscall, &path))
}

/// Runs filesystem call `op` with JSON `args`.
pub fn call(op: &str, args: &str) -> String {
    let args: Value = serde_json::from_str(args).unwrap_or(Value::Null);
    run(op, &args).to_string()
}
