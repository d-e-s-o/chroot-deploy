// Copyright (C) 2023-2024 Daniel Mueller <deso@posteo.net>
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
use std::mem::size_of;
use std::ops::Deref;
use std::ops::DerefMut;
use std::path::Path;
use std::process::ExitStatus;
use std::process::Stdio;
use std::str;
use std::str::FromStr as _;
use std::time::Duration;

use anyhow::Context as _;
use anyhow::Result;

use clap::error::ErrorKind;
use clap::Parser as _;

use futures_util::join;
use futures_util::TryFutureExt as _;

use fs4::lock_contended_error;
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
    let locked_err = lock_contended_error();
    loop {
      // Really the freakin' `lock_exclusive` API should be returning a
      // future, but that seems to be too much to ask and so we roll our
      // own poor man's future here by trying and retrying after a delay.
      match file.try_lock_exclusive() {
        Ok(()) => {
          let slf = Self { file: Some(file) };
          break Ok(slf)
        },
        Err(err) => {
          if err.kind() == locked_err.kind() {
            let () = sleep(Duration::from_millis(100)).await;
          } else {
            break Err(err).context("failed to lock file")
          }
        },
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


async fn read_ref_cnt(file: &mut FileLockGuard<'_>) -> Result<usize> {
  let _offset = file.rewind().await?;
  let mut buffer = [0u8; size_of::<usize>()];
  let count = file.read(&mut buffer).await?;
  if count == 0 {
    Ok(0)
  } else {
    let data = buffer.get(0..count).expect("read returned invalid count");
    let data = str::from_utf8(data).context("reference count file data is not valid UTF-8")?;
    let ref_cnt = usize::from_str(data)
      .context("reference count file does not contain a valid reference count")?;
    Ok(ref_cnt)
  }
}


async fn write_ref_cnt(file: &mut FileLockGuard<'_>, ref_cnt: usize) -> Result<()> {
  let _offset = file.rewind().await?;
  let ref_cnt = ref_cnt.to_string();
  let () = file
    .write_all(ref_cnt.as_bytes())
    .await
    .context("failed to write reference count")?;
  Ok(())
}


/// # Returns
/// This function returns a lock guard if the reference count is one, in
/// which case callers may want to perform a one-time initialization.
async fn inc_ref_cnt(ref_file: &mut File) -> Result<Option<FileLockGuard<'_>>> {
  let mut guard = FileLockGuard::lock(ref_file).await?;
  let ref_cnt = read_ref_cnt(&mut guard).await?;
  let () = write_ref_cnt(&mut guard, ref_cnt + 1).await?;
  let guard = if ref_cnt == 0 { Some(guard) } else { None };
  Ok(guard)
}

/// # Returns
/// This function returns a lock guard if the reference count reached
/// zero.
async fn dec_ref_cnt(ref_file: &mut File) -> Result<Option<FileLockGuard<'_>>> {
  let mut guard = FileLockGuard::lock(ref_file).await?;
  let ref_cnt = read_ref_cnt(&mut guard).await?;
  let () = write_ref_cnt(
    &mut guard,
    ref_cnt
      .checked_sub(1)
      .expect("cannot decrease reference count of zero"),
  )
  .await?;

  let guard = if ref_cnt == 1 { Some(guard) } else { None };
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
  B: FnOnce() -> FutB,
  FutB: Future<Output = Result<()>>,
  C: FnOnce() -> FutC,
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

  let guard = inc_ref_cnt(&mut ref_file).await?;
  if guard.is_some() {
    let setup_result = setup().await;
    let () = drop(guard);

    // NB: We never concluded the setup code so we do not invoke the
    //     cleanup on any of the error paths.
    if let Err(setup_err) = setup_result {
      match dec_ref_cnt(&mut ref_file).await {
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
    let () = drop(guard);
  }


  let body_result = body().await;
  let result = match dec_ref_cnt(&mut ref_file).await {
    Ok(Some(guard)) => {
      let cleanup_result = cleanup().await;
      // We treat lock file removal as optional and ignore errors.
      let _result = remove_file(ref_path).await;
      let () = drop(guard);
      body_result.and(cleanup_result)
    },
    Ok(None) => body_result,
    Err(inner_err) => {
      // NB: If we fail to decrement the reference count it's not safe
      //     to invoke any cleanup, because we have no idea how many
      //     outstanding references there may be. All we can do is
      //     short-circuit here.
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
    let () = extracter.unpack(dst).context("failed to unpack archive")?;
    Ok(())
  })
  .await?;

  result
}


/// Format a command with the given list of arguments as a string.
fn format_command<C, A, S>(command: C, args: A) -> String
where
  C: AsRef<OsStr>,
  A: IntoIterator<Item = S>,
  S: AsRef<OsStr>,
{
  args.into_iter().fold(
    command.as_ref().to_string_lossy().into_owned(),
    |mut cmd, arg| {
      cmd += " ";
      cmd += arg.as_ref().to_string_lossy().deref();
      cmd
    },
  )
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

    Err(io::Error::new(
      io::ErrorKind::Other,
      format!(
        "`{}` reported non-zero exit-status{code}{stderr}",
        format_command(command, args)
      ),
    ))
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
      io::Error::new(
        io::ErrorKind::Other,
        format!(
          "failed to run `{}`: {err}",
          format_command(command.as_ref(), args.clone())
        ),
      )
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
      io::Error::new(
        io::ErrorKind::Other,
        format!(
          "failed to run `{}`: {err}",
          format_command(command.as_ref(), args.clone())
        ),
      )
    })?;

  let () = evaluate(status, command, args, None)?;
  Ok(())
}


async fn setup_chroot(archive: &Path, chroot: &Path) -> Result<()> {
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

async fn chroot(chroot: &Path, command: Option<&[OsString]>, user: Option<&OsStr>) -> Result<()> {
  let args = [
    OsStr::new("/bin/su"),
    OsStr::new("--login"),
    user.unwrap_or_else(|| OsStr::new("root")),
  ];
  let args = args.as_slice();

  let command = if let Some(command) = command {
    let mut iter = command.iter();
    format_command(iter.next().context("no command given")?, iter)
  } else {
    let args = [
      r#"PS1="(chroot) \[\033[01;32m\]\u@\h\[\033[01;34m\] \w \$\[\033[00m\] ""#,
      "bash",
      "--norc",
      "-i",
    ];
    format_command("/bin/env", args)
  };

  let () = check_command(
    "chroot",
    [chroot.as_os_str()]
      .as_slice()
      .iter()
      .chain(args)
      .chain([OsStr::new("--session-command"), OsStr::new(&command)].as_slice()),
  )
  .await?;
  Ok(())
}

async fn cleanup_chroot(chroot: &Path, remove: bool) -> Result<()> {
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
  let chroot = || chroot(&chroot_dir, command.as_deref(), user.as_deref());
  let cleanup = || cleanup_chroot(&chroot_dir, remove);

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
      ErrorKind::DisplayHelp | ErrorKind::DisplayVersion => {
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

  /// Check that we can increment the reference count of a file.
  #[tokio::test]
  async fn lock_file_ref_cnt_inc() {
    let mut file = File::from_std(tempfile().unwrap());

    let mut guard = inc_ref_cnt(&mut file).await.unwrap().unwrap();
    let ref_cnt = read_ref_cnt(&mut guard).await.unwrap();
    assert_eq!(ref_cnt, 1);
  }

  /// Check that we can increment the reference count of a file multiple
  /// times.
  #[tokio::test]
  async fn lock_file_ref_cnt_inc_multi() {
    let mut file = File::from_std(tempfile().unwrap());

    let guard = inc_ref_cnt(&mut file).await.unwrap();
    assert!(guard.is_some());
    drop(guard);

    let guard = inc_ref_cnt(&mut file).await.unwrap();
    assert!(guard.is_none());
    drop(guard);

    let guard = inc_ref_cnt(&mut file).await.unwrap();
    assert!(guard.is_none());
    drop(guard);

    let guard = dec_ref_cnt(&mut file).await.unwrap();
    assert!(guard.is_none());
    drop(guard);

    let guard = dec_ref_cnt(&mut file).await.unwrap();
    assert!(guard.is_none());
    drop(guard);

    let guard = dec_ref_cnt(&mut file).await.unwrap();
    assert!(guard.is_some());
  }

  /// Check that we can decrement the reference count of a file.
  #[tokio::test]
  async fn lock_file_ref_cnt_dec() {
    let mut file = File::from_std(tempfile().unwrap());

    let guard = inc_ref_cnt(&mut file).await.unwrap().unwrap();
    let file = guard.unlock().unwrap();

    let mut guard = dec_ref_cnt(file).await.unwrap().unwrap();
    let ref_cnt = read_ref_cnt(&mut guard).await.unwrap();
    assert_eq!(ref_cnt, 0);
  }

  /// Check that file locking ensures mutual exclusion as expected.
  #[tokio::test]
  async fn lock_file_lock() {
    // We need to work with a named file here, because we should not
    // lock a single `File` instance multiple times. So we open the file
    // multiple times by path instead.
    let file = NamedTempFile::new().unwrap();
    let mut file1 = File::open(file.path()).await.unwrap();
    let _guard1 = inc_ref_cnt(&mut file1).await.unwrap().unwrap();

    let mut file2 = File::open(file.path()).await.unwrap();
    let inc = inc_ref_cnt(&mut file2);
    let timeout = sleep(Duration::from_millis(10));

    select! {
      result = inc => panic!("should not be able to increment reference count but got: {result:?}"),
      () = timeout => (),
    }
  }

  /// Check that a setup failure is handled as expected by `with_ref`.
  #[tokio::test]
  async fn with_ref_setup_failure() {
    let file = NamedTempFile::new().unwrap();
    let path = file.path();

    let setup = || async { Err(anyhow!("setup fail")) };
    let body = || async { unreachable!() };
    let cleanup = || async { unreachable!() };

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
    let body = || async { Err(anyhow!("body fail")) };
    let cleanup = || async { Ok(()) };

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
    let body = || async { Err(anyhow!("body fail")) };
    let cleanup = || async { Err(anyhow!("cleanup fail")) };

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
    let body = || async { Ok(()) };
    let cleanup = || async { Err(anyhow!("cleanup fail")) };

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
}
