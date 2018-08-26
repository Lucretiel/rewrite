extern crate structopt;
extern crate tempfile;

use std::error::Error;
use std::ffi::OsStr;
use std::fmt::{self, Display, Formatter};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};

use structopt::StructOpt;

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

    #[structopt(short = "i", long = "ignore-nonzero")]
    /// Ignore nonzero exit codes from the subprocess

    #[structopt(
        long = "tmpdir-sibling",
        raw(
            overrides_with_all = "&[
        \"tmpdir-sibling\",
        \"tmpdir-cwd\",
        \"tmpdir-system\",
        \"tmpdir\",
    ]"
        )
    )]
    /// Create the temporary file in the same directory as the target file. This is the default.
    tmpdir_sibling: bool,

    #[structopt(long = "tmpdir-cwd")]
    /// Create the temporary file in the current working directory.
    tmpdir_cwd: bool,

    #[structopt(long = "tmpdir-system")]
    /// Create the temporary file in the system temporary directory.
    tmpdir_system: bool,

    #[structopt(long = "tmpdir", short = "t", parse(from_os_str))]
    /// Create the temporary file in this directory.
    tmpdir: Option<PathBuf>,

    #[structopt(parse(from_os_str))]
    /// The file to rewrite
    rewrite_path: PathBuf,

    #[structopt(raw(last = "true"), raw(required = "true"))]
    /// The subcommand to run
    command: Vec<String>,
}

#[derive(Debug)]
enum RewriteError<'a> {
    // Error opening the file for read
    ReadOpenError { path: &'a Path, err: io::Error },

    // Error opening the file for write
    WriteOpenError { path: &'a Path, err: io::Error },

    // Error writing to the file
    WriteError { path: &'a Path, err: io::Error },

    //"Failed to spawn command\n\tcommand: {:?}\n\treason: {}", command, err
    SpawnError { command: Command, err: io::Error },

    CommandPipeError(io::Error),

    CommandExitCode(i32),

    CommandExitSignal(Option<i32>),
}

impl<'a> Display for RewriteError<'a> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        use RewriteError::*;

        match self {
            ReadOpenError { path, err } => {
                write!(f, "Failed to open '{}': {}", path.display(), err)
            }
            WriteOpenError { path, err } => write!(
                f,
                "Failed to open '{}' for writing: {}",
                path.display(),
                err
            ),
            WriteError { path, err } => write!(f, "Error writing tp '{}': {}", path.display(), err),
            SpawnError { command, err } => write!(
                f,
                "Failed to spawn rewrite command ({:?}): {}",
                command, err
            ),
            CommandPipeError(err) => write!(f, "Error reading subprocess output: {}", err),

            CommandExitCode(code) => write!(f, "Command exited with non-zero status code {}", code),
            CommandExitSignal(Some(sig)) => write!(f, "Subprocess exited via signal {}", sig),
            CommandExitSignal(None) => write!(f, "Subprocess exited due to unknown signal"),
        }
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

fn process_file<Arg, Cmd>(
    path: &Path,
    cmd_parts: Cmd,
    with_env: bool,
    buffer_size: usize,
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
    let mut running_command = command
        .spawn()
        .map_err(|err| RewriteError::SpawnError { command, err })?;

    let child_stdout = running_command
        .stdout
        .as_mut()
        .expect("Failed to get child stdout");

    // First, read into a buffer. Fall back to a file if we exceed buffer-size
    let mut buffer = Vec::with_capacity(buffer_size);

    let amount_read = {
        let mut limited_reader = child_stdout.take(buffer_size as u64);
        limited_reader
            .read_to_end(&mut buffer)
            .map_err(RewriteError::CommandPipeError)?
    };

    if amount_read >= buffer_size {
        panic!("Don't yet support large files");
    } else {
        Ok(ProcessResult::new_buffer(buffer))
    }
}

fn run(opt: &Opt) -> Result<(), RewriteError> {
    let path = &opt.rewrite_path;

    let processed_data = process_file(path, opt.command.iter(), !opt.no_env, opt.buffer_size)?;

    let mut dest_file = OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&opt.rewrite_path)
        .map_err(|err| RewriteError::WriteOpenError { err, path })?;

    processed_data
        .write_to_file(&mut dest_file)
        .map_err(|err| RewriteError::WriteError { err, path })
}

fn main() {
    let opt = Opt::from_args();

    if let Err(err) = run(&opt) {
        eprintln!("{}", err);
        exit(1);
    }
}
