// Copyright (C) 2023 Daniel Mueller <deso@posteo.net>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Args as Arguments;
use clap::Parser;
use clap::Subcommand;


/// Easy, fast, reference count supported deployment of chroot
/// environments.
#[derive(Debug, Parser)]
#[clap(version = env!("VERSION"))]
pub struct Args {
  #[command(subcommand)]
  pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
  /// Deploy a chroot.
  Deploy(Deploy),
}

/// An type representing the `deploy` command.
#[derive(Debug, Arguments)]
pub struct Deploy {
  /// The path to the compressed archive representing the chroot to
  /// deploy.
  pub archive: PathBuf,
  /// The user to log in as.
  #[clap(short, long)]
  pub user: Option<OsString>,
  /// Whether or not to remove the unpacked chroot file system after the
  /// last user left.
  #[clap(short, long)]
  pub remove: bool,
  /// The optional command to execute after deployment.
  #[clap(short, long, num_args(1..))]
  pub command: Option<Vec<OsString>>,
}
