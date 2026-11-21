/* SPDX-License-Identifier: GPL-3.0-or-later */
//! `gen_ex_list` — `php utils/bin/gen_ex_list.php` as a binary: one digest menu file per board
//! whose `ex_menu_update` flag is set, written under `var/gen_ex/`.
use std::process::ExitCode;

use lbbs::{batch, gen_ex_list};

fn main() -> ExitCode {
    batch::run(gen_ex_list::run)
}
