// Copyright (C) 2023 Daniel Mueller <deso@posteo.net>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::env::args_os;

use anyhow::Result;

use chroot_deploy::run;


#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
  run(args_os()).await
}
