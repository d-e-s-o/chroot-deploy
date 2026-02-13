// Copyright (C) 2023-2026 Daniel Mueller <deso@posteo.net>
// SPDX-License-Identifier: GPL-3.0-or-later

#![allow(clippy::let_and_return, clippy::let_unit_value)]
#![warn(clippy::dbg_macro, clippy::unwrap_used)]

mod args;

use std::env;
use std::env::temp_dir;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fs;
use std::future::ready;
use std::future::Future;
use std::io;
use std::ops::Deref;
use std::ops::DerefMut;
use std::path::Path;
use std::process;
use std::process::ExitStatus;
use std::process::Stdio;
use std::ptr;
use std::str::FromStr as _;
use std::time::Duration;

use anyhow::Context as _;
use anyhow::Result;

use clap::error::ErrorKind;
use clap::Parser as _;

use futures_util::join;
use futures_util::TryFutureExt as _;

use fs4::tokio::AsyncFileExt as _;

use tar::Archive;

use tokio::fs::canonicalize;
use tokio::fs::copy;
use tokio::fs::create_dir_all;
use tokio::fs::remove_dir_all;
use tokio::fs::remove_file;
use tokio::fs::try_exists;
use tokio::fs::File;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncSeekExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command as Process;
use tokio::task::spawn_blocking;
use tokio::time::sleep;

use xz2::read::XzDecoder;

use crate::args::Args;
use crate::args::Command;
use crate::args::Deploy;


/// Extract the file stem of a path.
///
/// Contrary to `Path::file_stem`, this function return the file name
/// part before *any* extensions, not just the last one.
fn file_stem(path: &Path) -> Result<&OsStr> {
  let mut last_stem = path.as_os_str();

  loop {
    let stem = Path::new(last_stem)
      .file_stem()
      .with_context(|| format!("failed to extract file stem of path `{}`", path.display()))?;

    if stem == last_stem {
      break Ok(stem)
    }

    last_stem = stem;
  }
}


#[derive(Debug)]
struct FileLockGuard<'file> {
  /// The locked file.
  file: Option<&'file mut File>,
}

impl<'file> FileLockGuard<'file> {
  async fn lock(file: &'file mut File) -> Result<Self> {
    loop {
      // Really the freakin' `lock_exclusive` API should be returning a
      // future, but that seems to be too much to ask and so we roll our
      // own poor man's future here by trying and retrying after a delay.
      let locked = file.try_lock_exclusive().context("failed to lock file")?;
      if locked {
        let slf = Self { file: Some(file) };
        break Ok(slf)
      } else {
        let () = sleep(Duration::from_millis(100)).await;
      }
    }
  }

  #[cfg(test)]
  fn unlock(mut self) -> Result<&'file mut File> {
    let file = self.file.take().expect("lock guard without a locked file");
    let () = file.unlock().context("failed to unlock file")?;
    Ok(file)
  }
}

impl Deref for FileLockGuard<'_> {
  type Target = File;

  fn deref(&self) -> &Self::Target {
    self
      .file
      .as_ref()
      .expect("lock guard without a locked file")
  }
}

impl DerefMut for FileLockGuard<'_> {
  fn deref_mut(&mut self) -> &mut Self::Target {
    self
      .file
      .as_mut()
      .expect("lock guard without a locked file")
  }
}

impl Drop for FileLockGuard<'_> {
  fn drop(&mut self) {
    if let Some(file) = self.file.as_ref() {
      // TODO: Should not unwrap.
      let () = file.unlock().expect("failed to unlock file");
    }
  }
}


async fn read_pids(file: &mut FileLockGuard<'_>) -> Result<Vec<u32>> {
  let _offset = file.rewind().await?;
  let mut data = String::new();
  let _count = file.read_to_string(&mut data).await?;

  if data.is_empty() {
    Ok(Vec::new())
  } else {
    let mut pids = data
      .lines()
      .filter(|line| !line.is_empty())
      .filter_map(|line| {
        // Transparently ignore lines that are not actual PIDs. The file
        // we read from is effectively user controlled.
        u32::from_str(line).ok()
      })
      .collect::<Vec<u32>>();

    let () = pids.sort_unstable();
    Ok(pids)
  }
}


async fn write_pids(file: &mut FileLockGuard<'_>, pids: &[u32]) -> Result<()> {
  let _offset = file.rewind().await?;
  let () = file
    .set_len(0)
    .await
    .context("failed to truncate PID file")?;

  let content = pids
    .iter()
    .map(|pid| pid.to_string())
    .collect::<Vec<_>>()
    .join("\n");
  let () = file
    .write_all(content.as_bytes())
    .await
    .context("failed to write PIDs")?;
  Ok(())
}


/// Result of adding a PID to the reference file.
#[derive(Debug)]
enum AddPidResult<'file> {
  /// Caller owns the PID file and holds the lock for setup.
  #[allow(dead_code)]
  OwnPid(FileLockGuard<'file>),
  /// Caller should join the namespace of an existing process with this
  /// PID.
  ExistingPid(u32),
}

async fn add_pid(ref_file: &mut File, pid: u32) -> Result<AddPidResult<'_>> {
  let mut guard = FileLockGuard::lock(ref_file).await?;
  let mut pids = read_pids(&mut guard).await?;
  let first_pid = pids.first().copied();

  if let Err(idx) = pids.binary_search(&pid) {
    let () = pids.insert(idx, pid);
  }

  let () = write_pids(&mut guard, &pids).await?;
  if let Some(pid) = first_pid {
    Ok(AddPidResult::ExistingPid(pid))
  } else {
    Ok(AddPidResult::OwnPid(guard))
  }
}

/// # Returns
/// This function returns a lock guard if the PID list became empty
/// after removal.
async fn remove_pid(ref_file: &mut File, pid: u32) -> Result<Option<FileLockGuard<'_>>> {
  let mut guard = FileLockGuard::lock(ref_file).await?;
  let mut pids = read_pids(&mut guard).await?;

  if let Ok(idx) = pids.binary_search(&pid) {
    let _pid = pids.remove(idx);
  } else {
    // Strictly speaking it's an invariant violation if the PID isn't
    // found. Except that the file we work on is effectively user
    // controlled. So just ignore the problem, which should correct
    // itself once we write out the updated PID list.
  }

  let () = write_pids(&mut guard, &pids).await?;
  let guard = if pids.is_empty() { Some(guard) } else { None };
  Ok(guard)
}


async fn with_ref<S, FutS, B, FutB, C, FutC>(
  ref_path: &Path,
  setup: S,
  body: B,
  cleanup: C,
) -> Result<()>
where
  S: FnOnce() -> FutS,
  FutS: Future<Output = Result<()>>,
  B: FnOnce(Option<u32>) -> FutB,
  FutB: Future<Output = Result<()>>,
  C: FnOnce(bool) -> FutC,
  FutC: Future<Output = Result<()>>,
{
  let mut ref_file = File::options()
    .create(true)
    .read(true)
    .write(true)
    .truncate(false)
    .open(ref_path)
    .await
    .with_context(|| format!("failed to open `{}`", ref_path.display()))?;

  let pid = process::id();
  let add_result = add_pid(&mut ref_file, pid).await?;
  let (is_first_user, ns_pid) = match &add_result {
    AddPidResult::OwnPid(_) => (true, None),
    AddPidResult::ExistingPid(pid) => (false, Some(*pid)),
  };

  if is_first_user {
    let setup_result = setup().await;
    let () = drop(add_result);

    // NB: We never concluded the setup code so we do not invoke the
    //     cleanup on any of the error paths.
    if let Err(setup_err) = setup_result {
      match remove_pid(&mut ref_file, pid).await {
        Ok(Some(_guard)) => {
          // We treat lock file removal as optional and ignore errors.
          let _result = remove_file(ref_path).await;
          return Err(setup_err)
        },
        Ok(None) => return Err(setup_err),
        Err(inner_err) => return Err(setup_err.context(inner_err)),
      }
    }
  } else {
    let () = drop(add_result);
  }


  let body_result = body(ns_pid).await;
  let result = match remove_pid(&mut ref_file, pid).await {
    Ok(Some(guard)) => {
      let cleanup_result = cleanup(is_first_user).await;
      // We treat lock file removal as optional and ignore errors.
      let _result = remove_file(ref_path).await;
      let () = drop(guard);
      body_result.and(cleanup_result)
    },
    Ok(None) => body_result,
    Err(inner_err) => {
      // NB: If we fail to remove our PID it's not safe to invoke any
      //     cleanup, because we have no idea how many outstanding
      //     references there may be. All we can do is short-circuit
      //     here.
      body_result.map_err(|err| err.context(inner_err))
    },
  };

  let () = drop(ref_file);
  result
}


/// Unpack a compressed tar archive.
async fn unpack_compressed_tar(archive: &Path, dst: &Path) -> Result<()> {
  let () = create_dir_all(dst)
    .await
    .with_context(|| format!("failed to create directory `{}`", dst.display()))?;
  let archive = archive.to_path_buf();
  let dst = dst.to_path_buf();

  // TODO: Need to support different compression algorithms.
  let result = spawn_blocking(move || {
    let file = fs::File::open(&archive).context("failed to open archive")?;
    let decoder = XzDecoder::new_multi_decoder(file);
    let mut extracter = Archive::new(decoder);
    let () = extracter.set_overwrite(true);
    let () = extracter.set_preserve_ownerships(true);
    let () = extracter.set_preserve_permissions(true);
    let () = extracter.unpack(dst).context("failed to unpack archive")?;
    Ok(())
  })
  .await?;

  result
}


/// Concatenate a command and its arguments into a single string.
fn concat_command<C, A, S>(command: C, args: A) -> OsString
where
  C: AsRef<OsStr>,
  A: IntoIterator<Item = S>,
  S: AsRef<OsStr>,
{
  args
    .into_iter()
    .fold(command.as_ref().to_os_string(), |mut cmd, arg| {
      cmd.push(OsStr::new(" "));
      cmd.push(arg.as_ref());
      cmd
    })
}


/// Format a command with the given list of arguments as a string.
fn format_command<C, A, S>(command: C, args: A) -> String
where
  C: AsRef<OsStr>,
  A: IntoIterator<Item = S>,
  S: AsRef<OsStr>,
{
  concat_command(command, args).to_string_lossy().to_string()
}

fn evaluate<C, A, S>(
  status: ExitStatus,
  command: C,
  args: A,
  stderr: Option<&[u8]>,
) -> io::Result<()>
where
  C: AsRef<OsStr>,
  A: IntoIterator<Item = S>,
  S: AsRef<OsStr>,
{
  if !status.success() {
    let code = if let Some(code) = status.code() {
      format!(" ({code})")
    } else {
      " (terminated by signal)".to_string()
    };

    let stderr = String::from_utf8_lossy(stderr.unwrap_or(&[]));
    let stderr = stderr.trim_end();
    let stderr = if !stderr.is_empty() {
      format!(": {stderr}")
    } else {
      String::new()
    };

    Err(io::Error::other(format!(
      "`{}` reported non-zero exit-status{code}{stderr}",
      format_command(command, args)
    )))
  } else {
    Ok(())
  }
}

/// Run a command with the provided arguments.
async fn run_command<C, A, S>(command: C, args: A) -> io::Result<()>
where
  C: AsRef<OsStr>,
  A: IntoIterator<Item = S> + Clone,
  S: AsRef<OsStr>,
{
  let output = Process::new(command.as_ref())
    .stdin(Stdio::inherit())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .env_clear()
    .envs(env::vars().filter(|(k, _)| k == "PATH"))
    .args(args.clone())
    .output()
    .await
    .map_err(|err| {
      io::Error::other(format!(
        "failed to run `{}`: {err}",
        format_command(command.as_ref(), args.clone())
      ))
    })?;

  let () = evaluate(output.status, command, args, Some(&output.stderr))?;
  Ok(())
}

/// Run a command with the provided arguments.
async fn check_command<C, A, S>(command: C, args: A) -> io::Result<()>
where
  C: AsRef<OsStr>,
  A: IntoIterator<Item = S> + Clone,
  S: AsRef<OsStr>,
{
  let status = Process::new(command.as_ref())
    .stdin(Stdio::inherit())
    .stdout(Stdio::inherit())
    .stderr(Stdio::inherit())
    .env_clear()
    .envs(env::vars().filter(|(k, _)| k == "PATH"))
    .args(args.clone())
    .status()
    .await
    .map_err(|err| {
      io::Error::other(format!(
        "failed to run `{}`: {err}",
        format_command(command.as_ref(), args.clone())
      ))
    })?;

  let () = evaluate(status, command, args, None)?;
  Ok(())
}


/// Create a new mount namespace for the current process.
///
/// This function calls `unshare(CLONE_NEWNS)` to create a new mount
/// namespace and sets the root mount propagation to private to prevent
/// mount events from leaking to the host.
fn create_namespace() -> Result<()> {
  // Create new mount namespace (process-wide)
  let rc = unsafe { libc::unshare(libc::CLONE_NEWNS) };
  if rc != 0 {
    return Err(io::Error::last_os_error()).context("failed to create mount namespace");
  }

  // Set propagation to private (prevent mount events leaking to host).
  let rc = unsafe {
    libc::mount(
      ptr::null(),
      c"/".as_ptr(),
      ptr::null(),
      libc::MS_REC | libc::MS_PRIVATE,
      ptr::null(),
    )
  };
  if rc != 0 {
    return Err(io::Error::last_os_error()).context("failed to set mount propagation to private");
  }

  Ok(())
}


async fn setup_chroot(archive: &Path, chroot: &Path) -> Result<()> {
  // Create mount namespace first (before any mounts).
  let () = create_namespace()?;

  let present = try_exists(chroot)
    .await
    .with_context(|| format!("failed to check existence of `{}`", chroot.display()))?;

  if !present {
    let () = unpack_compressed_tar(archive, chroot)
      .await
      .with_context(|| {
        format!(
          "failed to extract archive `{}` into chroot `{}`",
          archive.display(),
          chroot.display()
        )
      })?;
  }

  let proc = chroot.join("proc");
  let proc = run_command(
    "mount",
    [
      OsStr::new("-t"),
      OsStr::new("proc"),
      OsStr::new("proc"),
      proc.as_os_str(),
    ],
  );
  let dev = chroot.join("dev");
  let dev = create_dir_all(&dev).and_then(|()| {
    run_command(
      "mount",
      [OsStr::new("--rbind"), OsStr::new("/dev"), dev.as_os_str()],
    )
  });
  let sys = chroot.join("sys");
  let sys = create_dir_all(&sys).and_then(|()| {
    run_command(
      "mount",
      [OsStr::new("--rbind"), OsStr::new("/sys"), sys.as_os_str()],
    )
  });
  let repos = chroot.join("var").join("db").join("repos");
  let repos = create_dir_all(&repos).and_then(|()| {
    run_command(
      "mount",
      [
        OsStr::new("--bind"),
        OsStr::new("/var/db/repos"),
        repos.as_os_str(),
      ],
    )
  });
  let tmp = chroot.join("tmp");
  let tmp = create_dir_all(&tmp).and_then(|()| {
    run_command(
      "mount",
      [OsStr::new("--bind"), OsStr::new("/tmp"), tmp.as_os_str()],
    )
  });
  let run = chroot.join("run");
  let run = create_dir_all(&run).and_then(|()| {
    run_command(
      "mount",
      [OsStr::new("--bind"), OsStr::new("/run"), run.as_os_str()],
    )
  });
  let resolve = canonicalize(Path::new("/etc/resolv.conf"))
    .and_then(|resolve| copy(resolve, chroot.join("etc").join("resolv.conf")))
    .and_then(|_count| ready(Ok(())));
  let results = <[_; 7]>::from(join!(proc, dev, sys, repos, tmp, run, resolve));
  let () = results.into_iter().try_for_each(|result| result)?;
  Ok(())
}

async fn chroot(
  chroot_dir: &Path,
  ns_pid: Option<u32>,
  command: Option<&[OsString]>,
  user: Option<&OsStr>,
) -> Result<()> {
  let su_args = [
    OsStr::new("/bin/su"),
    OsStr::new("--login"),
    user.unwrap_or_else(|| OsStr::new("root")),
  ];
  let su_args = su_args.as_slice();

  let session_command = if let Some([cmd, args @ ..]) = command {
    format_command(cmd, args)
  } else {
    let args = [
      r#"PS1="(chroot) \[\033[01;32m\]\u@\h\[\033[01;34m\] \w \$\[\033[00m\] ""#,
      "bash",
      "--norc",
      "-i",
    ];
    format_command("/bin/env", args)
  };

  if let Some(pid) = ns_pid {
    // Join namespace of the first process using `nsenter`.
    let target_arg = format!("--target={pid}");

    let () = check_command(
      "nsenter",
      [
        OsStr::new(&target_arg),
        OsStr::new("--mount"),
        OsStr::new("--"),
      ]
      .into_iter()
      .chain(
        [OsStr::new("chroot"), chroot_dir.as_os_str()]
          .into_iter()
          .chain(su_args.iter().copied())
          .chain([
            OsStr::new("--session-command"),
            OsStr::new(&session_command),
          ]),
      ),
    )
    .await?;
  } else {
    // Already in namespace from setup phase, run `chroot` directly.
    let () = check_command(
      "chroot",
      [chroot_dir.as_os_str()]
        .as_slice()
        .iter()
        .chain(su_args)
        .chain(
          [
            OsStr::new("--session-command"),
            OsStr::new(&session_command),
          ]
          .as_slice(),
        ),
    )
    .await?;
  }
  Ok(())
}

async fn cleanup_chroot(chroot: &Path, is_first_user: bool, remove: bool) -> Result<()> {
  // Only unmount if we're the first user (i.e., our process is in the
  // namespace). Non-first users only had their `nsenter` child process
  // enter the namespace temporarily; their main process (running this
  // cleanup code) remains in the host namespace where these mounts are
  // not visible.
  if is_first_user {
    let run = run_command("umount", [chroot.join("run")]);
    let tmp = run_command("umount", [chroot.join("tmp")]);
    let repos = run_command("umount", [chroot.join("var").join("db").join("repos")]);
    let proc = run_command("umount", [chroot.join("proc")]);
    let sys = run_command(
      "umount",
      [
        OsString::from("--recursive"),
        chroot.join("sys").into_os_string(),
      ],
    );
    let dev = run_command(
      "umount",
      [
        OsString::from("--recursive"),
        chroot.join("dev").into_os_string(),
      ],
    );

    let results = join!(dev, sys, repos, tmp, run);
    // There exists some kind of a dependency causing the `proc` unmount
    // to occasionally fail when run in parallel to the others. So make
    // sure to run it strictly afterwards.
    let result = proc.await;
    let () = <[_; 5]>::from(results)
      .into_iter()
      .chain([result])
      .try_for_each(|result| result)?;
  }

  if remove {
    let () = remove_dir_all(chroot).await?;
  }
  Ok(())
}

/// Handler for the `deploy` sub-command.
async fn deploy(deploy: Deploy) -> Result<()> {
  let Deploy {
    archive,
    command,
    user,
    remove,
  } = deploy;

  let tmp = temp_dir();
  let stem = file_stem(&archive).with_context(|| {
    format!(
      "failed to extract file stem of path `{}`",
      archive.display()
    )
  })?;

  let chroot_dir = tmp.join(stem);
  let ref_path = tmp.join(stem).with_extension("lck");

  let setup = || setup_chroot(&archive, &chroot_dir);
  let chroot = |ns_pid| chroot(&chroot_dir, ns_pid, command.as_deref(), user.as_deref());
  let cleanup = |is_first_user| cleanup_chroot(&chroot_dir, is_first_user, remove);

  with_ref(&ref_path, setup, chroot, cleanup).await
}


/// Run the program and report errors, if any.
pub async fn run<A, T>(args: A) -> Result<()>
where
  A: IntoIterator<Item = T>,
  T: Into<OsString> + Clone,
{
  let args = match Args::try_parse_from(args) {
    Ok(args) => args,
    Err(err) => match err.kind() {
      ErrorKind::DisplayVersion
      | ErrorKind::DisplayHelp
      | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand => {
        print!("{}", err);
        return Ok(())
      },
      _ => return Err(err.into()),
    },
  };

  match args.command {
    Command::Deploy(deploy) => self::deploy(deploy).await,
  }
}


#[cfg(test)]
mod tests {
  use super::*;

  use std::ffi::OsStr;

  use anyhow::anyhow;

  use tempfile::tempfile;
  use tempfile::NamedTempFile;

  use tokio::select;


  /// Check that we can extract a file's stem properly.
  #[test]
  fn file_stem_extraction() {
    let path = Path::new("/tmp/stage3-amd64-openrc-20240211T161834Z.tar.xz");
    let stem = file_stem(path).unwrap();
    assert_eq!(stem, OsStr::new("stage3-amd64-openrc-20240211T161834Z"));
  }

  /// Check that we can add a PID to a file.
  #[tokio::test]
  async fn lock_file_add_pid() {
    let mut file = File::from_std(tempfile().unwrap());

    let AddPidResult::OwnPid(mut guard) = add_pid(&mut file, 1234).await.unwrap() else {
      panic!("expected OwnPid");
    };
    let pids = read_pids(&mut guard).await.unwrap();
    assert_eq!(pids, vec![1234]);
  }

  /// Check that we can add multiple PIDs to a file.
  #[tokio::test]
  async fn lock_file_add_pid_multi() {
    let mut file = File::from_std(tempfile().unwrap());

    let result = add_pid(&mut file, 1000).await.unwrap();
    assert!(matches!(result, AddPidResult::OwnPid(_)));
    drop(result);

    let result = add_pid(&mut file, 2000).await.unwrap();
    assert!(matches!(result, AddPidResult::ExistingPid(1000)));
    drop(result);

    let result = add_pid(&mut file, 3000).await.unwrap();
    assert!(matches!(result, AddPidResult::ExistingPid(1000)));
    drop(result);

    let guard = remove_pid(&mut file, 2000).await.unwrap();
    assert!(guard.is_none());
    drop(guard);

    let guard = remove_pid(&mut file, 1000).await.unwrap();
    assert!(guard.is_none());
    drop(guard);

    let guard = remove_pid(&mut file, 3000).await.unwrap();
    assert!(guard.is_some());
  }

  /// Check that we can remove a PID from a file.
  #[tokio::test]
  async fn lock_file_remove_pid() {
    let mut file = File::from_std(tempfile().unwrap());

    let AddPidResult::OwnPid(guard) = add_pid(&mut file, 1234).await.unwrap() else {
      panic!("expected OwnPid");
    };
    let file = guard.unlock().unwrap();

    let mut guard = remove_pid(file, 1234).await.unwrap().unwrap();
    let pids = read_pids(&mut guard).await.unwrap();
    assert!(pids.is_empty());
  }

  /// Check that PIDs are stored sorted.
  #[tokio::test]
  async fn lock_file_pids_sorted() {
    let mut file = File::from_std(tempfile().unwrap());

    let result = add_pid(&mut file, 3000).await.unwrap();
    drop(result);
    let result = add_pid(&mut file, 1000).await.unwrap();
    drop(result);
    let result = add_pid(&mut file, 2000).await.unwrap();
    drop(result);

    let mut guard = FileLockGuard::lock(&mut file).await.unwrap();
    let pids = read_pids(&mut guard).await.unwrap();
    assert_eq!(pids, vec![1000, 2000, 3000]);
  }

  /// Check that file locking ensures mutual exclusion as expected.
  #[tokio::test]
  async fn lock_file_lock() {
    // We need to work with a named file here, because we should not
    // lock a single `File` instance multiple times. So we open the file
    // multiple times by path instead.
    let file = NamedTempFile::new().unwrap();
    let mut file1 = File::options()
      .read(true)
      .write(true)
      .open(file.path())
      .await
      .unwrap();
    let AddPidResult::OwnPid(_guard1) = add_pid(&mut file1, 1000).await.unwrap() else {
      panic!("expected OwnPid");
    };

    let mut file2 = File::options()
      .read(true)
      .write(true)
      .open(file.path())
      .await
      .unwrap();
    let add = add_pid(&mut file2, 2000);
    let timeout = sleep(Duration::from_millis(10));

    select! {
      result = add => panic!("should not be able to add PID but got: {result:?}"),
      () = timeout => (),
    }
  }

  /// Check that a setup failure is handled as expected by `with_ref`.
  #[tokio::test]
  async fn with_ref_setup_failure() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path();

    let setup = || async { Err(anyhow!("setup fail")) };
    let body = |_ns_pid| async { unreachable!() };
    let cleanup = |_is_first_user| async { unreachable!() };

    let result = with_ref(path, setup, body, cleanup).await;
    assert_eq!(result.unwrap_err().to_string(), "setup fail");
    assert!(!try_exists(path).await.unwrap());
  }

  /// Check that a body failure is handled as expected by `with_ref`.
  #[tokio::test]
  async fn with_ref_body_failure() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path();

    let setup = || async { Ok(()) };
    let body = |_ns_pid| async { Err(anyhow!("body fail")) };
    let cleanup = |_is_first_user| async { Ok(()) };

    let result = with_ref(path, setup, body, cleanup).await;
    assert_eq!(result.unwrap_err().to_string(), "body fail");
    assert!(!try_exists(path).await.unwrap());
  }

  /// Check that a body failure in conjunction with a cleanup is handled
  /// as expected by `with_ref`.
  #[tokio::test]
  async fn with_ref_body_cleanup_failure() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path();

    let setup = || async { Ok(()) };
    let body = |_ns_pid| async { Err(anyhow!("body fail")) };
    let cleanup = |_is_first_user| async { Err(anyhow!("cleanup fail")) };

    let result = with_ref(path, setup, body, cleanup).await;
    assert_eq!(result.unwrap_err().to_string(), "body fail");
    assert!(!try_exists(path).await.unwrap());
  }

  /// Check that a cleanup failure is handled as expected by `with_ref`.
  #[tokio::test]
  async fn with_ref_cleanup_failure() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path();

    let setup = || async { Ok(()) };
    let body = |_ns_pid| async { Ok(()) };
    let cleanup = |_is_first_user| async { Err(anyhow!("cleanup fail")) };

    let result = with_ref(path, setup, body, cleanup).await;
    assert_eq!(result.unwrap_err().to_string(), "cleanup fail");
    assert!(!try_exists(path).await.unwrap());
  }

  /// Check that we do not error out on the --version option.
  #[tokio::test]
  async fn version() {
    let args = [OsStr::new("chroot-deploy"), OsStr::new("--version")];
    let () = run(args).await.unwrap();
  }

  /// Check that we do not error out on the --help option.
  #[tokio::test]
  async fn help() {
    let args = [OsStr::new("chroot-deploy"), OsStr::new("--help")];
    let () = run(args).await.unwrap();
  }

  /// Check that we do not error out when the user didn't provide any
  /// arguments.
  #[tokio::test]
  async fn no_args() {
    let args = [OsStr::new("chroot-deploy")];
    let () = run(args).await.unwrap();
  }
}
