/* SPDX-License-Identifier: GPL-3.0-or-later */
//! `gen_section_menu` — `php utils/bin/gen_section_menu.php` as a binary: `conf/menu.conf` plus
//! the board and digest menus read from the database, as `var/menu_merged.conf`.
use std::process::ExitCode;

use lbbs::{batch, gen_section_menu};

fn main() -> ExitCode {
    batch::run(gen_section_menu::run)
}
