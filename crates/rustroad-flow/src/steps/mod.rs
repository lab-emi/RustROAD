//! Native flow steps: file-level work a producer manifest runs between the
//! engine commands (PDK collateral preparation, the post-route repair loop,
//! the antenna-session records).
//!
//! Every step here was first a Python helper of the Ibex SG13G2 recipe. The
//! records these steps write are pinned by SHA-256 further down the flow, so
//! each port writes the bytes the helper wrote: `rustroad_pyjson` gives the
//! JSON text of CPython's `json` module, and the modules keep the helpers'
//! key order, rounding and file layout. Engine work is never linked in: a
//! step that needs another tool (`rustroad` itself, `rusteda`, `ruststa`)
//! runs the pinned executable named in its inputs as a child process and
//! reads the files that process wrote.
//!
//! Bounds: every input is read under an explicit byte cap; child processes
//! run one at a time unless a step says otherwise, under the step's deadline.
pub mod antenna_session;
pub mod gds_bundle;
pub mod layer_map;
pub mod physical_inputs;
pub mod postroute;
pub mod rc_profile;
mod text;

use rustroad_pyjson::{Dict, FileId, Layout, ParseError, Value, dumps};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// A coded step failure; the display form is `CODE: message`, the shape the
/// command line turns into `{"status":"error","code":CODE,...}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepError {
    pub code: &'static str,
    pub message: String,
}
impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for StepError {}
impl From<std::io::Error> for StepError {
    fn from(error: std::io::Error) -> Self {
        Self {
            code: "STEP_IO",
            message: error.to_string(),
        }
    }
}
impl From<ParseError> for StepError {
    fn from(error: ParseError) -> Self {
        Self {
            code: "STEP_JSON",
            message: error.to_string(),
        }
    }
}

pub type StepResult<T> = Result<T, StepError>;

pub(crate) fn fail(code: &'static str, message: impl Into<String>) -> StepError {
    StepError {
        code,
        message: message.into(),
    }
}

/// `require(ok, message)` of the reference helpers.
pub(crate) fn require(ok: bool, code: &'static str, message: impl Into<String>) -> StepResult<()> {
    if ok { Ok(()) } else { Err(fail(code, message)) }
}

/// Read at most `cap` bytes of a regular file.
pub(crate) fn read_bytes(path: &Path, cap: u64, code: &'static str) -> StepResult<Vec<u8>> {
    let file = File::open(path).map_err(|e| fail(code, format!("{}: {e}", path.display())))?;
    let size = file
        .metadata()
        .map_err(|e| fail(code, format!("{}: {e}", path.display())))?;
    if !size.is_file() || size.len() > cap {
        return Err(fail(code, format!("input byte budget: {}", path.display())));
    }
    let mut data = Vec::with_capacity(size.len() as usize);
    file.take(cap + 1)
        .read_to_end(&mut data)
        .map_err(|e| fail(code, format!("{}: {e}", path.display())))?;
    if data.len() as u64 > cap {
        return Err(fail(code, format!("input byte budget: {}", path.display())));
    }
    Ok(data)
}

/// Read a UTF-8 text file of at most `cap` bytes.
pub(crate) fn read_text(path: &Path, cap: u64, code: &'static str) -> StepResult<String> {
    String::from_utf8(read_bytes(path, cap, code)?)
        .map_err(|_| fail(code, format!("{}: not UTF-8", path.display())))
}

/// Read and decode a JSON file of at most `cap` bytes.
pub(crate) fn read_json(path: &Path, cap: u64, code: &'static str) -> StepResult<Value> {
    rustroad_pyjson::parse(&read_text(path, cap, code)?)
        .map_err(|e| fail(code, format!("{}: {e}", path.display())))
}

pub(crate) fn file_id(path: &Path, cap: u64, code: &'static str) -> StepResult<FileId> {
    FileId::of(path, cap).map_err(|message| fail(code, message))
}

pub(crate) fn render(value: &Value, layout: Layout, code: &'static str) -> StepResult<String> {
    dumps(value, layout).ok_or_else(|| fail(code, "non-finite number in a JSON record"))
}

/// `path.write_text(json.dumps(value, indent=n) + '\n')`.
pub(crate) fn write_json(
    path: &Path,
    value: &Value,
    layout: Layout,
    code: &'static str,
) -> StepResult<()> {
    let mut text = render(value, layout, code)?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| fail(code, format!("{}: {e}", path.display())))
}

/// `open(path, 'x')`: the record must not exist yet.
pub(crate) fn write_json_new(
    path: &Path,
    value: &Value,
    layout: Layout,
    code: &'static str,
) -> StepResult<()> {
    let mut text = render(value, layout, code)?;
    text.push('\n');
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|e| fail(code, format!("{}: {e}", path.display())))?;
    file.write_all(text.as_bytes())
        .map_err(|e| fail(code, format!("{}: {e}", path.display())))
}

/// A step output directory claimed under the helpers' protocol: created if
/// absent, `workspace.lock` held exclusively, and empty apart from the lock.
pub struct FreshDir {
    pub path: PathBuf,
    _lock: File,
}
impl FreshDir {
    /// # Errors
    /// `REVISION_CONFLICT` if the directory holds anything but its lock file
    /// or another process holds the lock.
    pub fn claim(path: &Path) -> StepResult<Self> {
        std::fs::create_dir_all(path)?;
        let path = std::fs::canonicalize(path)?;
        let lock = OpenOptions::new()
            .append(true)
            .create(true)
            .open(path.join("workspace.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .map_err(|_| fail("REVISION_CONFLICT", "step output directory is locked"))?;
        for entry in std::fs::read_dir(&path)? {
            if entry?.file_name() != "workspace.lock" {
                return Err(fail(
                    "REVISION_CONFLICT",
                    "step output directory must be empty (fresh run)",
                ));
            }
        }
        Ok(Self { path, _lock: lock })
    }
    /// Publish the step's revision marker (`HEAD` = `rev1`).
    ///
    /// # Errors
    /// I/O errors.
    pub fn publish(&self) -> StepResult<()> {
        std::fs::write(self.path.join("HEAD"), "rev1\n")?;
        Ok(())
    }
}

pub(crate) fn path_text(path: &Path, code: &'static str) -> StepResult<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| fail(code, format!("{}: path is not UTF-8", path.display())))
}

/// `Path.resolve()` (non-strict): canonical when the path exists, otherwise
/// the canonical parent joined with the last component.
pub(crate) fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
            resolve_lenient(parent).join(name)
        }
        _ => path.to_owned(),
    }
}

pub(crate) fn dict_of<'a>(
    value: &'a Value,
    code: &'static str,
    what: &str,
) -> StepResult<&'a Dict> {
    value
        .as_dict()
        .ok_or_else(|| fail(code, format!("{what}: object required")))
}

pub(crate) fn key<'a>(dict: &'a Dict, name: &str, code: &'static str) -> StepResult<&'a Value> {
    dict.get(name)
        .ok_or_else(|| fail(code, format!("missing field '{name}'")))
}

pub(crate) fn str_of<'a>(value: &'a Value, code: &'static str, what: &str) -> StepResult<&'a str> {
    value
        .as_str()
        .ok_or_else(|| fail(code, format!("{what}: string required")))
}

/// Peak resident set of waited-for children or of this process, in KiB
/// (`resource.getrusage(...).ru_maxrss` on Linux).
pub(crate) fn peak_rss_kib(children: bool) -> i64 {
    // SAFETY: `getrusage` only writes the plain-data struct it is given.
    #[allow(unsafe_code)]
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        let who = if children {
            libc::RUSAGE_CHILDREN
        } else {
            libc::RUSAGE_SELF
        };
        if libc::getrusage(who, &mut usage) != 0 {
            return 0;
        }
        usage
    };
    usage.ru_maxrss
}
