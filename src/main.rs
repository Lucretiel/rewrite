use std::borrow::Cow;
use std::env;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{exit, Command, Stdio};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;

#[cfg(windows)]
use std::os::windows::process::ExitStatusExt;

use joinery::Joinable;
use structopt::StructOpt;
use tempfile::{Builder as TempFileBuilder, PersistError};

/// rewrite edits a file in place in place with a command.
///
/// rewrite is a tool that edits a file in place with a command. The file is sent to the command
/// via stdin, and is rewritten with the command's stdout. rewrite works by writing the command's
/// stdout to a temporary file, then replacing the existing file with the temporary one. It's
/// roughly equivelent to:
///
///     my_command < file.txt > file.txt
///
/// By default, the temporary file is created in the same directory as the target file, though
/// this can be changed.
///
/// If the command exits with a nonzero exit code, the target file is *not* overwritten.
#[derive(Debug, StructOpt)]
struct Opt {
    // TODO: drop-priveleges. Should be relatively easy, but the process::Command interface
    // makes it surprisingly difficult to compose.
    /// Run the command as normal (including writing the temporary file), but don't modify the file
    #[structopt(short = "n", long = "no-op")]
    no_op: bool,

    /// Don't set REWRITE_* environment variables in the target
    #[structopt(short = "e", long = "no-env")]
    no_env: bool,

    /// Create the temporary file in the same directory as the target file. This is the default.
    #[structopt(
        short = "s",
        long = "sibling-temp",
        raw(overrides_with_all = r#"&["tmpdir-temp", "dir"]"#)
    )]
    sibling_dir: bool,

    /// Create the temporary file in the system temporary directory.
    #[structopt(short = "t", long = "tmpdir-temp")]
    tmpdir: bool,

    /// Create the temporary file in the given directory
    #[structopt(short = "d", long = "dir")]
    target_dir: Option<PathBuf>,

    // TODO: make this work on windows
    /// Shell mode: concatenate the command with whitespace and run it in the shell (via sh -c)
    #[structopt(short = "c", long = "shell-mode")]
    shell_mode: bool,

    // TODO: it might be better to make this the default, and have a "keep-root" instead
    /// When running `sudo rewrite` to edit root files, run the command as the original user
    /// instead of root.
    #[structopt(short = "D", long = "drop-root")]
    drop_root: bool,

    #[structopt(parse(from_os_str))]
    /// The file to rewrite
    rewrite_path: PathBuf,

    #[structopt(raw(last = "true"), raw(required = "true"))]
    /// The subcommand to run
    command: Vec<String>,
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

// Trait for calling exit on errors
trait Bailer<T, E> {
    fn or_bail(self, f: impl FnOnce(E)) -> T;
}

impl<T, E> Bailer<T, E> for Result<T, E> {
    fn or_bail(self, f: impl FnOnce(E)) -> T {
        self.unwrap_or_else(move |err| {
            f(err);
            exit(1)
        })
    }
}

#[derive(Debug)]
enum RewriteError<'a> {
    Open(io::Error),
    CreateTemp { dir: &'a Path, err: io::Error },
    DupTemp(io::Error),
    SpawnChild(io::Error),
    Signal(Option<i32>),
    Persist(PersistError),
    NoSudoUser(env::VarError),
    GetPermissions(io::Error),
    SetPermissions(io::Error),
}

fn run<'a>(sys_temp_dir: &'a Path, opt: &'a Opt) -> Result<i32, RewriteError<'a>> {
    let path = &opt.rewrite_path;
    let file = File::open(path).map_err(RewriteError::Open)?;
    let file_permissions = file
        .metadata()
        .map_err(RewriteError::GetPermissions)?
        .permissions();

    // Get the desired directory
    let dir_path = if opt.tmpdir {
        sys_temp_dir
    } else if let Some(ref target_dir) = opt.target_dir {
        target_dir
    } else {
        path.parent()
            .expect("Target file doesn't have a parent directory?")
    };

    let filename = path.file_name().expect("Target file doesn't have a name?");

    // Attach the filename as a suffix so that we can tell what file this is scratch for
    let scratch_file = TempFileBuilder::new()
        .prefix(".rewrite-tmp-")
        .suffix(filename.to_string_lossy().as_ref())
        .tempfile_in(dir_path)
        .map_err(|err| RewriteError::CreateTemp { dir: dir_path, err })?;

    // We can't pass a NamedTempFile to a subprocess, so we attempt to duplicate
    // the file descriptor and create a `File`.
    // TODO: in principle, this shouldn't be necessary. A NamedTemporaryFile is
    // a pair of File and TempPath, the latter of which deletes the file on drop.
    // There is an open issue on github to allow the file to be destructured:
    // https://github.com/Stebalien/tempfile/issues/60
    let scratch_file_for_child = scratch_file
        .as_file()
        .try_clone()
        .map_err(RewriteError::DupTemp)?;

    let mut constructed_command = Vec::with_capacity(opt.command.len());

    if opt.drop_root {
        // Get the user id from the environment
        let username = env::var("SUDO_USER").map_err(RewriteError::NoSudoUser)?;
        constructed_command.push(Cow::Borrowed("sudo"));
        constructed_command.push(Cow::Borrowed("--user"));
        constructed_command.push(Cow::Owned(username));
        constructed_command.push(Cow::Borrowed("--"));
    }

    if opt.shell_mode {
        // Concat the command
        let command = opt.command.iter().join_with(' ').to_string();
        constructed_command.push("sh".into());
        constructed_command.push("-c".into());
        constructed_command.push(command.into());
    } else {
        constructed_command.extend(opt.command.iter().map(|part| Cow::Borrowed(part.as_str())));
    }

    let mut cmd_iter = constructed_command.iter().map(AsRef::as_ref);

    // Construct the command. We're guaranteed to have a command because structopt
    // requires the command vector to have at least one element.
    let mut command = Command::new(cmd_iter.next().expect("No command was given"));

    // Attach arguments
    command.args(cmd_iter);

    // Attach environment
    if !opt.no_env {
        command.env("REWRITE_FILE", path);
    }

    // Attach input file, output file
    command
        .stdin(file)
        .stdout(scratch_file_for_child)
        .stderr(Stdio::inherit());

    // And go!
    let child_result = command.status().map_err(RewriteError::SpawnChild)?;

    // Check for success
    match child_result.code() {
        Some(0) => {}
        Some(code) => return Ok(code),
        None => return Err(RewriteError::Signal(child_result.exit_signal())),
    };

    if !opt.no_op {
        scratch_file
            .persist(path)
            .map_err(RewriteError::Persist)?
            .set_permissions(file_permissions)
            .map_err(RewriteError::SetPermissions)?;
    }

    Ok(0)
}

fn main() {
    use crate::RewriteError::*;

    let opt = Opt::from_args();
    let path = &opt.rewrite_path;
    let sys_temp_dir = env::temp_dir();

    let result = run(&sys_temp_dir, &opt);

    let code = match result {
        Ok(0) => 0,
        Ok(code) => {
            eprintln!("Command exited with status code {}; skipping write", code);
            code
        }
        Err(err) => {
            match err {
                Open(err) => eprintln!("Error opening '{}' for read: {}", path.display(), err),
                CreateTemp { dir, err } => eprintln!(
                    "Error creating temporary file in '{}': {}",
                    dir.display(),
                    err
                ),
                DupTemp(err) => eprintln!("Error creating duplicate file descriptor: {}", err),
                SpawnChild(err) => eprintln!("Error spawning command: {}", err),
                Signal(None) => eprintln!("Command terminated from unknown signal"),
                Signal(Some(sig)) => eprintln!("Command terminated from signal {}", sig),
                Persist(err) => eprintln!("Error persisting temporary file: {}", err),
                NoSudoUser(err) => eprintln!("--drop-priveleges was given, but there was an error reading SUDO_USER: {}", err),
                GetPermissions(err) => eprintln!("Error getting file permissions for {}: {}", path.display(), err),
                SetPermissions(err) => eprintln!("command completed successfully, but error restoring file permissions to the new file: {}", err),
            }
            1
        }
    };

    exit(code);
}
