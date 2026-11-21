/* SPDX-License-Identifier: GPL-3.0-or-later */
//! `gen_top` — `php utils/bin/gen_top.php` as a binary: the ten hottest articles of the
//! recommended boards, as `var/bbs_top.txt` and `var/bbs_top_menu.conf`.
use std::process::ExitCode;

use lbbs::{batch, gen_top};

fn main() -> ExitCode {
    batch::run(gen_top::run)
}
