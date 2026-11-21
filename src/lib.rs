//! Rust port of LBBS, the LeafOK telnet/SSH BBS client server.
//!
//! Layout follows the original `src/` modules where the mapping is one to one, and
//! replaces the process-global state (`BBS_*`, shared memory segments, `UTF8_fixed_width`)
//! with an explicit [`session::Session`] plus [`state::AppState`] shared by tasks.

pub mod bbs;
pub mod bbsnet;
pub mod batch;
pub mod board;
pub mod browse;
pub mod bwf;
pub mod cache;
pub mod config;
pub mod daemon;
pub mod data_file_mgr;
pub mod db;
pub mod editor;
pub mod favorlist;
pub mod favorlog;
pub mod gen_ex_list;
pub mod gen_section_menu;
pub mod gen_top;
pub mod ipmask;
pub mod loader;
pub mod lml;
pub mod log;
pub mod login;
pub mod menu;
pub mod net;
pub mod paths;
pub mod post;
pub mod privilege;
pub mod roster;
pub mod screen;
pub mod section_list;
pub mod session;
pub mod ssh;
pub mod state;
pub mod term;
pub mod text;
pub mod userinfo;
pub mod userlist;
pub mod viewlog;
