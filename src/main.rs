extern crate structopt;
extern crate tempfile;

use std::error::Error;
use std::ffi::OsStr;
use std::fmt::{self, Display, Formatter};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Path, PathBuf};
use std::process::{exit, Child, Command, Stdio};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;

use structopt::StructOpt;
use tempfile::tempfile;

#[derive(Debug, StructOpt)]
struct Opt {
    #[structopt(short = "b", long = "buffer-size", default_value = "8388608")]
    /// The size of the in-memory buffer to use
    ///
    /// If the command output exceeds the buffer, the command will create and use
    /// a temporary file for the remaining output. Defaults to 8MB.
    buffer_size: usize,

    #[structopt(short = "o", long = "buffer-only")]
    /// Don't use a temporary file. Fail if the file size exceeds the in-memory buffer.
    buffer_only: bool,

    // TODO: drop-priveleges. Should be relatively easy, but the process::Command interface
    // makes it surprisingly difficult to compose.
    #[structopt(short = "n", long = "no-op")]
    /// Run the command as normal, but don't modify the file
    ///
    /// In no-op mode, rewrite will do everything it normally does, including
    /// writing to a temporary file, if enabled. The only thing it doesn't do is
    /// modify the contents of the file.
    no_op: bool,

    #[structopt(short = "E", long = "no-env")]
    /// Don't set REWRITE_* environment variables in the target
    no_env: bool,

    #[structopt(parse(from_os_str))]
    /// The file to rewrite
    rewrite_path: PathBuf,

    #[structopt(raw(last = "true"), raw(required = "true"))]
    /// The subcommand to run
    command: Vec<String>,
}

macro_rules! select_write {
    ($match:expr => $dest:expr => {$(
        $pat:pat => $fmt:expr $(, $arg:expr)*;
    )*}) => ({
        match $match {$(
            $pat => write!($dest, $fmt, $($arg,)*),
        )*}
    })
}

#[derive(Debug)]
enum RewriteError<'a> {
    ReadOpenError { path: &'a Path, err: io::Error },
    WriteOpenError { path: &'a Path, err: io::Error },
    WriteError { path: &'a Path, err: io::Error },
    SpawnError { command: Command, err: io::Error },
    CommandPipeError(io::Error),
    CommandExitCode(i32),
    CommandExitSignal(Option<i32>),
    CreateTempfileError(io::Error),
    TempfileWriteError(io::Error),
    TempfileSeekError(io::Error),
    TempfileDisallowed,
}

impl<'a> Display for RewriteError<'a> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        use RewriteError::*;

        select_write!(self => f => {
            ReadOpenError { path, err } =>
                "Failed to open '{}': {}", path.display(), err;
            WriteOpenError { path, err } =>
                "Failed to open '{}' for writing: {}",path.display(), err;
            WriteError { path, err } =>
                "Error writing tp '{}': {}", path.display(), err;
            SpawnError { command, err } =>
                "Failed to spawn rewrite command ({:?}): {}", command, err;
            CommandPipeError(err) =>
                "Error reading subprocess output: {}", err;
            CommandExitCode(code) =>
                "Command exited with non-zero status code {}", code;
            CommandExitSignal(Some(sig)) =>
                "Subprocess exited via signal {}", sig;
            CommandExitSignal(None) =>
                "Subprocess exited due to unknown signal";
            CreateTempfileError(err) =>
                "Failed to create temporary file for scratch space: {}", err;
            TempfileWriteError(err) =>
                "Error copying process output into temporary file: {}", err;
            TempfileSeekError(err) =>
                "Error seeking temporary file: {}", err;
            TempfileDisallowed =>
                "command output exceeded in-memory buffer limit";
        })
    }
}

impl<'a> Error for RewriteError<'a> {
    fn cause(&self) -> Option<&dyn Error> {
        use RewriteError::*;

        match self {
            ReadOpenError { err, .. } => Some(err),
            WriteOpenError { err, .. } => Some(err),
            WriteError { err, .. } => Some(err),
            SpawnError { err, .. } => Some(err),
            CommandPipeError(err) => Some(err),
            CommandExitCode(..) => None,
            CommandExitSignal(..) => None,
            CreateTempfileError(err) => Some(err),
            TempfileWriteError(err) => Some(err),
            TempfileSeekError(err) => Some(err),
            TempfileDisallowed => None,
        }
    }
}

#[derive(Debug)]
struct ProcessResult {
    buffer: Vec<u8>,
    file: Option<File>,
}

impl ProcessResult {
    fn new_buffer(buffer: Vec<u8>) -> Self {
        ProcessResult { buffer, file: None }
    }

    fn new_buffer_file(buffer: Vec<u8>, mut file: File) -> io::Result<Self> {
        file.seek(SeekFrom::Start(0))?;
        Ok(ProcessResult {
            buffer,
            file: Some(file),
        })
    }

    fn write_to_file(self, dest: &mut File) -> io::Result<()> {
        dest.write_all(&self.buffer)?;

        match self.file {
            Some(mut file) => io::copy(&mut file, dest).map(|_| ()),
            None => Ok(()),
        }
    }
}

trait ExitStatusSignal {
    fn exit_signal(&self) -> Option<i32>;
}

#[cfg(unix)]
impl<T: ExitStatusExt> ExitStatusSignal for T {
    fn exit_signal(&self) -> Option<i32> {
        self.signal()
    }
}

#[cfg(windows)]
impl<T: ExitStatusExt> ExitStatusSignal for T {
    fn exit_signal(&self) -> Option<i32> {
        None
    }
}

// Drop wrapper that ensures a child is .kill'd when it goes out of scope
#[derive(Debug)]
struct KillChild(Child);

impl Drop for KillChild {
    fn drop(&mut self) {
        if let Err(err) = self.kill() {
            panic!("Failed to kill subprocess: {}", err);
        }
    }
}

impl From<Child> for KillChild {
    fn from(child: Child) -> Self {
        KillChild(child)
    }
}

impl Deref for KillChild {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for KillChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

fn process_file<Arg, Cmd>(
    path: &Path,
    cmd_parts: Cmd,
    with_env: bool,
    buffer_size: usize,
    buffer_only: bool,
) -> Result<ProcessResult, RewriteError>
where
    Arg: AsRef<OsStr>,
    Cmd: IntoIterator<Item = Arg>,
{
    let file = File::open(path).map_err(|err| RewriteError::ReadOpenError { err, path })?;

    let mut cmd_iter = cmd_parts.into_iter();

    // Create command. The expect shouldn't trigger, since structopt requires at least 1 arg.
    let mut command = Command::new(cmd_iter.next().expect("No command was given"));

    // Attach arguments
    command.args(cmd_iter);

    // Attach environment
    if with_env {
        command
            .env("REWRITE_PATH", path)
            // Panic shouldn't trigger, because File::open would have already failed.
            .env("REWRITE_FILENAME", path.file_name().expect("Invalid filename"));
    }

    // Attach pipes to the command
    command
        .stdin(file)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    // And go!
    // TODO: create a Drop wrapper that kills the process if something panics
    let mut child: KillChild = command
        .spawn()
        .map_err(|err| RewriteError::SpawnError { command, err })?
        .into();

    let process_result = {
        let child_stdout = child.stdout.as_mut().expect("Failed to get child stdout");

        // First, read into a buffer. Fall back to a file if we exceed buffer-size
        let mut buffer = Vec::with_capacity(buffer_size);

        let amount_read = {
            let mut limited_reader = child_stdout.take(buffer_size as u64);
            limited_reader
                .read_to_end(&mut buffer)
                .map_err(RewriteError::CommandPipeError)?
        };

        if amount_read >= buffer_size {
            if !buffer_only {
                // If we exceeded the buffer limit, copy the remaining bytes to an unnamed temporary
                // file.
                let mut scratch_file = tempfile().map_err(RewriteError::CreateTempfileError)?;
                io::copy(child_stdout, &mut scratch_file)
                    .map_err(RewriteError::TempfileWriteError)?;
                ProcessResult::new_buffer_file(buffer, scratch_file)
                    .map_err(RewriteError::TempfileSeekError)?
            } else {
                return Err(RewriteError::TempfileDisallowed);
            }
        } else {
            ProcessResult::new_buffer(buffer)
        }
    };

    let child_result = child
        .wait()
        .expect("Failed to wait for subprocess to finish");

    match child_result.code() {
        Some(0) => Ok(process_result),
        Some(code) => Err(RewriteError::CommandExitCode(code)),
        None => Err(RewriteError::CommandExitSignal(child_result.exit_signal())),
    }
}

fn run(opt: &Opt) -> Result<(), RewriteError> {
    let path = &opt.rewrite_path;

    let processed_data = process_file(
        path,
        opt.command.iter(),
        !opt.no_env,
        opt.buffer_size,
        opt.buffer_only,
    )?;

    if !opt.no_op {
        let mut dest_file = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&opt.rewrite_path)
            .map_err(|err| RewriteError::WriteOpenError { err, path })?;

        processed_data
            .write_to_file(&mut dest_file)
            .map_err(|err| RewriteError::WriteError { err, path })
    } else {
        eprintln!("rewrite: successfully processed file. --no-op, skipping writeback.");
        Ok(())
    }
}

fn main() {
    let opt = Opt::from_args();

    if let Err(err) = run(&opt) {
        eprintln!("rewrite error: {}", err);
        exit(1);
    }
}
